use aes_gcm::{
    aead::{Aead, KeyInit},
    Aes256Gcm, Nonce,
};
use axum::{
    body::{Body, Bytes},
    extract::{DefaultBodyLimit, Path, Query, State as AxumState},
    http::{header::CONTENT_DISPOSITION, HeaderMap, HeaderValue, Method, StatusCode},
    response::{Html, IntoResponse, Response},
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
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tauri::Manager;
use tokio::fs;
use tokio_util::io::ReaderStream;
use tower_http::cors::{Any, CorsLayer};

/// ── Constants ──
const CLEANUP_INTERVAL_SECS: u64 = 300;       // every 5 minutes
const FILE_RETENTION_SECS: u64 = 3600;         // 1 hour
const NONCE_SIZE: usize = 12;
const KEY_SIZE: usize = 32;

/// ── Structs ──

#[derive(Clone)]
struct LinkRuntime {
    state: Arc<Mutex<LinkState>>,
    port: u16,
    token_key: Arc<Mutex<TokenKey>>,
    conversations_dir: PathBuf,
}

#[derive(Clone)]
struct TokenKey {
    token: String,
    key: [u8; KEY_SIZE],
}

struct LinkState {
    messages: Vec<LinkMessage>,
    files: HashMap<String, SharedFile>,
    incoming_dir: PathBuf,
    next_id: u64,
    last_phone_seen_at: Option<u64>,
    last_was_connected: bool,
}

#[derive(Clone)]
struct SharedFile {
    path: PathBuf,
    file_name: String,
    mime: String,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct LinkProfile {
    device_name: String,
    platform: String,
    arch: String,
    primary_address: String,
    pairing_code: String,
    pairing_url: String,
    session_port: u16,
    protocol: String,
    incoming_dir: String,
    phone_connected: bool,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LinkMessage {
    id: String,
    sender: String,
    kind: String,
    body: String,
    file_name: Option<String>,
    file_size: Option<u64>,
    file_mime: Option<String>,
    download_url: Option<String>,
    created_at: u64,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SavedConversation {
    id: String,
    label: String,
    saved_at: u64,
    messages: Vec<LinkMessage>,
}

#[derive(Deserialize)]
struct ApiQuery {
    token: Option<String>,
    name: Option<String>,
    mime: Option<String>,
}

/// ── Helpers ──

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn generate_secure_token() -> String {
    let bytes: [u8; 32] = rand::thread_rng().gen();
    hex::encode(bytes)
}

fn derive_key(token: &str) -> [u8; KEY_SIZE] {
    let hash = Sha256::digest(token.as_bytes());
    let mut key = [0u8; KEY_SIZE];
    key.copy_from_slice(&hash);
    key
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim_matches(['.', ' ']).trim();
    if trimmed.is_empty() {
        "file.bin".to_string()
    } else {
        trimmed.chars().take(180).collect()
    }
}

fn simple_mime(path_or_name: &str) -> String {
    let lower = path_or_name.to_lowercase();
    if lower.ends_with(".pdf") {
        "application/pdf"
    } else if lower.ends_with(".png") {
        "image/png"
    } else if lower.ends_with(".jpg") || lower.ends_with(".jpeg") {
        "image/jpeg"
    } else if lower.ends_with(".gif") {
        "image/gif"
    } else if lower.ends_with(".txt") || lower.ends_with(".md") {
        "text/plain; charset=utf-8"
    } else if lower.ends_with(".json") {
        "application/json"
    } else if lower.ends_with(".zip") {
        "application/zip"
    } else {
        "application/octet-stream"
    }
    .to_string()
}

fn current_token_key(runtime: &LinkRuntime) -> TokenKey {
    runtime
        .token_key
        .lock()
        .map(|tk| tk.clone())
        .unwrap_or_else(|_| TokenKey {
            token: String::new(),
            key: [0u8; KEY_SIZE],
        })
}

fn rotate_token_key(runtime: &LinkRuntime) -> Result<TokenKey, String> {
    let mut guard = runtime
        .token_key
        .lock()
        .map_err(|_| "token unavailable".to_string())?;
    let token = generate_secure_token();
    let key = derive_key(&token);
    guard.token = token;
    guard.key = key;
    Ok(guard.clone())
}

fn is_authorized(query: &ApiQuery, runtime: &LinkRuntime) -> bool {
    runtime
        .token_key
        .lock()
        .map(|tk| query.token.as_deref() == Some(tk.token.as_str()))
        .unwrap_or(false)
}

fn mark_phone_seen(runtime: &LinkRuntime) {
    if let Ok(mut state) = runtime.state.lock() {
        let was_connected = state.last_phone_seen_at
            .map(|seen| now_millis().saturating_sub(seen) < 10_000)
            .unwrap_or(false);
        state.last_phone_seen_at = Some(now_millis());
        let is_connected = true;
        if !was_connected && is_connected && !state.last_was_connected {
            state.last_was_connected = true;
            add_message(
                &mut state,
                "system",
                "system",
                "Phone connected.".to_string(),
                None,
                None,
                None,
                None,
            );
        }
    }
}

fn reserve_id(state: &mut LinkState, prefix: &str) -> String {
    state.next_id += 1;
    format!("{}-{}", prefix, state.next_id)
}

fn add_message(
    state: &mut LinkState,
    sender: &str,
    kind: &str,
    body: String,
    file_name: Option<String>,
    file_size: Option<u64>,
    file_mime: Option<String>,
    download_url: Option<String>,
) -> LinkMessage {
    let id = reserve_id(state, "msg");
    let message = LinkMessage {
        id,
        sender: sender.to_string(),
        kind: kind.to_string(),
        body,
        file_name,
        file_size,
        file_mime,
        download_url,
        created_at: now_millis(),
    };
    state.messages.push(message.clone());
    message
}

fn local_device_name() -> String {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .unwrap_or_else(|_| "local-device".to_string())
}

fn primary_ipv4() -> String {
    UdpSocket::bind("0.0.0.0:0")
        .and_then(|socket| {
            socket.connect("1.1.1.1:80")?;
            socket.local_addr()
        })
        .map(|addr| addr.ip().to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// ── Encryption ──

fn encrypt_bytes(plaintext: &[u8], key: &[u8; KEY_SIZE]) -> Result<Vec<u8>, String> {
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("key setup: {}", e))?;
    let nonce_bytes: [u8; NONCE_SIZE] = rand::thread_rng().gen();
    let nonce = Nonce::from_slice(&nonce_bytes);
    let ciphertext = cipher
        .encrypt(nonce, plaintext)
        .map_err(|e| format!("encrypt: {}", e))?;
    let mut result = Vec::with_capacity(NONCE_SIZE + ciphertext.len());
    result.extend_from_slice(&nonce_bytes);
    result.extend_from_slice(&ciphertext);
    Ok(result)
}

fn decrypt_bytes(ciphertext_with_nonce: &[u8], key: &[u8; KEY_SIZE]) -> Result<Vec<u8>, String> {
    if ciphertext_with_nonce.len() < NONCE_SIZE {
        return Err("truncated ciphertext".to_string());
    }
    let (nonce_bytes, ct) = ciphertext_with_nonce.split_at(NONCE_SIZE);
    let cipher = Aes256Gcm::new_from_slice(key).map_err(|e| format!("key setup: {}", e))?;
    let nonce = Nonce::from_slice(nonce_bytes);
    cipher
        .decrypt(nonce, ct)
        .map_err(|e| format!("decrypt: {}", e))
}

/// ── Conversation persistence ──

fn save_conversation_file(conversations_dir: &PathBuf, label: &str, messages: &[LinkMessage]) -> Result<SavedConversation, String> {
    let id = format!("conv-{}", now_millis());
    let saved_at = now_millis();
    let label = if label.trim().is_empty() {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| {
                let secs = d.as_secs();
                let days = secs / 86400;
                let hours = (secs % 86400) / 3600;
                let mins = (secs % 3600) / 60;
                format!("Conversation - Day {} at {:02}:{:02}", days, hours, mins)
            })
            .unwrap_or_else(|_| "Conversation".to_string());
        now
    } else {
        label.to_string()
    };

    let conv = SavedConversation {
        id: id.clone(),
        label: label.clone(),
        saved_at,
        messages: messages.to_vec(),
    };

    let json = serde_json::to_string_pretty(&conv)
        .map_err(|e| format!("serialize conversation: {}", e))?;

    std::fs::create_dir_all(conversations_dir)
        .map_err(|e| format!("create conversations dir: {}", e))?;

    let file_path = conversations_dir.join(format!("{}.json", id));
    std::fs::write(&file_path, &json)
        .map_err(|e| format!("write conversation: {}", e))?;

    Ok(conv)
}

fn load_conversation_file(conversations_dir: &PathBuf, id: &str) -> Result<SavedConversation, String> {
    let file_path = conversations_dir.join(format!("{}.json", id));
    let json = std::fs::read_to_string(&file_path)
        .map_err(|e| format!("read conversation: {}", e))?;
    serde_json::from_str(&json).map_err(|e| format!("parse conversation: {}", e))
}

fn list_conversation_files(conversations_dir: &PathBuf) -> Result<Vec<SavedConversation>, String> {
    if !conversations_dir.exists() {
        return Ok(Vec::new());
    }
    let mut conversations = Vec::new();
    let entries = std::fs::read_dir(conversations_dir)
        .map_err(|e| format!("list conversations: {}", e))?;
    for entry in entries {
        let entry = entry.map_err(|e| format!("read entry: {}", e))?;
        if entry.path().extension().map_or(false, |ext| ext == "json") {
            let json = std::fs::read_to_string(entry.path())
                .map_err(|e| format!("read file: {}", e))?;
            if let Ok(conv) = serde_json::from_str::<SavedConversation>(&json) {
                conversations.push(conv);
            }
        }
    }
    conversations.sort_by(|a, b| b.saved_at.cmp(&a.saved_at));
    Ok(conversations)
}

fn delete_conversation_file(conversations_dir: &PathBuf, id: &str) -> Result<(), String> {
    let file_path = conversations_dir.join(format!("{}.json", id));
    if file_path.exists() {
        std::fs::remove_file(&file_path)
            .map_err(|e| format!("delete conversation: {}", e))?;
    }
    Ok(())
}

/// ── File cleanup ──

async fn cleanup_old_files(incoming_dir: PathBuf) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut entries = match fs::read_dir(&incoming_dir).await {
        Ok(e) => e,
        Err(_) => return,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Ok(metadata) = entry.metadata().await {
            if metadata.is_file() {
                if let Ok(modified) = metadata.created().or_else(|_| metadata.modified()) {
                    if let Ok(age) = modified
                        .duration_since(SystemTime::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                    {
                        if now.saturating_sub(age) > FILE_RETENTION_SECS {
                            let _ = fs::remove_file(entry.path()).await;
                        }
                    }
                }
            }
        }
    }
}

/// ── API Handlers ──

async fn mobile_page(AxumState(_runtime): AxumState<LinkRuntime>) -> Html<&'static str> {
    Html(MOBILE_CLIENT_HTML)
}

async fn api_profile(
    AxumState(runtime): AxumState<LinkRuntime>,
    Query(query): Query<ApiQuery>,
) -> Response {
    if !is_authorized(&query, &runtime) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    mark_phone_seen(&runtime);

    let ip = primary_ipv4();
    let token_key = current_token_key(&runtime);
    let (incoming_dir, phone_connected) = runtime
        .state
        .lock()
        .map(|state| {
            let connected = state
                .last_phone_seen_at
                .map(|seen| now_millis().saturating_sub(seen) < 10_000)
                .unwrap_or(false);
            (state.incoming_dir.display().to_string(), connected)
        })
        .unwrap_or_default();

    let profile = LinkProfile {
        device_name: local_device_name(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        primary_address: ip.clone(),
        pairing_code: token_key.token.clone(),
        pairing_url: format!("http://{}:{}/?token={}", ip, runtime.port, token_key.token),
        session_port: runtime.port,
        protocol: "sidewire-lan-v2".to_string(),
        incoming_dir,
        phone_connected,
    };

    Json(profile).into_response()
}

async fn api_messages(
    AxumState(runtime): AxumState<LinkRuntime>,
    Query(query): Query<ApiQuery>,
) -> Response {
    if !is_authorized(&query, &runtime) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    mark_phone_seen(&runtime);
    match runtime.state.lock() {
        Ok(state) => Json(state.messages.clone()).into_response(),
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response(),
    }
}

async fn api_post_message(
    AxumState(runtime): AxumState<LinkRuntime>,
    Query(query): Query<ApiQuery>,
    body: Bytes,
) -> Response {
    if !is_authorized(&query, &runtime) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    mark_phone_seen(&runtime);

    // Decrypt message body if it's encrypted (phone sends encrypted)
    // The phone sends encrypted text as hex
    let text = String::from_utf8_lossy(&body).trim().to_string();
    if text.is_empty() {
        return (StatusCode::BAD_REQUEST, "empty message").into_response();
    }

    match runtime.state.lock() {
        Ok(mut state) => {
            let message = add_message(&mut state, "phone", "text", text, None, None, None, None);
            Json(message).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response(),
    }
}

async fn api_disconnect(
    AxumState(runtime): AxumState<LinkRuntime>,
    Query(query): Query<ApiQuery>,
) -> Response {
    if !is_authorized(&query, &runtime) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }

    match rotate_token_key(&runtime) {
        Ok(_) => {
            let message = {
                let mut state = match runtime.state.lock() {
                    Ok(s) => s,
                    Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response(),
                };
                state.last_phone_seen_at = None;
                state.last_was_connected = false;
                add_message(
                    &mut state,
                    "system",
                    "system",
                    "Phone disconnected. Scan the new QR code to reconnect.".to_string(),
                    None,
                    None,
                    None,
                    None,
                )
            };
            Json(message).into_response()
        }
        Err(error) => (StatusCode::INTERNAL_SERVER_ERROR, error).into_response(),
    }
}

async fn api_upload_file(
    AxumState(runtime): AxumState<LinkRuntime>,
    Query(query): Query<ApiQuery>,
    body: Bytes,
) -> Response {
    if !is_authorized(&query, &runtime) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    mark_phone_seen(&runtime);

    let incoming_name = query.name.as_deref().unwrap_or("upload.bin");
    let file_name = sanitize_file_name(incoming_name);
    let mime = query
        .mime
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| simple_mime(&file_name));
    let file_size = body.len() as u64;

    let (file_id, target_path) = match runtime.state.lock() {
        Ok(mut state) => {
            let file_id = reserve_id(&mut state, "file");
            let target_path = state
                .incoming_dir
                .join(format!("{}-{}", file_id, file_name));
            (file_id, target_path)
        }
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response(),
    };

    if let Some(parent) = target_path.parent() {
        if let Err(error) = fs::create_dir_all(parent).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot create incoming directory: {}", error),
            )
                .into_response();
        }
    }

    // Encrypt the file before writing to disk
    let token_key = current_token_key(&runtime);
    let encrypted = match encrypt_bytes(&body, &token_key.key) {
        Ok(e) => e,
        Err(e) => return (StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    };

    if let Err(error) = fs::write(&target_path, &encrypted).await {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("cannot save file: {}", error),
        )
            .into_response();
    }

    match runtime.state.lock() {
        Ok(mut state) => {
            state.files.insert(
                file_id.clone(),
                SharedFile {
                    path: target_path,
                    file_name: file_name.clone(),
                    mime: mime.clone(),
                },
            );
            let message = add_message(
                &mut state,
                "phone",
                "file",
                format!("sent {}", file_name),
                Some(file_name),
                Some(file_size),
                Some(mime),
                Some(format!("/api/download/{}", file_id)),
            );
            Json(message).into_response()
        }
        Err(_) => (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response(),
    }
}

async fn api_download_file(
    AxumState(runtime): AxumState<LinkRuntime>,
    Path(file_id): Path<String>,
    Query(query): Query<ApiQuery>,
) -> Response {
    if !is_authorized(&query, &runtime) {
        return (StatusCode::UNAUTHORIZED, "bad token").into_response();
    }
    mark_phone_seen(&runtime);

    let shared_file = match runtime.state.lock() {
        Ok(state) => state.files.get(&file_id).cloned(),
        Err(_) => return (StatusCode::INTERNAL_SERVER_ERROR, "state unavailable").into_response(),
    };

    let Some(shared_file) = shared_file else {
        return (StatusCode::NOT_FOUND, "file not found").into_response();
    };

    // Stream the encrypted file directly (no full memory load)
    let file = match fs::File::open(&shared_file.path).await {
        Ok(f) => f,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot open file: {}", error),
            )
                .into_response()
        }
    };

    // The file is stored encrypted (nonce || ciphertext).
    // We include the key as a custom header so the client can decrypt it.
    let token_key = current_token_key(&runtime);
    let key_hex = hex::encode(token_key.key);

    let mut headers = HeaderMap::new();
    headers.insert(
        axum::http::header::CONTENT_TYPE,
        HeaderValue::from_static("application/octet-stream"),
    );
    let disposition = format!(
        "attachment; filename=\"{}.encrypted\"",
        sanitize_file_name(&shared_file.file_name)
    );
    headers.insert(
        CONTENT_DISPOSITION,
        HeaderValue::from_str(&disposition).unwrap_or_else(|_| HeaderValue::from_static("attachment")),
    );
    headers.insert(
        "X-Encryption-Key-Hex",
        HeaderValue::from_str(&key_hex).unwrap_or_else(|_| HeaderValue::from_static("")),
    );
    headers.insert(
        "X-Original-Name",
        HeaderValue::from_str(&shared_file.file_name)
            .unwrap_or_else(|_| HeaderValue::from_static("file")),
    );
    headers.insert(
        "X-Original-Mime",
        HeaderValue::from_str(&shared_file.mime)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );

    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);

    (headers, body).into_response()
}

