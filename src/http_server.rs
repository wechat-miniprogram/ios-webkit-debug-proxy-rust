// HTTP/WebSocket service module
// Uses axum framework to replace hand-written HTTP parsing and WebSocket frame handling
// Provides DevTools related HTTP endpoints and WebSocket debug proxy

use axum::{
    extract::{
        ws::{Message, WebSocket},
        Path, State, WebSocketUpgrade,
    },
    http::{header, HeaderMap, Method, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Router,
};
use log::{debug, error, warn};
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};

use crate::DeviceInfo;

/// Device port shared state
#[allow(dead_code)]
pub(crate) struct DevicePortState {
    /// Port number
    pub(crate) port: u16,
    /// Device ID
    pub(crate) device_id: Option<String>,
    /// Device name
    pub(crate) device_name: Option<String>,
    /// Device OS version
    pub(crate) device_os_version: u32,
    /// Whether connected
    pub(crate) connected: bool,
    /// Page list
    pub(crate) pages: Vec<PageEntry>,
    /// Frontend URL
    pub(crate) frontend: Option<String>,
    /// WebSocket message send channel: (page_num, ws_id, message data)
    /// Provided by proxy core, used to forward WebSocket client messages to WebInspector
    pub(crate) ws_to_wi_tx: Option<mpsc::Sender<WsToWiMessage>>,
    /// WebInspector message receiver registrar
    /// When a WebSocket client connects, register a channel for receiving messages from WI
    pub(crate) wi_to_ws_register_tx: Option<mpsc::Sender<WiToWsRegistration>>,
}

/// Page entry
#[derive(Clone)]
pub(crate) struct PageEntry {
    pub(crate) page_num: u32,
    pub(crate) app_id: String,
    #[allow(dead_code)]
    pub(crate) page_id: u32,
    pub(crate) title: Option<String>,
    pub(crate) url: Option<String>,
    #[allow(dead_code)]
    pub(crate) sender_id: Option<String>,
}

/// WebSocket -> WebInspector message
pub(crate) struct WsToWiMessage {
    pub(crate) page_num: u32,
    pub(crate) ws_id: String,
    pub(crate) data: Vec<u8>,
    pub(crate) is_close: bool,
}

/// WebInspector -> WebSocket registration
pub(crate) struct WiToWsRegistration {
    pub(crate) page_num: u32,
    pub(crate) ws_id: String,
    pub(crate) tx: mpsc::Sender<Vec<u8>>,
}

/// Device list shared state (for the device list page on port 9221)
pub(crate) struct DeviceListState {
    pub(crate) devices: Vec<DeviceInfo>,
}

/// axum shared state type
pub(crate) type SharedDevicePortState = Arc<RwLock<DevicePortState>>;
pub(crate) type SharedDeviceListState = Arc<RwLock<DeviceListState>>;

/// Create device port axum Router
pub(crate) fn create_device_router(state: SharedDevicePortState) -> Router {
    Router::new()
        .route("/", get(handle_index).head(handle_index))
        .route("/json", get(handle_json).head(handle_json))
        .route("/json/list", get(handle_json).head(handle_json))
        .route(
            "/json/version",
            get(handle_json_version).head(handle_json_version),
        )
        .route("/devtools/page/{page_num}", get(handle_ws_upgrade))
        .route(
            "/devtools/{*path}",
            get(handle_devtools_static).head(handle_devtools_static),
        )
        .route(
            "/devtools/",
            get(handle_devtools_static).head(handle_devtools_static),
        )
        .fallback(handle_404)
        .with_state(state)
}

/// Create WebSocket-only axum Router (without HTTP page/JSON endpoints)
pub(crate) fn create_device_ws_only_router(state: SharedDevicePortState) -> Router {
    Router::new()
        .route("/devtools/page/{page_num}", get(handle_ws_upgrade))
        .fallback(handle_404)
        .with_state(state)
}

/// Create device list axum Router (port 9221)
pub(crate) fn create_device_list_router(state: SharedDeviceListState) -> Router {
    Router::new()
        .route(
            "/",
            get(handle_device_list_html).head(handle_device_list_html),
        )
        .route(
            "/json",
            get(handle_device_list_json).head(handle_device_list_json),
        )
        .route(
            "/json/list",
            get(handle_device_list_json).head(handle_device_list_json),
        )
        .route("/json/version", get(handle_version_json))
        .fallback(handle_404_simple)
        .with_state(state)
}

/// CORS headers
fn cors_headers() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert("Access-Control-Allow-Origin", "*".parse().unwrap());
    headers.insert("Access-Control-Allow-Methods", "GET, HEAD".parse().unwrap());
    headers
}

