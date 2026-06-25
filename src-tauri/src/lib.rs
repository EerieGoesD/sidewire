use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Query, State as AxumState},
    http::{header::CONTENT_DISPOSITION, HeaderMap, HeaderValue, Method, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    net::{TcpListener, UdpSocket},
    path::PathBuf,
    sync::{atomic::{AtomicBool, Ordering}, Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::Manager;
use tokio::fs;
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};

const CLEANUP_INTERVAL_SECS: u64 = 300;
const FILE_RETENTION_SECS: u64 = 3600;
const NONCE_SIZE: usize = 12;
const KEY_SIZE: usize = 32;
const UDP_DISCOVERY_PORT: u16 = 42069;
const ROOM_CODE_LEN: usize = 6;
// LAN HTTP server uses a known port range so other devices can connect by IP
// without relying on UDP discovery.
const LAN_PORT_START: u16 = 8765;
const LAN_PORT_END: u16 = 8784;

// ── Structs ──

#[derive(Clone)]
struct LinkRuntime {
    state: Arc<Mutex<LinkState>>,
    port: u16,
    room: Arc<Mutex<RoomState>>,
    conversations_dir: PathBuf,
    // Remote (relay) end-to-end key, derived on-device from the room password.
    // Never sent anywhere; the relay only ever sees ciphertext encrypted with this.
    remote_key: Arc<Mutex<Option<[u8; KEY_SIZE]>>>,
    // Whether this device is actively hosting a LOCAL room. Only then do we
    // broadcast the room code, so other devices don't discover us by default.
    hosting: Arc<AtomicBool>,
}

#[derive(Clone)]
struct RoomState {
    room_code: String,
    password: String,
    tokens: Vec<String>,
    device_names: HashMap<String, String>,
    creator_device_name: String,
}

struct LinkState {
    messages: Vec<LinkMessage>,
    files: HashMap<String, SharedFile>,
    incoming_dir: PathBuf,
    next_id: u64,
}

#[derive(Clone)]
struct SharedFile { path: PathBuf, file_name: String, mime: String }

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct RoomInfo {
    room_code: String,
    device_name: String,
    devices: Vec<String>,
    port: u16,
    ip: String,
    has_password: bool,
    hosting: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkMessage {
    id: String, sender: String, device_name: String, kind: String, body: String,
    file_name: Option<String>, file_size: Option<u64>, file_mime: Option<String>,
    download_url: Option<String>, created_at: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedConversation { id: String, label: String, saved_at: u64, messages: Vec<LinkMessage> }

#[derive(Deserialize)]
struct ApiQuery {
    token: Option<String>, name: Option<String>, mime: Option<String>,
    device: Option<String>, password: Option<String>,
}

// ── Room Code ──

fn generate_room_code() -> String {
    const CHARS: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
    let mut rng = rand::thread_rng();
    (0..ROOM_CODE_LEN).map(|_| { let idx = rng.gen_range(0..CHARS.len()); CHARS[idx] as char }).collect()
}

// ── Helpers ──

fn now_millis() -> u64 { SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or_default() }
fn generate_secure_token() -> String { let bytes: [u8; 32] = rand::thread_rng().gen(); hex::encode(bytes) }
fn derive_key(token: &str) -> [u8; KEY_SIZE] { let hash = Sha256::digest(token.as_bytes()); let mut key = [0u8; KEY_SIZE]; key.copy_from_slice(&hash); key }

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name.chars().map(|c| match c { '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_', c if c.is_control() => '_', c => c }).collect();
    let trimmed = cleaned.trim_matches(['.', ' ']).trim();
    if trimmed.is_empty() { "file.bin".to_string() } else { trimmed.chars().take(180).collect() }
}

fn simple_mime(path_or_name: &str) -> String {
    let lower = path_or_name.to_lowercase();
    if lower.ends_with(".pdf") { "application/pdf" } else if lower.ends_with(".png") { "image/png" }
    else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") { "image/jpeg" }
    else if lower.ends_with(".gif") { "image/gif" } else if lower.ends_with(".txt") || lower.ends_with(".md") { "text/plain; charset=utf-8" }
    else if lower.ends_with(".json") { "application/json" } else if lower.ends_with(".zip") { "application/zip" }
    else { "application/octet-stream" }.to_string()
}

fn local_device_name() -> String { std::env::var("COMPUTERNAME").or_else(|_| std::env::var("HOSTNAME")).unwrap_or_else(|_| "local-device".to_string()) }

fn primary_ipv4() -> String {
    UdpSocket::bind("0.0.0.0:0").and_then(|socket| { socket.connect("1.1.1.1:80")?; socket.local_addr() }).map(|addr| addr.ip().to_string()).unwrap_or_else(|_| "127.0.0.1".to_string())
}

// ── Room / Auth ──

fn is_authorized(query: &ApiQuery, room: &RoomState) -> bool {
    query.token.as_deref().map(|t| room.tokens.contains(&t.to_string())).unwrap_or(false)
}

fn register_device(room: &mut RoomState, device_name: String) -> String {
    let token = generate_secure_token();
    room.tokens.push(token.clone());
    room.device_names.insert(token.clone(), device_name);
    token
}

fn connected_device_names(room: &RoomState) -> Vec<String> {
    let mut names: Vec<String> = room.tokens.iter().filter_map(|t| room.device_names.get(t).cloned()).collect();
    names.insert(0, room.creator_device_name.clone()); names.sort(); names.dedup(); names
}

fn reserved_id(state: &mut LinkState, prefix: &str) -> String { state.next_id += 1; format!("{}-{}", prefix, state.next_id) }

fn add_message(state: &mut LinkState, sender: &str, device_name: &str, kind: &str, body: String,
    file_name: Option<String>, file_size: Option<u64>, file_mime: Option<String>, download_url: Option<String>) -> LinkMessage {
    let id = reserved_id(state, "msg");
    let message = LinkMessage { id, sender: sender.to_string(), device_name: device_name.to_string(), kind: kind.to_string(), body, file_name, file_size, file_mime, download_url, created_at: now_millis() };
    state.messages.push(message.clone()); message
}

// ── Encryption ──

fn get_encryption_key(room: &RoomState) -> [u8; KEY_SIZE] { derive_key(&room.room_code) }

fn encrypt_bytes(plaintext: &[u8], key: &[u8; KEY_SIZE]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("key setup: {}", e))?;
    let nonce_bytes: [u8; NONCE_SIZE] = rand::thread_rng().gen();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher.encrypt(nonce, plaintext).map_err(|e| format!("encrypt: {}", e))?;
    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes); result.extend_from_slice(&ciphertext);
    Ok(result)
}