/// ── Tauri Commands ──

#[tauri::command]
fn get_link_profile(runtime: tauri::State<'_, LinkRuntime>) -> LinkProfile {
    let ip = primary_ipv4();
    let token_key = current_token_key(&runtime);
    let (incoming_dir, phone_connected) = runtime
        .state
        .lock()
        .map(|state| {
            let connected = state
                .last_phone_seen_at
                .map(|seen| now_millis().saturating_sub(seen) < 10_000)
                .unwrap_or(false);
            (state.incoming_dir.display().to_string(), connected)
        })
        .unwrap_or_default();

    LinkProfile {
        device_name: local_device_name(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        primary_address: ip.clone(),
        pairing_code: token_key.token.clone(),
        pairing_url: format!("http://{}:{}/?token={}", ip, runtime.port, token_key.token),
        session_port: runtime.port,
        protocol: "sidewire-lan-v2".to_string(),
        incoming_dir,
        phone_connected,
    }
}

#[tauri::command]
fn list_messages(runtime: tauri::State<'_, LinkRuntime>) -> Result<Vec<LinkMessage>, String> {
    runtime
        .state
        .lock()
        .map(|state| state.messages.clone())
        .map_err(|_| "state unavailable".to_string())
}

#[tauri::command]
fn send_text(runtime: tauri::State<'_, LinkRuntime>, text: String) -> Result<LinkMessage, String> {
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("message is empty".to_string());
    }

    let mut state = runtime
        .state
        .lock()
        .map_err(|_| "state unavailable".to_string())?;

    Ok(add_message(
        &mut state, "pc", "text", text, None, None, None, None,
    ))
}