/// GET / - HTML page list
async fn handle_index(method: Method, State(state): State<SharedDevicePortState>) -> Response {
    let state = state.read().await;
    let html = build_pages_html(&state);
    let mut response = (StatusCode::OK, cors_headers(), Html(html)).into_response();
    if method == Method::HEAD {
        *response.body_mut() = axum::body::Body::empty();
    }
    response
}

/// GET /json, GET /json/list - JSON page list
async fn handle_json(
    method: Method,
    headers: HeaderMap,
    State(state): State<SharedDevicePortState>,
) -> Response {
    let state = state.read().await;
    let host = headers
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .map(|h| {
            // Strip the port part
            h.rsplit_once(':').map(|(host, _)| host).unwrap_or(h)
        })
        .unwrap_or("localhost");
    let json = build_pages_json(&state, host);

    let mut resp_headers = cors_headers();
    resp_headers.insert(
        header::CONTENT_TYPE,
        "application/json; charset=UTF-8".parse().unwrap(),
    );

    let mut response = (StatusCode::OK, resp_headers, json).into_response();
    if method == Method::HEAD {
        *response.body_mut() = axum::body::Body::empty();
    }
    response
}

/// GET /json/version
async fn handle_json_version(method: Method) -> Response {
    let version = serde_json::json!({
        "Browser": "iOS WebKit Debug Proxy",
        "Protocol-Version": "1.1"
    });
    let mut resp_headers = cors_headers();
    resp_headers.insert(
        header::CONTENT_TYPE,
        "application/json; charset=UTF-8".parse().unwrap(),
    );
    let mut response = (StatusCode::OK, resp_headers, version.to_string()).into_response();
    if method == Method::HEAD {
        *response.body_mut() = axum::body::Body::empty();
    }
    response
}

/// WebSocket upgrade handler - /devtools/page/{page_num}
async fn handle_ws_upgrade(
    Path(page_num): Path<u32>,
    ws: WebSocketUpgrade,
    State(state): State<SharedDevicePortState>,
) -> Response {
    debug!("WebSocket upgrade request: page_num={}", page_num);

    // Verify page exists
    {
        let s = state.read().await;
        if !s.pages.iter().any(|p| p.page_num == page_num) {
            return (StatusCode::NOT_FOUND, "Page not found").into_response();
        }
    }

    ws.on_upgrade(move |socket| handle_ws_connection(socket, page_num, state))
}

/// Handle WebSocket connection
async fn handle_ws_connection(socket: WebSocket, page_num: u32, state: SharedDevicePortState) {
    let ws_id = crate::rpc::new_uuid();
    debug!(
        "WebSocket connection established: page_num={}, ws_id={}",
        page_num, ws_id
    );

    // Create channel for receiving messages from WI
    let (wi_msg_tx, mut wi_msg_rx) = mpsc::channel::<Vec<u8>>(64);

    // Register with proxy core
    {
        let s = state.read().await;
        if let Some(ref register_tx) = s.wi_to_ws_register_tx {
            let _ = register_tx
                .send(WiToWsRegistration {
                    page_num,
                    ws_id: ws_id.clone(),
                    tx: wi_msg_tx,
                })
                .await;
        }
    }

    // Notify proxy core to establish debug channel (forwardSocketSetup)
    {
        let s = state.read().await;
        if let Some(ref ws_to_wi_tx) = s.ws_to_wi_tx {
            let _ = ws_to_wi_tx
                .send(WsToWiMessage {
                    page_num,
                    ws_id: ws_id.clone(),
                    data: Vec::new(), // Empty data means initialize connection
                    is_close: false,
                })
                .await;
        }
    }

    let (mut ws_sender, mut ws_receiver) = socket.split();

    use futures_util::{SinkExt, StreamExt};

    // Bidirectional bridge
    loop {
        tokio::select! {
            // Receive messages from WebSocket client -> forward to WebInspector
            msg = ws_receiver.next() => {
                match msg {
                    Some(Ok(Message::Text(text))) => {
                        let s = state.read().await;
                        if let Some(ref ws_to_wi_tx) = s.ws_to_wi_tx {
                            let _ = ws_to_wi_tx.send(WsToWiMessage {
                                page_num,
                                ws_id: ws_id.clone(),
                                data: text.as_bytes().to_vec(),
                                is_close: false,
                            }).await;
                        }
                    }
                    Some(Ok(Message::Binary(data))) => {
                        let s = state.read().await;
                        if let Some(ref ws_to_wi_tx) = s.ws_to_wi_tx {
                            let _ = ws_to_wi_tx.send(WsToWiMessage {
                                page_num,
                                ws_id: ws_id.clone(),
                                data: data.to_vec(),
                                is_close: false,
                            }).await;
                        }
                    }
                    Some(Ok(Message::Ping(data))) => {
                        if ws_sender.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        // Notify proxy core to close debug channel
                        let s = state.read().await;
                        if let Some(ref ws_to_wi_tx) = s.ws_to_wi_tx {
                            let _ = ws_to_wi_tx.send(WsToWiMessage {
                                page_num,
                                ws_id: ws_id.clone(),
                                data: Vec::new(),
                                is_close: true,
                            }).await;
                        }
                        break;
                    }
                    Some(Ok(_)) => {} // Pong etc., ignore
                    Some(Err(e)) => {
                        warn!("WebSocket receive error: {}", e);
                        break;
                    }
                }
            }
            // Receive messages from WebInspector -> forward to WebSocket client
            msg = wi_msg_rx.recv() => {
                match msg {
                    Some(data) => {
                        let text = String::from_utf8_lossy(&data).to_string();
                        if ws_sender.send(Message::Text(text.into())).await.is_err() {
                            break;
                        }
                    }
                    None => {
                        // WI channel closed
                        let _ = ws_sender.send(Message::Close(None)).await;
                        break;
                    }
                }
            }
        }
    }

    debug!(
        "WebSocket connection closed: page_num={}, ws_id={}",
        page_num, ws_id
    );
}