fn decrypt_bytes(data: &[u8], key: &[u8; KEY_SIZE]) -> Result<Vec<u8>, String> {
    if data.len() < NONCE_SIZE { return Err("ciphertext too short".to_string()); }
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("key setup: {}", e))?;
    let nonce = Nonce::from_slice(&data[..NONCE_SIZE]);
    cipher.decrypt(nonce, &data[NONCE_SIZE..]).map_err(|_| "decrypt failed (wrong password?)".to_string())
}

// ── Remote (relay) end-to-end crypto ──
//
// The room password never leaves the device. We derive an AES key from it
// (PBKDF2, salt = room code) for encrypting content, and a separate join-auth
// hash so the relay can gate joining without ever learning the password or key.

const PBKDF2_ROUNDS: u32 = 210_000;

fn derive_remote_key(password: &str, room_code: &str) -> [u8; KEY_SIZE] {
    let mut key = [0u8; KEY_SIZE];
    pbkdf2::pbkdf2_hmac::<Sha256>(password.as_bytes(), room_code.as_bytes(), PBKDF2_ROUNDS, &mut key);
    key
}

fn join_auth_hash(password: &str, room_code: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(room_code.as_bytes());
    hasher.update([0u8]);
    hasher.update(password.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_key_is_deterministic_and_password_bound() {
        let a = derive_remote_key("hunter2", "SIDE-ABC123");
        let b = derive_remote_key("hunter2", "SIDE-ABC123");
        let c = derive_remote_key("wrong", "SIDE-ABC123");
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn text_roundtrip_and_wrong_key_fails() {
        let key = derive_remote_key("pw", "SIDE-XYZ789");
        let ct = encrypt_bytes("hello world".as_bytes(), &key).unwrap();
        assert_eq!(decrypt_bytes(&ct, &key).unwrap(), b"hello world");
        let bad = derive_remote_key("nope", "SIDE-XYZ789");
        assert!(decrypt_bytes(&ct, &bad).is_err());
    }

    #[test]
    fn file_framing_roundtrip() {
        let bytes = vec![0u8, 1, 2, 3, 255, 254];
        let framed = frame_file("report.pdf", "application/pdf", &bytes);
        let (name, mime, data) = unframe_file(&framed).unwrap();
        assert_eq!(name, "report.pdf");
        assert_eq!(mime, "application/pdf");
        assert_eq!(data, &bytes[..]);
    }

    #[test]
    fn auth_hash_differs_from_key_material() {
        // The relay-visible join hash must not equal the encryption key.
        let key = derive_remote_key("pw", "SIDE-CODE01");
        let auth = join_auth_hash("pw", "SIDE-CODE01");
        assert_ne!(auth, hex::encode(key));
        assert_eq!(auth, join_auth_hash("pw", "SIDE-CODE01"));
        assert_ne!(auth, join_auth_hash("pw2", "SIDE-CODE01"));
    }
}

// File payload framing: [u16 name_len][name][u16 mime_len][mime][raw bytes]
fn frame_file(name: &str, mime: &str, bytes: &[u8]) -> Vec<u8> {
    let name_b = name.as_bytes(); let mime_b = mime.as_bytes();
    let mut out = Vec::with_capacity(4 + name_b.len() + mime_b.len() + bytes.len());
    out.extend_from_slice(&(name_b.len() as u16).to_le_bytes()); out.extend_from_slice(name_b);
    out.extend_from_slice(&(mime_b.len() as u16).to_le_bytes()); out.extend_from_slice(mime_b);
    out.extend_from_slice(bytes);
    out
}

fn unframe_file(data: &[u8]) -> Result<(String, String, &[u8]), String> {
    let err = || "corrupt file payload".to_string();
    if data.len() < 2 { return Err(err()); }
    let name_len = u16::from_le_bytes([data[0], data[1]]) as usize;
    let mut p = 2; if data.len() < p + name_len + 2 { return Err(err()); }
    let name = String::from_utf8_lossy(&data[p..p + name_len]).to_string(); p += name_len;
    let mime_len = u16::from_le_bytes([data[p], data[p + 1]]) as usize; p += 2;
    if data.len() < p + mime_len { return Err(err()); }
    let mime = String::from_utf8_lossy(&data[p..p + mime_len]).to_string(); p += mime_len;
    Ok((name, mime, &data[p..]))
}

// ── Conversation persistence ──

fn save_conversation_file(conversations_dir: &PathBuf, label: &str, messages: &[LinkMessage]) -> Result<SavedConversation, String> {
    let id = format!("conv-{}", now_millis()); let saved_at = now_millis();
    let label = if label.trim().is_empty() {
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| {
            let secs = d.as_secs(); let days = secs / 86400; let hours = (secs % 86400) / 3600; let mins = (secs % 3600) / 60;
            format!("Conversation - Day {} at {:02}:{:02}", days, hours, mins)
        }).unwrap_or_else(|_| "Conversation".to_string()); now
    } else { label.to_string() };
    let conv = SavedConversation { id: id.clone(), label: label.clone(), saved_at, messages: messages.to_vec() };
    let json = serde_json::to_string_pretty(&conv).map_err(|e| format!("serialize conversation: {}", e))?;
    std::fs::create_dir_all(conversations_dir).map_err(|e| format!("create conversations dir: {}", e))?;
    std::fs::write(&conversations_dir.join(format!("{}.json", id)), &json).map_err(|e| format!("write conversation: {}", e))?;
    Ok(conv)
}

fn load_conversation_file(conversations_dir: &PathBuf, id: &str) -> Result<SavedConversation, String> {
    let json = std::fs::read_to_string(&conversations_dir.join(format!("{}.json", id))).map_err(|e| format!("read conversation: {}", e))?;
    serde_json::from_str(&json).map_err(|e| format!("parse conversation: {}", e))
}

fn list_conversation_files(conversations_dir: &PathBuf) -> Result<Vec<SavedConversation>, String> {
    if !conversations_dir.exists() { return Ok(Vec::new()); }
    let mut conversations = Vec::new();
    let entries = std::fs::read_dir(conversations_dir).map_err(|e| format!("list conversations: {}", e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read entry: {}", e))?;
        if entry.path().extension().map_or(false, |ext| ext == "json") {
            if let Ok(json) = std::fs::read_to_string(entry.path()) {
                if let Ok(conv) = serde_json::from_str::<SavedConversation>(&json) { conversations.push(conv); }
            }
        }
    }
    conversations.sort_by(|a, b| b.saved_at.cmp(&a.saved_at)); Ok(conversations)
}

fn delete_conversation_file(conversations_dir: &PathBuf, id: &str) -> Result<(), String> {
    let path = conversations_dir.join(format!("{}.json", id));
    if path.exists() { std::fs::remove_file(&path).map_err(|e| format!("delete conversation: {}", e))?; }
    Ok(())
}

// ── File cleanup ──

async fn cleanup_old_files(incoming_dir: PathBuf) {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut entries = match fs::read_dir(&incoming_dir).await { Ok(e) => e, Err(_) => return };
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(metadata) = entry.metadata().await {
            if metadata.is_file() {
                if let Ok(modified) = metadata.created().or_else(|_| metadata.modified()) {
                    if let Ok(age) = modified.duration_since(SystemTime::UNIX_EPOCH).map(|d| d.as_secs()) {
                        if now.saturating_sub(age) > FILE_RETENTION_SECS { let _ = fs::remove_file(entry.path()).await; }
                    }
                }
            }
        }
    }
}

// ── UDP Discovery ──

fn start_udp_broadcast(room_code: &str, port: u16, hosting: Arc<AtomicBool>) {
    let code = room_code.to_string();
    std::thread::spawn(move || {
        if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
            socket.set_broadcast(true).ok();
            let msg = format!("SIDEWIRE_ROOM:{}:{}", code, port);
            // Only announce the room while actively hosting a local room.
            loop {
                if hosting.load(Ordering::Relaxed) { socket.send_to(msg.as_bytes(), format!("255.255.255.255:{}", UDP_DISCOVERY_PORT)).ok(); }
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    });
}

// ── API Handlers ──

async fn api_root() -> axum::response::Html<&'static str> { axum::response::Html(MOBILE_CLIENT_HTML) }

async fn api_room_info(AxumState(runtime): AxumState<LinkRuntime>) -> Json<RoomInfo> {
    let room = runtime.room.lock().unwrap();
    Json(RoomInfo { room_code: room.room_code.clone(), device_name: room.creator_device_name.clone(), devices: connected_device_names(&room), port: runtime.port, ip: primary_ipv4(), has_password: !room.password.is_empty(), hosting: runtime.hosting.load(Ordering::Relaxed) })
}

async fn api_join_room(AxumState(runtime): AxumState<LinkRuntime>, Query(query): Query<ApiQuery>) -> Response {
    let password = query.password.as_deref().unwrap_or("");
    { let room = runtime.room.lock().unwrap(); if !room.password.is_empty() && room.password != password { return (StatusCode::UNAUTHORIZED, "wrong password").into_response(); } }
    let device_name = query.device.as_deref().unwrap_or("unknown").to_string();
    let mut room = runtime.room.lock().unwrap();
    let token = register_device(&mut room, device_name);
    let key = get_encryption_key(&room); let key_hex = hex::encode(key);
    let msg = add_message(&mut runtime.state.lock().unwrap(), "system", "system", "system", format!("{} joined the room", query.device.as_deref().unwrap_or("unknown")), None, None, None, None);
    Json(serde_json::json!({ "token": token, "keyHex": key_hex, "message": msg })).into_response()
}

async fn api_messages(AxumState(runtime): AxumState<LinkRuntime>, Query(query): Query<ApiQuery>) -> Response {
    let room = runtime.room.lock().unwrap();
    if !is_authorized(&query, &room) { return (StatusCode::UNAUTHORIZED, "invalid token").into_response(); }
    drop(room);
    match runtime.state.lock() { Ok(state) => Json(state.messages.clone()).into_response(), Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response() }
}

async fn api_send_message(AxumState(runtime): AxumState<LinkRuntime>, Query(query): Query<ApiQuery>, body: String) -> Response {
    let room = runtime.room.lock().unwrap();
    if !is_authorized(&query, &room) { return (StatusCode::UNAUTHORIZED, "invalid token").into_response(); }
    let device_name = query.device.as_deref().unwrap_or("unknown").to_string(); drop(room);
    let text = body.trim().to_string();
    if text.is_empty() { return (StatusCode::BAD_REQUEST, "empty message").into_response(); }
    match runtime.state.lock() { Ok(mut state) => { let message = add_message(&mut state, "user", &device_name, "text", text, None, None, None, None); Json(message).into_response() }, Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response() }
}

async fn api_upload_file(AxumState(runtime): AxumState<LinkRuntime>, Query(query): Query<ApiQuery>, body: Bytes) -> Response {
    let auth_ok = runtime.room.lock().map(|r| is_authorized(&query, &r)).unwrap_or(false);
    if !auth_ok { return (StatusCode::UNAUTHORIZED, "invalid token").into_response(); }
    let device_name = query.device.as_deref().unwrap_or("unknown").to_string();
    let incoming_name = query.name.as_deref().unwrap_or("upload.bin"); let file_name = sanitize_file_name(incoming_name);
    let mime = query.mime.filter(|v| !v.trim().is_empty()).unwrap_or_else(|| simple_mime(&file_name));
    let file_size = body.len() as u64;
    let (file_id, target_path) = match runtime.state.lock() { Ok(mut state) => { let file_id = reserved_id(&mut state, "file"); let target_path = state.incoming_dir.join(format!("{}-{}", file_id, file_name)); (file_id, target_path) }, Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response() };
    if let Some(parent) = target_path.parent() { if let Err(error) = fs::create_dir_all(parent).await { return (StatusCode::INTERNAL_SERVER_ERROR, format!("cannot create incoming directory: {}", error)).into_response(); } }
    let key = runtime.room.lock().map(|r| get_encryption_key(&r)).unwrap_or([0u8; 32]);
    let encrypted = match encrypt_bytes(&body, &key) { Ok(e) => e, Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response() };
    if let Err(error) = fs::write(&target_path, &encrypted).await { return (StatusCode::INTERNAL_SERVER_ERROR, format!("cannot save file: {}", error)).into_response(); }
    match runtime.state.lock() { Ok(mut state) => { state.files.insert(file_id.clone(), SharedFile { path: target_path, file_name: file_name.clone(), mime: mime.clone() }); let message = add_message(&mut state, "user", &device_name, "file", format!("sent {}", file_name), Some(file_name), Some(file_size), Some(mime), Some(format!("/api/download/{}", file_id))); Json(message).into_response() }, Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response() }
}

async fn api_download_file(AxumState(runtime): AxumState<LinkRuntime>, Path(file_id): Path<String>, Query(query): Query<ApiQuery>) -> Response {
    let auth_ok = runtime.room.lock().map(|r| is_authorized(&query, &r)).unwrap_or(false);
    if !auth_ok { return (StatusCode::UNAUTHORIZED, "invalid token").into_response(); }
    let key_hex = runtime.room.lock().map(|r| hex::encode(get_encryption_key(&r))).unwrap_or_default();
    let shared_file = match runtime.state.lock() { Ok(state) => state.files.get(&file_id).cloned(), Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response() };
    let Some(shared_file) = shared_file else { return (StatusCode::NOT_FOUND, "file not found").into_response() };
    let file = match fs::File::open(&shared_file.path).await { Ok(f) => f, Err(error) => { return (StatusCode::INTERNAL_SERVER_ERROR, format!("cannot open file: {}", error)).into_response() } };
    let mut headers = HeaderMap::new();
    headers.insert(axum::http::header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    headers.insert(CONTENT_DISPOSITION, HeaderValue::from_str(&format!("attachment; filename=\"{}.encrypted\"", sanitize_file_name(&shared_file.file_name))).unwrap_or_else(|_| HeaderValue::from_static("attachment")));
    headers.insert("X-Encryption-Key-Hex", HeaderValue::from_str(&key_hex).unwrap_or_else(|_| HeaderValue::from_static("")));
    headers.insert("X-Original-Name", HeaderValue::from_str(&shared_file.file_name).unwrap_or_else(|_| HeaderValue::from_static("file")));
    headers.insert("X-Original-Mime", HeaderValue::from_str(&shared_file.mime).unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")));
    (headers, Body::from_stream(ReaderStream::new(file))).into_response()
}

// ── Tauri Commands ──

#[tauri::command]
fn get_room_info(runtime: tauri::State<'_, LinkRuntime>) -> RoomInfo {
    let room = runtime.room.lock().unwrap();
    RoomInfo { room_code: room.room_code.clone(), device_name: room.creator_device_name.clone(), devices: connected_device_names(&room), port: runtime.port, ip: primary_ipv4(), has_password: !room.password.is_empty(), hosting: runtime.hosting.load(Ordering::Relaxed) }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteKeyResult { auth_hash: String }

#[tauri::command]
fn remote_set_key(runtime: tauri::State<'_, LinkRuntime>, password: String, room_code: String) -> Result<RemoteKeyResult, String> {
    if password.is_empty() { return Err("password is required".to_string()); }
    let key = derive_remote_key(&password, &room_code);
    *runtime.remote_key.lock().map_err(|_| "state unavailable".to_string())? = Some(key);
    Ok(RemoteKeyResult { auth_hash: join_auth_hash(&password, &room_code) })
}

#[tauri::command]
fn remote_clear_key(runtime: tauri::State<'_, LinkRuntime>) -> Result<(), String> {
    *runtime.remote_key.lock().map_err(|_| "state unavailable".to_string())? = None;
    Ok(())
}

// Begin/stop announcing this device's local room over the network. Called when
// the user opens or leaves a local room, so we never broadcast unprompted.
#[tauri::command]
fn start_local_hosting(runtime: tauri::State<'_, LinkRuntime>) { runtime.hosting.store(true, Ordering::Relaxed); }
#[tauri::command]
fn stop_local_hosting(runtime: tauri::State<'_, LinkRuntime>) { runtime.hosting.store(false, Ordering::Relaxed); }

fn current_remote_key(runtime: &LinkRuntime) -> Result<[u8; KEY_SIZE], String> {
    runtime.remote_key.lock().map_err(|_| "state unavailable".to_string())?
        .ok_or_else(|| "no remote key set".to_string())
}

#[tauri::command]
fn remote_encrypt_text(runtime: tauri::State<'_, LinkRuntime>, text: String) -> Result<String, String> {
    let key = current_remote_key(&runtime)?;
    Ok(BASE64.encode(encrypt_bytes(text.as_bytes(), &key)?))
}

#[tauri::command]
fn remote_decrypt_text(runtime: tauri::State<'_, LinkRuntime>, payload: String) -> Result<String, String> {
    let key = current_remote_key(&runtime)?;
    let data = BASE64.decode(payload.as_bytes()).map_err(|_| "bad payload".to_string())?;
    let plain = decrypt_bytes(&data, &key)?;
    String::from_utf8(plain).map_err(|_| "invalid utf8".to_string())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteFileResult { payload: String, file_size: u64 }

#[tauri::command]
fn remote_encrypt_file(runtime: tauri::State<'_, LinkRuntime>, path: String) -> Result<RemoteFileResult, String> {
    let key = current_remote_key(&runtime)?;
    let path = PathBuf::from(path);
    let metadata = std::fs::metadata(&path).map_err(|e| format!("cannot read file: {}", e))?;
    if !metadata.is_file() { return Err("selected path is not a file".to_string()); }
    let file_name = sanitize_file_name(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file.bin".to_string()));
    let mime = simple_mime(&file_name);
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read file: {}", e))?;
    let file_size = bytes.len() as u64;
    let framed = frame_file(&file_name, &mime, &bytes);
    let payload = BASE64.encode(encrypt_bytes(&framed, &key)?);
    Ok(RemoteFileResult { payload, file_size })
}

// Same as remote_encrypt_file but from bytes read in the webview - works on
// Android where the file picker returns a content:// URI that std::fs cannot read.
#[tauri::command]
fn remote_encrypt_bytes(runtime: tauri::State<'_, LinkRuntime>, file_name: String, mime: String, data_base64: String) -> Result<RemoteFileResult, String> {
    let key = current_remote_key(&runtime)?;
    let bytes = BASE64.decode(data_base64.as_bytes()).map_err(|_| "bad data".to_string())?;
    let file_name = sanitize_file_name(&file_name);
    let mime = if mime.trim().is_empty() { simple_mime(&file_name) } else { mime };
    let file_size = bytes.len() as u64;
    let framed = frame_file(&file_name, &mime, &bytes);
    let payload = BASE64.encode(encrypt_bytes(&framed, &key)?);
    Ok(RemoteFileResult { payload, file_size })
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RemoteFileMeta { file_name: String, file_mime: String, file_size: u64 }

#[tauri::command]
fn remote_file_meta(runtime: tauri::State<'_, LinkRuntime>, payload: String) -> Result<RemoteFileMeta, String> {
    let key = current_remote_key(&runtime)?;
    let data = BASE64.decode(payload.as_bytes()).map_err(|_| "bad payload".to_string())?;
    let plain = decrypt_bytes(&data, &key)?;
    let (name, mime, bytes) = unframe_file(&plain)?;
    Ok(RemoteFileMeta { file_name: name, file_mime: mime, file_size: bytes.len() as u64 })
}

#[tauri::command]
fn remote_decrypt_file(runtime: tauri::State<'_, LinkRuntime>, payload: String, save_path: String) -> Result<(), String> {
    let key = current_remote_key(&runtime)?;
    let data = BASE64.decode(payload.as_bytes()).map_err(|_| "bad payload".to_string())?;
    let plain = decrypt_bytes(&data, &key)?;
    let (_name, _mime, bytes) = unframe_file(&plain)?;
    std::fs::write(&save_path, bytes).map_err(|e| format!("cannot save file: {}", e))?;
    Ok(())
}

// ── Local-client (LAN) file helpers ──

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct LocalFileResult { file_name: String, file_mime: String, base64: String }

#[tauri::command]
fn read_file_base64(path: String) -> Result<LocalFileResult, String> {
    let path = PathBuf::from(path);
    let metadata = std::fs::metadata(&path).map_err(|e| format!("cannot read file: {}", e))?;
    if !metadata.is_file() { return Err("selected path is not a file".to_string()); }
    let file_name = sanitize_file_name(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file.bin".to_string()));
    let mime = simple_mime(&file_name);
    let bytes = std::fs::read(&path).map_err(|e| format!("cannot read file: {}", e))?;
    Ok(LocalFileResult { file_name, file_mime: mime, base64: BASE64.encode(bytes) })
}

// Decrypt a file downloaded from a LAN host (nonce||ciphertext, key from the
// X-Encryption-Key-Hex response header) and write the plaintext to save_path.
#[tauri::command]
fn save_local_download(cipher_base64: String, key_hex: String, save_path: String) -> Result<(), String> {
    let cipher = BASE64.decode(cipher_base64.as_bytes()).map_err(|_| "bad data".to_string())?;
    let key_vec = hex::decode(key_hex).map_err(|_| "bad key".to_string())?;
    if key_vec.len() != KEY_SIZE { return Err("bad key length".to_string()); }
    let mut key = [0u8; KEY_SIZE]; key.copy_from_slice(&key_vec);
    let plain = decrypt_bytes(&cipher, &key)?;
    std::fs::write(&save_path, plain).map_err(|e| format!("cannot save file: {}", e))?;
    Ok(())
}

// Save a file that another device uploaded to THIS host. Uploads are stored
// encrypted on disk (nonce||ciphertext, room key), so decrypt with the room key
// and write the plaintext to save_path. file_id is the trailing segment of the
// message's download_url (/api/download/{file_id}).
#[tauri::command]
fn save_host_file(runtime: tauri::State<'_, LinkRuntime>, file_id: String, save_path: String) -> Result<(), String> {
    let key = runtime.room.lock().map(|r| get_encryption_key(&r)).map_err(|_| "room unavailable".to_string())?;
    let shared = runtime.state.lock().map_err(|_| "state unavailable".to_string())?.files.get(&file_id).cloned();
    let shared = shared.ok_or_else(|| "file not found".to_string())?;
    let cipher = std::fs::read(&shared.path).map_err(|e| format!("cannot read file: {}", e))?;
    let plain = decrypt_bytes(&cipher, &key)?;
    std::fs::write(&save_path, plain).map_err(|e| format!("cannot save file: {}", e))?;
    Ok(())
}

// ── Decrypt-to-base64: return the plaintext file bytes to the webview (for
// previewing the image inline and for writing via the fs plugin, which handles
// Android content URIs that std::fs cannot). One command per transport. ──

#[tauri::command]
fn remote_decrypt_base64(runtime: tauri::State<'_, LinkRuntime>, payload: String) -> Result<LocalFileResult, String> {
    let key = current_remote_key(&runtime)?;
    let data = BASE64.decode(payload.as_bytes()).map_err(|_| "bad payload".to_string())?;
    let plain = decrypt_bytes(&data, &key)?;
    let (name, mime, bytes) = unframe_file(&plain)?;
    Ok(LocalFileResult { file_name: name, file_mime: mime, base64: BASE64.encode(bytes) })
}

#[tauri::command]
fn host_file_base64(runtime: tauri::State<'_, LinkRuntime>, file_id: String) -> Result<LocalFileResult, String> {
    let key = runtime.room.lock().map(|r| get_encryption_key(&r)).map_err(|_| "room unavailable".to_string())?;
    let shared = runtime.state.lock().map_err(|_| "state unavailable".to_string())?.files.get(&file_id).cloned();
    let shared = shared.ok_or_else(|| "file not found".to_string())?;
    let cipher = std::fs::read(&shared.path).map_err(|e| format!("cannot read file: {}", e))?;
    let plain = decrypt_bytes(&cipher, &key)?;
    Ok(LocalFileResult { file_name: shared.file_name.clone(), file_mime: shared.mime.clone(), base64: BASE64.encode(plain) })
}

#[tauri::command]
fn lan_decrypt_base64(cipher_base64: String, key_hex: String, file_name: String) -> Result<LocalFileResult, String> {
    let cipher = BASE64.decode(cipher_base64.as_bytes()).map_err(|_| "bad data".to_string())?;
    let key_vec = hex::decode(key_hex).map_err(|_| "bad key".to_string())?;
    if key_vec.len() != KEY_SIZE { return Err("bad key length".to_string()); }
    let mut key = [0u8; KEY_SIZE]; key.copy_from_slice(&key_vec);
    let plain = decrypt_bytes(&cipher, &key)?;
    let name = sanitize_file_name(&file_name);
    let mime = simple_mime(&name);
    Ok(LocalFileResult { file_name: name, file_mime: mime, base64: BASE64.encode(plain) })
}

#[tauri::command]
async fn discover_rooms(runtime: tauri::State<'_, LinkRuntime>, timeout_secs: u64) -> Result<Vec<serde_json::Value>, String> {
    let own_code = runtime.room.lock().map(|r| r.room_code.clone()).unwrap_or_default();
    // Run the blocking UDP listen off the main thread so the UI never freezes.
    tauri::async_runtime::spawn_blocking(move || discover_rooms_blocking(timeout_secs, own_code))
        .await
        .map_err(|e| format!("scan failed: {}", e))
}

fn discover_rooms_blocking(timeout_secs: u64, own_code: String) -> Vec<serde_json::Value> {
    let mut results = Vec::new();
    // Must bind the port the broadcaster sends to, or no packets are ever received.
    if let Ok(socket) = UdpSocket::bind(("0.0.0.0", UDP_DISCOVERY_PORT)) {
        socket.set_read_timeout(Some(Duration::from_secs(timeout_secs))).ok(); socket.set_broadcast(true).ok();
        let start = std::time::Instant::now();
        while start.elapsed().as_secs() < timeout_secs {
            let mut buf = [0u8; 256];
            match socket.recv_from(&mut buf) {
                Ok((len, addr)) => {
                    let msg = String::from_utf8_lossy(&buf[..len]);
                    if msg.starts_with("SIDEWIRE_ROOM:") {
                        let parts: Vec<&str> = msg.split(':').collect();
                        // Skip our own broadcast - you don't join your own room.
                        if parts.len() >= 3 && parts[1] != own_code {
                            results.push(serde_json::json!({ "roomCode": parts[1], "ip": addr.ip().to_string(), "port": parts[2].parse::<u16>().unwrap_or(0) }));
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }
    results.sort_by(|a, b| a["roomCode"].as_str().cmp(&b["roomCode"].as_str()));
    results.dedup_by(|a, b| a["roomCode"] == b["roomCode"]);
    results
}

#[tauri::command]
fn list_messages(runtime: tauri::State<'_, LinkRuntime>) -> Result<Vec<LinkMessage>, String> {
    runtime.state.lock().map(|state| state.messages.clone()).map_err(|_| "state unavailable".to_string())
}

#[tauri::command]
fn send_text(runtime: tauri::State<'_, LinkRuntime>, text: String, device_name: String) -> Result<LinkMessage, String> {
    let text = text.trim().to_string();
    if text.is_empty() { return Err("message is empty".to_string()); }
    let mut state = runtime.state.lock().map_err(|_| "state unavailable".to_string())?;
    Ok(add_message(&mut state, "user", &device_name, "text", text, None, None, None, None))
}

#[tauri::command]
fn share_file(runtime: tauri::State<'_, LinkRuntime>, path: String, device_name: String) -> Result<LinkMessage, String> {
    let path = PathBuf::from(path);
    let metadata = std::fs::metadata(&path).map_err(|e| format!("cannot read file: {}", e))?;
    if !metadata.is_file() { return Err("selected path is not a file".to_string()); }
    let file_name = sanitize_file_name(&path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_else(|| "file.bin".to_string()));
    let file_size = metadata.len(); let mime = simple_mime(&file_name);
    let plaintext = std::fs::read(&path).map_err(|e| format!("cannot read file: {}", e))?;
    let room = runtime.room.lock().unwrap(); let key = get_encryption_key(&room); let encrypted = encrypt_bytes(&plaintext, &key)?; drop(room);
    let mut state = runtime.state.lock().map_err(|_| "state unavailable".to_string())?;
    let file_id = reserved_id(&mut state, "file");
    let encrypted_path = state.incoming_dir.join(format!("{}-{}", file_id, file_name));
    std::fs::write(&encrypted_path, &encrypted).map_err(|e| format!("cannot write encrypted file: {}", e))?;
    state.files.insert(file_id.clone(), SharedFile { path: encrypted_path, file_name: file_name.clone(), mime: mime.clone() });
    Ok(add_message(&mut state, "user", &device_name, "file", format!("shared {}", file_name), Some(file_name), Some(file_size), Some(mime), Some(format!("/api/download/{}", file_id))))
}

// Share a file from bytes read in the webview (works on Android, unlike share_file
// which needs a real filesystem path).
#[tauri::command]
fn share_file_bytes(runtime: tauri::State<'_, LinkRuntime>, file_name: String, mime: String, data_base64: String, device_name: String) -> Result<LinkMessage, String> {
    let bytes = BASE64.decode(data_base64.as_bytes()).map_err(|_| "bad data".to_string())?;
    let file_name = sanitize_file_name(&file_name);
    let mime = if mime.trim().is_empty() { simple_mime(&file_name) } else { mime };
    let file_size = bytes.len() as u64;
    let room = runtime.room.lock().unwrap(); let key = get_encryption_key(&room); let encrypted = encrypt_bytes(&bytes, &key)?; drop(room);
    let mut state = runtime.state.lock().map_err(|_| "state unavailable".to_string())?;
    let file_id = reserved_id(&mut state, "file");
    let encrypted_path = state.incoming_dir.join(format!("{}-{}", file_id, file_name));
    std::fs::write(&encrypted_path, &encrypted).map_err(|e| format!("cannot write encrypted file: {}", e))?;
    state.files.insert(file_id.clone(), SharedFile { path: encrypted_path, file_name: file_name.clone(), mime: mime.clone() });
    Ok(add_message(&mut state, "user", &device_name, "file", format!("shared {}", file_name), Some(file_name), Some(file_size), Some(mime), Some(format!("/api/download/{}", file_id))))
}

#[tauri::command]
fn clear_messages(runtime: tauri::State<'_, LinkRuntime>) -> Result<Vec<LinkMessage>, String> {
    runtime.state.lock().map(|mut state| { state.messages.clear(); vec![add_message(&mut state, "system", "system", "system", "Conversation cleared.".to_string(), None, None, None, None)] }).map_err(|_| "state unavailable".to_string())
}

#[tauri::command]
fn save_conversation(runtime: tauri::State<'_, LinkRuntime>, label: String) -> Result<SavedConversation, String> {
    let messages = runtime.state.lock().map(|state| state.messages.clone()).map_err(|_| "state unavailable".to_string())?;
    save_conversation_file(&runtime.conversations_dir, &label, &messages)
}

#[tauri::command]
fn load_conversation(runtime: tauri::State<'_, LinkRuntime>, id: String) -> Result<SavedConversation, String> { load_conversation_file(&runtime.conversations_dir, &id) }
#[tauri::command]
fn list_saved_conversations(runtime: tauri::State<'_, LinkRuntime>) -> Result<Vec<SavedConversation>, String> { list_conversation_files(&runtime.conversations_dir) }
#[tauri::command]
fn delete_conversation(runtime: tauri::State<'_, LinkRuntime>, id: String) -> Result<(), String> { delete_conversation_file(&runtime.conversations_dir, &id) }

#[tauri::command]
fn cleanup_old_files_now(runtime: tauri::State<'_, LinkRuntime>) -> Result<String, String> {
    let incoming_dir = runtime.state.lock().map(|state| state.incoming_dir.clone()).map_err(|_| "state unavailable".to_string())?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut deleted = 0u64;
    if let Ok(entries) = std::fs::read_dir(&incoming_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() { if metadata.is_file() {
                if let Ok(modified) = metadata.created().or_else(|_| metadata.modified()) {
                    if let Ok(age) = modified.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()) {
                        if now.saturating_sub(age) > FILE_RETENTION_SECS { let _ = std::fs::remove_file(entry.path()); deleted += 1; }
                    }
                }
            }}
        }
    }
    Ok(format!("Cleaned up {} old file(s)", deleted))
}

// ── App lifecycle ──

fn start_link_server(device_name: String) -> Result<LinkRuntime, String> {
    let room_code = generate_room_code();
    let incoming_dir = std::env::temp_dir().join("sidewire").join("incoming");
    std::fs::create_dir_all(&incoming_dir).map_err(|e| format!("cannot create incoming dir: {}", e))?;
    let conversations_dir = dirs_next::data_dir().unwrap_or_else(|| PathBuf::from(std::env::temp_dir().join("sidewire"))).join("SideWire").join("conversations");
    std::fs::create_dir_all(&conversations_dir).map_err(|e| format!("cannot create conversations dir: {}", e))?;
    // Prefer a known port so devices can join by IP; fall back to a random port.
    let listener = (LAN_PORT_START..=LAN_PORT_END).find_map(|p| TcpListener::bind(("0.0.0.0", p)).ok())
        .map_or_else(|| TcpListener::bind("0.0.0.0:0"), Ok)
        .map_err(|e| format!("cannot bind: {}", e))?;
    let port = listener.local_addr().map_err(|e| format!("cannot read port: {}", e))?.port();
    listener.set_nonblocking(true).map_err(|e| format!("cannot set nonblocking: {}", e))?;
    let state = Arc::new(Mutex::new(LinkState { messages: Vec::new(), files: HashMap::new(), incoming_dir: incoming_dir.clone(), next_id: 0 }));
    let room = Arc::new(Mutex::new(RoomState { room_code: room_code.clone(), password: String::new(), tokens: Vec::new(), device_names: HashMap::new(), creator_device_name: device_name.clone() }));
    { let mut s = state.lock().map_err(|_| "state unavailable".to_string())?; add_message(&mut s, "system", "system", "system", format!("Room {} created. Share the code.", room_code), None, None, None, None); }
    let hosting = Arc::new(AtomicBool::new(false));
    let runtime = LinkRuntime { state, port, room: room.clone(), conversations_dir: conversations_dir.clone(), remote_key: Arc::new(Mutex::new(None)), hosting: hosting.clone() };
    start_udp_broadcast(&room_code, port, hosting);
    let server_runtime = runtime.clone();
    let router = Router::new()
        .route("/", get(api_root)).route("/api/room", get(api_room_info)).route("/api/join", get(api_join_room))
        .route("/api/messages", get(api_messages)).route("/api/message", post(api_send_message))
        .route("/api/upload", post(api_upload_file)).route("/api/download/{file_id}", get(api_download_file))
        .layer(CorsLayer::new().allow_origin(Any).allow_methods([Method::GET, Method::POST, Method::OPTIONS]).allow_headers(Any).expose_headers(Any))
        .layer(DefaultBodyLimit::max(512 * 1024 * 1024)).with_state(server_runtime);
    let incoming_for_cleanup = incoming_dir.clone();
    tauri::async_runtime::spawn(async move { match tokio::net::TcpListener::from_std(listener) { Ok(l) => { if let Err(e) = axum::serve(l, router).await { eprintln!("sidewire server stopped: {}", e); } } Err(e) => eprintln!("sidewire server could not start: {}", e) } });
    tauri::async_runtime::spawn(async move { loop { tokio::time::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)).await; cleanup_old_files(incoming_for_cleanup.clone()).await; } });
    Ok(runtime)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_notification::init()).plugin(tauri_plugin_opener::init()).plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_iap::init()).plugin(tauri_plugin_os::init()).plugin(tauri_plugin_fs::init())
        .setup(|app| {
            let runtime = start_link_server(local_device_name()).map_err(|e| Box::<dyn std::error::Error>::from(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
            app.manage(runtime);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![get_room_info, discover_rooms, list_messages, send_text, share_file, clear_messages, save_conversation, load_conversation, list_saved_conversations, delete_conversation, cleanup_old_files_now, remote_set_key, remote_clear_key, remote_encrypt_text, remote_decrypt_text, remote_encrypt_file, remote_encrypt_bytes, remote_file_meta, remote_decrypt_file, read_file_base64, save_local_download, save_host_file, remote_decrypt_base64, host_file_base64, lan_decrypt_base64, share_file_bytes, start_local_hosting, stop_local_hosting])
        .run(tauri::generate_context!()).expect("error while running tauri application");
}

/// ── Embedded mobile HTML ──

const MOBILE_CLIENT_HTML: &str = r##"<!doctype html>
<html lang="en"><head><meta charset="utf-8"/><meta name="viewport" content="width=device-width,initial-scale=1"/><title>SideWire</title>
<style>*{box-sizing:border-box}html,body{height:100%;margin:0}body{background:#050508;color:#f0f0f0;font-family:"Cascadia Mono",Consolas,"Courier New",monospace;overflow:hidden}
body::after{content:"";position:fixed;inset:0;pointer-events:none;background:repeating-linear-gradient(to bottom,transparent 0,transparent 3px,rgba(0,0,0,.12) 3px,rgba(0,0,0,.12) 4px);z-index:10}
.shell{min-height:100%;display:grid;grid-template-rows:auto auto 1fr auto}
header{border-bottom:1px solid rgba(30,144,255,.22);padding:14px 16px;display:flex;gap:10px;align-items:center;justify-content:space-between}
.prompt{color:rgba(30,144,255,.8);font-size:12px}.room-code{color:#1e90ff;font-size:18px;font-weight:600}
main{overflow:auto;padding:16px;display:flex;flex-direction:column;gap:10px}
.msg{border:1px solid rgba(30,144,255,.18);background:rgba(10,10,15,.78);padding:10px 12px;max-width:88%;border-radius:4px}
.user{align-self:flex-start}.system{align-self:center;color:rgba(255,255,255,.55);font-size:12px}
.meta{color:rgba(30,144,255,.65);font-size:11px;margin-bottom:4px}
.file{margin-top:8px;display:flex;align-items:center;justify-content:space-between;gap:12px}
a,button,label{color:#f0f0f0;border:1px solid rgba(30,144,255,.32);background:rgba(5,5,8,.9);border-radius:3px;padding:9px 11px;font:inherit;text-decoration:none}
button,label{cursor:pointer}
input{min-width:0;color:#f0f0f0;background:rgba(10,10,15,.9);border:1px solid rgba(30,144,255,.26);border-radius:3px;padding:10px 12px;font:inherit}
form{border-top:1px solid rgba(30,144,255,.22);padding:12px;display:grid;grid-template-columns:1fr auto auto;gap:8px}
input[type=file]{display:none}.error{color:#e84a8a;padding:8px 16px 0;font-size:12px}
.join-form{padding:16px;display:grid;gap:10px}.join-form label{color:rgba(255,255,255,.74);font-size:13px}
.join-btn{background:#1e90ff;border-color:#1e90ff;color:#050508}
.pw-input{-webkit-text-security:disc}</style></head><body>
<div class="shell" id="app"><header><div><div class="prompt">SideWire</div><strong id="roomLabel">Join a room</strong></div><div class="room-code" id="roomCode"></div></header>
<div class="error" id="error"></div>
<div id="joinView" class="join-form">
<label for="code">Room code</label><input id="code" type="text" autocomplete="off" placeholder="e.g. 7K2Q9X"/>
<label for="pw">Password (if required)</label><input id="pw" class="pw-input" type="text" autocomplete="off" placeholder="leave blank if none"/>
<div style="display:grid;grid-template-columns:1fr 1fr;gap:8px"><label for="dname">Your name</label><input id="dname" type="text" autocomplete="off" placeholder="My Phone" value="Phone"/></div>
<button class="join-btn" id="joinBtn">Join Room</button></div>
<main id="messages" style="display:none"></main>
<form id="form" style="display:none"><input id="text" type="text" autocomplete="off" placeholder="Send a message"/><label for="file">📎<input id="file" type="file" multiple/></label><button type="submit">send</button></form></div>
<script>
const c=document.getElementById("code"),pw=document.getElementById("pw"),dn=document.getElementById("dname"),jb=document.getElementById("joinBtn"),msgs=document.getElementById("messages"),f=document.getElementById("form"),err=document.getElementById("error"),tx=document.getElementById("text"),fl=document.getElementById("file"),rc=document.getElementById("roomCode"),rl=document.getElementById("roomLabel");
let token="",host="";
function bytes(n){if(!n)return"0 B";const u=["B","KB","MB","GB"];let v=n,i=0;while(v>=1024&&i<u.length-1){v/=1024;i++}return v.toFixed(v>=10||i===0?0:1)+" "+u[i]}
async function join(){const code=c.value.trim().toUpperCase(),pass=pw.value.trim(),name=dn.value.trim()||"Phone";if(!code){err.textContent="Enter a room code.";return}
const ip=prompt("Host IP (shown on their screen):");if(!ip){err.textContent="";return}
err.textContent="Searching...";
for(let port=8765;port<=8784;port++){try{const r=await fetch("http://"+ip+":"+port+"/api/room",{cache:"no-store"});if(r.ok){const i=await r.json();if(i.roomCode===code){host=ip+":"+port;break}}}catch{}}
if(!host){err.textContent="Could not find room.";return}
let joinUrl="http://"+host+"/api/join?device="+encodeURIComponent(name);
if(pass)joinUrl+="&password="+encodeURIComponent(pass);
try{const r=await fetch(joinUrl,{cache:"no-store"});if(!r.ok){const t=await r.text();if(t.includes("password")){err.textContent="Wrong password."}else{err.textContent="Join failed."}return}
const d=await r.json();token=d.token;rc.textContent=code;rl.textContent="Connected";document.getElementById("joinView").style.display="none";msgs.style.display="flex";f.style.display="grid";err.textContent="";refresh()}catch(e){err.textContent=String(e)}}
jb.addEventListener("click",join);c.addEventListener("keydown",e=>{if(e.key==="Enter")join()});
let last="";
async function refresh(){if(!token||!host)return;try{const r=await fetch("http://"+host+"/api/messages?token="+encodeURIComponent(token),{cache:"no-store"});if(!r.ok)throw new Error(await r.text());const items=await r.json(),sig=JSON.stringify(items.map(m=>m.id));if(sig===last)return;last=sig;msgs.textContent="";items.forEach(item=>{const row=document.createElement("div");row.className="msg "+(item.sender==="system"?"system":"user");const meta=document.createElement("div");meta.className="meta";meta.textContent=(item.deviceName||item.sender)+" :: "+new Date(item.createdAt).toLocaleTimeString([],{hour:"2-digit",minute:"2-digit"});const body=document.createElement("div");body.textContent=item.body;row.append(meta,body);if(item.kind==="file"){const fr=document.createElement("div");fr.className="file";const nm=document.createElement("span");nm.textContent=(item.fileName||"file")+" / "+bytes(item.fileSize||0);fr.appendChild(nm);row.appendChild(fr)}msgs.appendChild(row)});msgs.scrollTop=msgs.scrollHeight}catch{}}
f.addEventListener("submit",async e=>{e.preventDefault();const v=tx.value.trim();if(!v)return;tx.value="";try{await fetch("http://"+host+"/api/message?token="+encodeURIComponent(token)+"&device="+encodeURIComponent(dn.value||"Phone"),{method:"POST",body:v})}catch{}refresh()});
fl.addEventListener("change",async()=>{for(const f of Array.from(fl.files||[])){await fetch("http://"+host+"/api/upload?token="+encodeURIComponent(token)+"&device="+encodeURIComponent(dn.value||"Phone")+"&name="+encodeURIComponent(f.name)+"&mime="+encodeURIComponent(f.type||"application/octet-stream"),{method:"POST",body:f})}fl.value="";refresh()});
setInterval(refresh,1200);
</script></body></html>"##;