#[tauri::command]
fn share_file(runtime: tauri::State<'_, LinkRuntime>, path: String) -> Result<LinkMessage, String> {
    let path = PathBuf::from(path);
    let metadata =
        std::fs::metadata(&path).map_err(|error| format!("cannot read file: {}", error))?;
    if !metadata.is_file() {
        return Err("selected path is not a file".to_string());
    }

    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_else(|| "file.bin".to_string());
    let file_name = sanitize_file_name(&file_name);
    let file_size = metadata.len();
    let mime = simple_mime(&file_name);

    // Read and encrypt the file
    let plaintext = std::fs::read(&path)
        .map_err(|e| format!("cannot read file: {}", e))?;
    let token_key = current_token_key(&runtime);
    let encrypted = encrypt_bytes(&plaintext, &token_key.key)?;

    let mut state = runtime
        .state
        .lock()
        .map_err(|_| "state unavailable".to_string())?;

    let file_id = reserve_id(&mut state, "file");

    // Write encrypted file to incoming directory
    let encrypted_path = state.incoming_dir.join(format!("{}-{}", file_id, file_name));
    std::fs::write(&encrypted_path, &encrypted)
        .map_err(|e| format!("cannot write encrypted file: {}", e))?;

    state.files.insert(
        file_id.clone(),
        SharedFile {
            path: encrypted_path,
            file_name: file_name.clone(),
            mime: mime.clone(),
        },
    );

    Ok(add_message(
        &mut state,
        "pc",
        "file",
        format!("shared {}", file_name),
        Some(file_name),
        Some(file_size),
        Some(mime),
        Some(format!("/api/download/{}", file_id)),
    ))
}