/// GET /devtools/* - Proxy to frontend static file server
async fn handle_devtools_static(
    method: Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
    State(state): State<SharedDevicePortState>,
) -> Response {
    let resource = uri.path();
    let path = resource.strip_prefix("/devtools/").unwrap_or("");

    let frontend = {
        let s = state.read().await;
        s.frontend.clone()
    };

    let fe_url = match frontend {
        Some(url) => url,
        None => {
            return (
                StatusCode::NOT_FOUND,
                cors_headers(),
                "Frontend is disabled.",
            )
                .into_response();
        }
    };

    // Only support http:// frontend URL for proxying
    if !fe_url.starts_with("http://") && !fe_url.starts_with("https://") {
        return (
            StatusCode::NOT_FOUND,
            cors_headers(),
            "Frontend URL is not http(s)://, cannot proxy.",
        )
            .into_response();
    }

    // Security check
    if path.contains("..") {
        return (StatusCode::FORBIDDEN, cors_headers(), "Invalid path").into_response();
    }

    let is_safe = path
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.' || c == '/' || c == '_');
    if !is_safe && !path.is_empty() {
        return (StatusCode::FORBIDDEN, cors_headers(), "Invalid path").into_response();
    }

    // Parse frontend URL path
    let url = if let Ok(parsed) = url::Url::parse(&fe_url) {
        let fe_dir = if let Some(pos) = parsed.path().rfind('/') {
            &parsed.path()[..=pos]
        } else {
            "/"
        };
        let fe_file = parsed.path().rsplit('/').next().unwrap_or("");

        let final_path = if path.is_empty() {
            format!("{}{}", fe_dir, fe_file)
        } else {
            format!("{}{}", fe_dir, path)
        };

        let mut target = parsed.clone();
        target.set_path(&final_path);
        target.to_string()
    } else {
        return (
            StatusCode::BAD_GATEWAY,
            cors_headers(),
            "Invalid frontend URL",
        )
            .into_response();
    };

    debug!("Proxying /devtools/ request: {} -> {}", resource, url);

    // Use reqwest for async HTTP request
    let client = reqwest::Client::new();
    let req = if method == Method::HEAD {
        client.head(&url)
    } else {
        client.get(&url)
    };

    match req.send().await {
        Ok(resp) => {
            let status =
                StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut resp_headers = cors_headers();

            // Copy Content-Type
            if let Some(ct) = resp.headers().get(header::CONTENT_TYPE) {
                resp_headers.insert(header::CONTENT_TYPE, ct.clone());
            }

            let body = resp.bytes().await.unwrap_or_default();
            (status, resp_headers, body).into_response()
        }
        Err(e) => {
            error!("Proxy request failed: {}", e);
            (
                StatusCode::BAD_GATEWAY,
                cors_headers(),
                format!("Unable to fetch from frontend: {}", e),
            )
                .into_response()
        }
    }
}

/// 404 handler
async fn handle_404(
    method: Method,
    axum::extract::OriginalUri(uri): axum::extract::OriginalUri,
) -> Response {
    let body = format!("<html><body>404: {} not found</body></html>", uri.path());
    let mut resp_headers = cors_headers();
    resp_headers.insert(
        header::CONTENT_TYPE,
        "text/html; charset=UTF-8".parse().unwrap(),
    );
    let mut response = (StatusCode::NOT_FOUND, resp_headers, body).into_response();
    if method == Method::HEAD {
        *response.body_mut() = axum::body::Body::empty();
    }
    response
}

