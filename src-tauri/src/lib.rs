use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State as AxumState},
    http::{
        header::{CONTENT_DISPOSITION, CONTENT_TYPE},
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    response::{Html, IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    net::{TcpListener, UdpSocket},
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{SystemTime, UNIX_EPOCH},
};
use tauri::Manager;
use tower_http::cors::{Any, CorsLayer};

#[derive(Clone)]
struct LinkRuntime {
    state: Arc<Mutex<LinkState>>,
    port: u16,
    token: Arc<Mutex<String>>,
}

struct LinkState {
    messages: Vec<LinkMessage>,
    files: HashMap<String, SharedFile>,
    incoming_dir: PathBuf,
    next_id: u64,
    last_phone_seen_at: Option<u64>,
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

#[derive(Clone, Serialize)]
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

#[derive(Deserialize)]
struct ApiQuery {
    token: Option<String>,
    name: Option<String>,
    mime: Option<String>,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn pairing_code() -> String {
    format!("{:06}", now_millis() % 1_000_000)
}

fn current_token(runtime: &LinkRuntime) -> String {
    runtime
        .token
        .lock()
        .map(|token| token.clone())
        .unwrap_or_default()
}

fn rotate_token(runtime: &LinkRuntime) -> Result<String, String> {
    let mut token = runtime
        .token
        .lock()
        .map_err(|_| "token unavailable".to_string())?;
    let current = token.clone();
    let mut next = pairing_code();
    if next == current {
        next = format!("{:06}", now_millis().saturating_add(1) % 1_000_000);
    }
    *token = next.clone();
    Ok(next)
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

fn is_authorized(query: &ApiQuery, runtime: &LinkRuntime) -> bool {
    runtime
        .token
        .lock()
        .map(|token| query.token.as_deref() == Some(token.as_str()))
        .unwrap_or(false)
}

fn mark_phone_seen(runtime: &LinkRuntime) {
    if let Ok(mut state) = runtime.state.lock() {
        state.last_phone_seen_at = Some(now_millis());
    }
}

fn make_profile(runtime: &LinkRuntime) -> LinkProfile {
    let ip = primary_ipv4();
    let token = current_token(runtime);
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
        pairing_code: token.clone(),
        pairing_url: format!("http://{}:{}/?token={}", ip, runtime.port, token),
        session_port: runtime.port,
        protocol: "sidewire-lan-v1".to_string(),
        incoming_dir,
        phone_connected,
    }
}

fn write_session_profile(runtime: &LinkRuntime) {
    if let Ok(profile_json) = serde_json::to_string_pretty(&make_profile(runtime)) {
        let session_path = std::env::temp_dir().join("sidewire").join("session.json");
        let _ = std::fs::write(session_path, profile_json);
    }
}

fn disconnect_runtime(runtime: &LinkRuntime) -> Result<LinkMessage, String> {
    rotate_token(runtime)?;
    let message = {
        let mut state = runtime
            .state
            .lock()
            .map_err(|_| "state unavailable".to_string())?;

        state.last_phone_seen_at = None;
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
    write_session_profile(runtime);
    Ok(message)
}

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
    Json(make_profile(&runtime)).into_response()
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

    match disconnect_runtime(&runtime) {
        Ok(message) => Json(message).into_response(),
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
        if let Err(error) = tokio::fs::create_dir_all(parent).await {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot create incoming directory: {}", error),
            )
                .into_response();
        }
    }

    if let Err(error) = tokio::fs::write(&target_path, &body).await {
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

    let bytes = match tokio::fs::read(&shared_file.path).await {
        Ok(bytes) => bytes,
        Err(error) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("cannot read file: {}", error),
            )
                .into_response()
        }
    };

    let mut headers = HeaderMap::new();
    let content_type = HeaderValue::from_str(&shared_file.mime)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
    let disposition = format!(
        "attachment; filename=\"{}\"",
        sanitize_file_name(&shared_file.file_name)
    );
    let content_disposition = HeaderValue::from_str(&disposition)
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));

    headers.insert(CONTENT_TYPE, content_type);
    headers.insert(CONTENT_DISPOSITION, content_disposition);
    (headers, bytes).into_response()
}

#[tauri::command]
fn get_link_profile(runtime: tauri::State<'_, LinkRuntime>) -> LinkProfile {
    make_profile(&runtime)
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

    let mut state = runtime
        .state
        .lock()
        .map_err(|_| "state unavailable".to_string())?;

    let file_id = reserve_id(&mut state, "file");
    state.files.insert(
        file_id.clone(),
        SharedFile {
            path,
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
    disconnect_runtime(&runtime)
}

fn start_link_server() -> Result<LinkRuntime, String> {
    let token = pairing_code();
    let incoming_dir = std::env::temp_dir().join("sidewire").join("incoming");
    std::fs::create_dir_all(&incoming_dir)
        .map_err(|error| format!("cannot create incoming directory: {}", error))?;

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
        incoming_dir,
        next_id: 0,
        last_phone_seen_at: None,
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
        token: Arc::new(Mutex::new(token)),
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
            disconnect_phone
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

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

    function api(path) {
      return path + (path.includes("?") ? "&" : "?") + "token=" + encodeURIComponent(token);
    }

    function download(path) {
      if (!path) return "";
      if (!path.startsWith("http")) return api(path);
      const url = new URL(path);
      url.searchParams.set("token", token);
      return url.toString();
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
            link.href = download(item.downloadUrl);
            link.textContent = "download";
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