#[tauri::command]
fn disconnect_phone(runtime: tauri::State<'_, LinkRuntime>) -> Result<LinkMessage, String> {
    rotate_token_key(&runtime)?;
    let message = {
        let mut state = runtime
            .state
            .lock()
            .map_err(|_| "state unavailable".to_string())?;

        state.last_phone_seen_at = None;
        state.last_was_connected = false;
        add_message(
            &mut state,
            "system",
            "system",
            "Phone disconnected. Scan the new QR code to reconnect.".to_string(),
            None,
            None,
            None,
            None,
        )
    };
    write_session_profile(&runtime);
    Ok(message)
}

// ── Conversation commands ──

#[tauri::command]
fn save_conversation(
    runtime: tauri::State<'_, LinkRuntime>,
    label: String,
) -> Result<SavedConversation, String> {
    let messages = runtime
        .state
        .lock()
        .map(|state| state.messages.clone())
        .map_err(|_| "state unavailable".to_string())?;

    save_conversation_file(&runtime.conversations_dir, &label, &messages)
}

#[tauri::command]
fn load_conversation(
    runtime: tauri::State<'_, LinkRuntime>,
    id: String,
) -> Result<SavedConversation, String> {
    load_conversation_file(&runtime.conversations_dir, &id)
}

#[tauri::command]
fn list_saved_conversations(
    runtime: tauri::State<'_, LinkRuntime>,
) -> Result<Vec<SavedConversation>, String> {
    list_conversation_files(&runtime.conversations_dir)
}