/// Simple 404
async fn handle_404_simple() -> Response {
    (
        StatusCode::NOT_FOUND,
        cors_headers(),
        "<html><body>404 Not Found</body></html>",
    )
        .into_response()
}

/// Device list - HTML
async fn handle_device_list_html(
    method: Method,
    State(state): State<SharedDeviceListState>,
) -> Response {
    let state = state.read().await;
    let items: Vec<String> = state
        .devices
        .iter()
        .map(|d| {
            format!(
                r#"<li><a href="http://localhost:{}/">{}</a> - {}</li>"#,
                d.port, d.device_name, d.device_id
            )
        })
        .collect();
    let html = format!(
        "<html><head><title>iOS Devices</title></head><body>iOS Devices:<p><ol>{}</ol></body></html>",
        items.join("\n")
    );
    let mut response = (StatusCode::OK, cors_headers(), Html(html)).into_response();
    if method == Method::HEAD {
        *response.body_mut() = axum::body::Body::empty();
    }
    response
}

/// Device list - JSON
async fn handle_device_list_json(
    method: Method,
    State(state): State<SharedDeviceListState>,
) -> Response {
    let state = state.read().await;
    let items: Vec<serde_json::Value> = state
        .devices
        .iter()
        .map(|d| {
            let os_major = (d.device_os_version >> 16) & 0xFF;
            let os_minor = (d.device_os_version >> 8) & 0xFF;
            let os_patch = d.device_os_version & 0xFF;
            serde_json::json!({
                "deviceId": d.device_id,
                "deviceName": d.device_name,
                "deviceOSVersion": format!("{}.{}.{}", os_major, os_minor, os_patch),
                "url": format!("localhost:{}", d.port)
            })
        })
        .collect();

    let json = serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string());

    let mut resp_headers = cors_headers();
    resp_headers.insert(
        header::CONTENT_TYPE,
        "application/json; charset=UTF-8".parse().unwrap(),
    );

    let mut response = (StatusCode::OK, resp_headers, json).into_response();
    if method == Method::HEAD {
        *response.body_mut() = axum::body::Body::empty();
    }
    response
}

/// Version info JSON
async fn handle_version_json(method: Method) -> Response {
    let version = serde_json::json!({
        "Browser": "iOS WebKit Debug Proxy",
        "Protocol-Version": "1.1"
    });
    let mut resp_headers = cors_headers();
    resp_headers.insert(
        header::CONTENT_TYPE,
        "application/json; charset=UTF-8".parse().unwrap(),
    );
    let mut response = (StatusCode::OK, resp_headers, version.to_string()).into_response();
    if method == Method::HEAD {
        *response.body_mut() = axum::body::Body::empty();
    }
    response
}

// =====================
// HTML/JSON build helper functions
// =====================

/// Build page list HTML
fn build_pages_html(state: &DevicePortState) -> String {
    let device_name = state.device_name.as_deref().unwrap_or("Unknown");

    let items: Vec<String> = state
        .pages
        .iter()
        .map(|page| {
            let frontend_url = generate_frontend_url(state, page.page_num);
            format!(
                r#"<li value="{}"><a href="{}">{} - {}</a></li>"#,
                page.page_num,
                frontend_url,
                page.title.as_deref().unwrap_or("?"),
                page.url.as_deref().unwrap_or("?")
            )
        })
        .collect();

    format!(
        "<html><head><title>{}</title></head><body>Inspectable pages for {}:<p><ol>{}</ol></body></html>",
        device_name,
        device_name,
        items.join("\n")
    )
}

/// Build page list JSON
fn build_pages_json(state: &DevicePortState, host: &str) -> String {
    let items: Vec<serde_json::Value> = state
        .pages
        .iter()
        .map(|page| {
            let frontend_url = generate_frontend_url(state, page.page_num);
            serde_json::json!({
                "devtoolsFrontendUrl": frontend_url,
                "faviconUrl": "",
                "thumbnailUrl": format!("/thumb/{}", page.url.as_deref().unwrap_or("")),
                "title": page.title.as_deref().unwrap_or(""),
                "url": page.url.as_deref().unwrap_or(""),
                "webSocketDebuggerUrl": format!("ws://{}:{}/devtools/page/{}", host, state.port, page.page_num),
                "appId": page.app_id
            })
        })
        .collect();

    serde_json::to_string(&items).unwrap_or_else(|_| "[]".to_string())
}

/// Generate frontend URL
fn generate_frontend_url(state: &DevicePortState, page_num: u32) -> String {
    if let Some(ref fe) = state.frontend {
        format!(
            "/devtools/{}?ws=localhost:{}/devtools/page/{}",
            fe.rsplit('/').next().unwrap_or("devtools.html"),
            state.port,
            page_num
        )
    } else {
        String::new()
    }
}