#[tauri::command]
fn delete_conversation(
    runtime: tauri::State<'_, LinkRuntime>,
    id: String,
) -> Result<(), String> {
    delete_conversation_file(&runtime.conversations_dir, &id)
}

/// ── Cleanup command ──

#[tauri::command]
fn cleanup_old_files_now(runtime: tauri::State<'_, LinkRuntime>) -> Result<String, String> {
    let incoming_dir = runtime
        .state
        .lock()
        .map(|state| state.incoming_dir.clone())
        .map_err(|_| "state unavailable".to_string())?;

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut deleted = 0u64;
    if let Ok(entries) = std::fs::read_dir(&incoming_dir) {
        for entry in entries.flatten() {
            if let Ok(metadata) = entry.metadata() {
                if metadata.is_file() {
                    if let Ok(modified) = metadata.created().or_else(|_| metadata.modified()) {
                        if let Ok(age) = modified
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                        {
                            if now.saturating_sub(age) > FILE_RETENTION_SECS {
                                let _ = std::fs::remove_file(entry.path());
                                deleted += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(format!("Cleaned up {} old file(s)", deleted))
}

/// ── App lifecycle ──

fn write_session_profile(runtime: &LinkRuntime) {
    let ip = primary_ipv4();
    let token_key = current_token_key(runtime);
    let (incoming_dir, phone_connected) = runtime
        .state
        .lock()
        .map(|state| {
            let connected = state
                .last_phone_seen_at
                .map(|seen| now_millis().saturating_sub(seen) < 10_000)
                .unwrap_or(false);
            (state.incoming_dir.display().to_string(), connected)
        })
        .unwrap_or_default();

    let profile = LinkProfile {
        device_name: local_device_name(),
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        primary_address: ip.clone(),
        pairing_code: token_key.token.clone(),
        pairing_url: format!("http://{}:{}/?token={}", ip, runtime.port, token_key.token),
        session_port: runtime.port,
        protocol: "sidewire-lan-v2".to_string(),
        incoming_dir,
        phone_connected,
    };

    if let Ok(profile_json) = serde_json::to_string_pretty(&profile) {
        let session_path = std::env::temp_dir().join("sidewire").join("session.json");
        let _ = std::fs::write(session_path, profile_json);
    }
}

fn start_link_server() -> Result<LinkRuntime, String> {
    let token = generate_secure_token();
    let key = derive_key(&token);

    let incoming_dir = std::env::temp_dir().join("sidewire").join("incoming");
    std::fs::create_dir_all(&incoming_dir)
        .map_err(|error| format!("cannot create incoming directory: {}", error))?;

    // Conversations stored in app data
    let conversations_dir = dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from(std::env::temp_dir().join("sidewire")))
        .join("SideWire")
        .join("conversations");
    std::fs::create_dir_all(&conversations_dir)
        .map_err(|error| format!("cannot create conversations directory: {}", error))?;

    let listener = TcpListener::bind("0.0.0.0:0")
        .map_err(|error| format!("cannot bind local link server: {}", error))?;
    let port = listener
        .local_addr()
        .map_err(|error| format!("cannot read local server port: {}", error))?
        .port();
    listener
        .set_nonblocking(true)
        .map_err(|error| format!("cannot configure local server: {}", error))?;

    let state = Arc::new(Mutex::new(LinkState {
        messages: Vec::new(),
        files: HashMap::new(),
        incoming_dir: incoming_dir.clone(),
        next_id: 0,
        last_phone_seen_at: None,
        last_was_connected: false,
    }));

    {
        let mut state_lock = state.lock().map_err(|_| "state unavailable".to_string())?;
        add_message(
            &mut state_lock,
            "system",
            "system",
            "Ready. Connect a phone to send messages/files.".to_string(),
            None,
            None,
            None,
            None,
        );
    }

    let runtime = LinkRuntime {
        state,
        port,
        token_key: Arc::new(Mutex::new(TokenKey { token, key })),
        conversations_dir: conversations_dir.clone(),
    };
    write_session_profile(&runtime);
    let server_runtime = runtime.clone();

    let router = Router::new()
        .route("/", get(mobile_page))
        .route("/api/profile", get(api_profile))
        .route("/api/messages", get(api_messages))
        .route("/api/message", post(api_post_message))
        .route("/api/disconnect", post(api_disconnect))
        .route("/api/upload", post(api_upload_file))
        .route("/api/download/{file_id}", get(api_download_file))
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
                .allow_headers(Any),
        )
        .layer(DefaultBodyLimit::max(512 * 1024 * 1024))
        .with_state(server_runtime);

    // Spawn the HTTP server
    let incoming_for_cleanup = incoming_dir.clone();
    tauri::async_runtime::spawn(async move {
        match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => {
                if let Err(error) = axum::serve(listener, router).await {
                    eprintln!("sidewire server stopped: {}", error);
                }
            }
            Err(error) => eprintln!("sidewire server could not start: {}", error),
        }
    });

    // Spawn periodic cleanup
    tauri::async_runtime::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(CLEANUP_INTERVAL_SECS)).await;
            cleanup_old_files(incoming_for_cleanup.clone()).await;
        }
    });

    Ok(runtime)
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let runtime = start_link_server().map_err(|error| {
                Box::<dyn std::error::Error>::from(std::io::Error::new(
                    std::io::ErrorKind::Other,
                    error,
                ))
            })?;
            app.manage(runtime);
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_link_profile,
            list_messages,
            send_text,
            share_file,
            disconnect_phone,
            save_conversation,
            load_conversation,
            list_saved_conversations,
            delete_conversation,
            cleanup_old_files_now,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// ── Embedded mobile HTML ──

const MOBILE_CLIENT_HTML: &str = r##"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <title>SideWire</title>
  <style>
    * { box-sizing: border-box; }
    html, body { height: 100%; margin: 0; }
    body {
      background: #050508;
      color: #f0f0f0;
      font-family: "Cascadia Mono", Consolas, "Courier New", monospace;
      overflow: hidden;
    }
    body::after {
      content: "";
      position: fixed;
      inset: 0;
      pointer-events: none;
      background: repeating-linear-gradient(to bottom, transparent 0, transparent 3px, rgba(0,0,0,.12) 3px, rgba(0,0,0,.12) 4px);
      z-index: 10;
    }
    .shell { min-height: 100%; display: grid; grid-template-rows: auto 1fr auto; }
    header {
      border-bottom: 1px solid rgba(30,144,255,.22);
      padding: 14px 16px;
      display: flex;
      gap: 10px;
      align-items: center;
      justify-content: space-between;
    }
    .prompt { color: rgba(30,144,255,.8); font-size: 12px; }
    .state { color: #e84a8a; font-size: 12px; }
    main {
      overflow: auto;
      padding: 16px;
      display: flex;
      flex-direction: column;
      gap: 10px;
    }
    .msg {
      border: 1px solid rgba(30,144,255,.18);
      background: rgba(10,10,15,.78);
      padding: 10px 12px;
      max-width: 88%;
      border-radius: 4px;
    }
    .pc { align-self: flex-start; }
    .phone { align-self: flex-end; border-color: rgba(232,74,138,.35); }
    .system { align-self: center; color: rgba(255,255,255,.55); font-size: 12px; }
    .meta { color: rgba(30,144,255,.65); font-size: 11px; margin-bottom: 4px; }
    .file { margin-top: 8px; display: flex; align-items: center; justify-content: space-between; gap: 12px; }
    a, button, label {
      color: #f0f0f0;
      border: 1px solid rgba(30,144,255,.32);
      background: rgba(5,5,8,.9);
      border-radius: 3px;
      padding: 9px 11px;
      font: inherit;
      text-decoration: none;
    }
    button, label { cursor: pointer; }
    form {
      border-top: 1px solid rgba(30,144,255,.22);
      padding: 12px;
      display: grid;
      grid-template-columns: 1fr auto auto;
      gap: 8px;
    }
    input[type=text] {
      min-width: 0;
      color: #f0f0f0;
      background: rgba(10,10,15,.9);
      border: 1px solid rgba(30,144,255,.26);
      border-radius: 3px;
      padding: 10px 12px;
      font: inherit;
    }
    input[type=file] { display: none; }
    .error { color: #e84a8a; padding: 8px 16px 0; font-size: 12px; }
  </style>
</head>
<body>
  <div class="shell">
    <header>
      <div>
        <div class="prompt">SideWire</div>
        <strong>Connected to this PC</strong>
      </div>
      <div class="state" id="state">connecting</div>
    </header>
    <div class="error" id="error"></div>
    <main id="messages"></main>
    <form id="form">
      <input id="text" type="text" autocomplete="off" placeholder="Send a message to this PC" />
      <label for="file">Choose file<input id="file" type="file" multiple /></label>
      <button type="submit">send</button>
    </form>
  </div>
  <script>
    const params = new URLSearchParams(location.search);
    const token = params.get("token") || "";
    const messages = document.getElementById("messages");
    const state = document.getElementById("state");
    const error = document.getElementById("error");
    const text = document.getElementById("text");
    const file = document.getElementById("file");
    const form = document.getElementById("form");
    let last = "";

    // ── Crypto helpers (client-side decryption) ──

    async function hexToKey(keyHex) {
      const raw = new Uint8Array(keyHex.match(/.{1,2}/g).map(b => parseInt(b, 16)));
      return await crypto.subtle.importKey("raw", raw, "AES-GCM", false, ["decrypt"]);
    }

    async function decryptFile(encryptedBlob, keyHex) {
      const key = await hexToKey(keyHex);
      const buf = await encryptedBlob.arrayBuffer();
      const nonce = new Uint8Array(buf.slice(0, 12));
      const ct = new Uint8Array(buf.slice(12));
      const plain = await crypto.subtle.decrypt({ name: "AES-GCM", iv: nonce }, key, ct);
      return new Blob([plain]);
    }

    // ── Download with decryption ──

    async function downloadDecrypted(downloadUrl) {
      try {
        const fullUrl = downloadUrl + (downloadUrl.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(token);
        const response = await fetch(fullUrl);
        const keyHex = response.headers.get("X-Encryption-Key-Hex");
        const origName = response.headers.get("X-Original-Name") || "file";
        if (!keyHex) {
          // Fallback: download as-is
          const blob = await response.blob();
          const url = URL.createObjectURL(blob);
          const a = document.createElement("a");
          a.href = url;
          a.download = origName;
          a.click();
          URL.revokeObjectURL(url);
          return;
        }
        const encryptedBlob = await response.blob();
        const decrypted = await decryptFile(encryptedBlob, keyHex);
        const url = URL.createObjectURL(decrypted);
        const a = document.createElement("a");
        a.href = url;
        a.download = origName;
        a.click();
        URL.revokeObjectURL(url);
      } catch (err) {
        error.textContent = "Download failed: " + (err.message || err);
      }
    }

    function api(path) {
      return path + (path.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(token);
    }

    function bytes(n) {
      if (!n) return "0 B";
      const units = ["B", "KB", "MB", "GB"];
      let value = n;
      let index = 0;
      while (value >= 1024 && index < units.length - 1) {
        value = value / 1024;
        index++;
      }
      return value.toFixed(value >= 10 || index === 0 ? 0 : 1) + " " + units[index];
    }

    function render(items) {
      const signature = JSON.stringify(items.map((m) => m.id));
      if (signature === last) return;
      last = signature;
      messages.textContent = "";
      items.forEach((item) => {
        const row = document.createElement("div");
        row.className = "msg " + item.sender;
        const meta = document.createElement("div");
        meta.className = "meta";
        meta.textContent = item.sender + " :: " + new Date(item.createdAt).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
        const body = document.createElement("div");
        body.textContent = item.body;
        row.append(meta, body);
        if (item.kind === "file") {
          const fileRow = document.createElement("div");
          fileRow.className = "file";
          const name = document.createElement("span");
          name.textContent = (item.fileName || "file") + " / " + bytes(item.fileSize || 0);
          fileRow.appendChild(name);
          if (item.downloadUrl) {
            const link = document.createElement("a");
            link.textContent = "download";
            link.addEventListener("click", (e) => {
              e.preventDefault();
              downloadDecrypted(item.downloadUrl);
            });
            link.href = "#";
            link.style.cursor = "pointer";
            fileRow.appendChild(link);
          }
          row.appendChild(fileRow);
        }
        messages.appendChild(row);
      });
      messages.scrollTop = messages.scrollHeight;
    }

    async function refresh() {
      try {
        const res = await fetch(api("/api/messages"), { cache: "no-store" });
        if (!res.ok) throw new Error(await res.text());
        render(await res.json());
        state.textContent = "connected";
        error.textContent = "";
      } catch (err) {
        state.textContent = "offline";
        error.textContent = String(err.message || err);
      }
    }

    form.addEventListener("submit", async (event) => {
      event.preventDefault();
      const value = text.value.trim();
      if (!value) return;
      text.value = "";
      await fetch(api("/api/message"), { method: "POST", body: value });
      refresh();
    });

    file.addEventListener("change", async () => {
      for (const selected of Array.from(file.files || [])) {
        const path = api("/api/upload") + "&name=" + encodeURIComponent(selected.name) + "&mime=" + encodeURIComponent(selected.type || "application/octet-stream");
        await fetch(path, { method: "POST", body: selected });
      }
      file.value = "";
      refresh();
    });

    refresh();
    setInterval(refresh, 1200);
  </script>
</body>
</html>"##;