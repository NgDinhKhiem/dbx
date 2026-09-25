use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use std::{net::Ipv4Addr, net::TcpListener};

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tauri::async_runtime::JoinHandle;
use tokio_util::sync::CancellationToken;

use dbx_core::connection::AppState;

use super::local_auth::{constant_time_eq, is_app_origin, random_token};

/// Ephemeral by default: the webview learns the port (and the per-launch
/// token) through `redis_pubsub_server_endpoint`, so nothing depends on a
/// well-known port that other local software or web pages could target.
const DEFAULT_PUBSUB_PORT: u16 = 0;

pub struct PubSubServerState {
    port: Option<u16>,
    token: Arc<str>,
    shutdown: CancellationToken,
    task: Mutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
struct PubSubRouterState {
    app: Arc<AppState>,
    token: Arc<str>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PubSubServerEndpoint {
    port: u16,
    token: String,
}

impl PubSubServerState {
    fn unavailable() -> Self {
        Self { port: None, token: Arc::from(""), shutdown: CancellationToken::new(), task: Mutex::new(None) }
    }

    fn get(&self) -> Result<u16, String> {
        self.port.ok_or_else(|| "Redis PubSub server is unavailable".to_string())
    }

    pub async fn shutdown(&self, deadline: Duration) {
        self.shutdown.cancel();
        let task = self.task.lock().unwrap_or_else(|error| error.into_inner()).take();
        let Some(mut task) = task else { return };
        if tokio::time::timeout(deadline, &mut task).await.is_err() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for PubSubServerState {
    fn drop(&mut self) {
        self.shutdown.cancel();
        if let Some(task) = self.task.get_mut().unwrap_or_else(|error| error.into_inner()).take() {
            task.abort();
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PubSubWsParams {
    connection_id: String,
    #[serde(default)]
    monitor: bool,
    #[serde(default)]
    token: String,
}

fn build_pubsub_router(state: Arc<AppState>, token: Arc<str>) -> Router {
    Router::new().route("/api/redis/pubsub/ws", get(ws_handler)).with_state(PubSubRouterState { app: state, token })
}

/// Browsers attach an Origin to every WebSocket handshake, so any web page
/// can reach a loopback port; only the DBX webview's origin is accepted.
/// Non-browser clients (no Origin) still need the per-launch token.
fn pubsub_request_allowed(headers: &HeaderMap, expected_token: &str, token: &str) -> bool {
    if expected_token.is_empty() || !constant_time_eq(expected_token, token) {
        return false;
    }
    match headers.get(header::ORIGIN) {
        None => true,
        Some(origin) => origin.to_str().is_ok_and(is_app_origin),
    }
}

fn pubsub_server_port() -> u16 {
    std::env::var("DBX_PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(DEFAULT_PUBSUB_PORT)
}

#[tauri::command]
pub fn redis_pubsub_server_port(state: tauri::State<'_, PubSubServerState>) -> Result<u16, String> {
    state.get()
}

/// Port plus the per-launch token the WebSocket handshake must carry.
#[tauri::command]
pub fn redis_pubsub_server_endpoint(
    state: tauri::State<'_, PubSubServerState>,
) -> Result<PubSubServerEndpoint, String> {
    Ok(PubSubServerEndpoint { port: state.get()?, token: state.token.to_string() })
}

fn bind_pubsub_listener(preferred_port: u16) -> Result<TcpListener, String> {
    let preferred_addr = (Ipv4Addr::LOCALHOST, preferred_port);
    match TcpListener::bind(preferred_addr) {
        Ok(listener) => Ok(listener),
        Err(preferred_error) if preferred_port != 0 => {
            log::warn!(
                "Failed to bind PubSub server on {preferred_addr:?}: {preferred_error}; using an available port instead"
            );
            TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).map_err(|fallback_error| {
                format!("Failed to bind PubSub server on an available port: {fallback_error}")
            })
        }
        Err(error) => Err(format!("Failed to bind PubSub server on {preferred_addr:?}: {error}")),
    }
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    headers: HeaderMap,
    Query(params): Query<PubSubWsParams>,
    State(router_state): State<PubSubRouterState>,
) -> Response {
    if !pubsub_request_allowed(&headers, &router_state.token, &params.token) {
        return StatusCode::FORBIDDEN.into_response();
    }
    let state = router_state.app;
    let connection_id = params.connection_id;
    ws.on_upgrade(move |socket| async move {
        if params.monitor {
            handle_monitor_socket(socket, state, connection_id).await;
        } else {
            handle_socket(socket, state, connection_id).await;
        }
    })
    .into_response()
}

async fn handle_monitor_socket(mut socket: WebSocket, state: Arc<AppState>, connection_id: String) {
    let monitor = tokio::select! {
        result = dbx_core::redis_ops::redis_create_monitor_core(&state, &connection_id) => result,
        _ = socket.recv() => return,
    };
    let monitor = match monitor {
        Ok(monitor) => monitor,
        Err(error) => {
            let _ = socket.send(Message::Text(serde_json::json!({ "error": error }).to_string().into())).await;
            return;
        }
    };
    if socket.send(Message::Text(serde_json::json!({ "ready": true }).to_string().into())).await.is_err() {
        return;
    }
    let mut messages = monitor.into_on_message::<String>();
    let (mut sender, mut receiver) = socket.split();
    loop {
        tokio::select! {
            incoming = receiver.next() => {
                match incoming {
                    Some(Ok(Message::Ping(payload))) => {
                        if sender.send(Message::Pong(payload)).await.is_err() { break; }
                    }
                    Some(Ok(Message::Pong(_))) => {}
                    _ => break,
                }
            }
            message = messages.next() => {
                let Some(message) = message else {
                    let _ = sender.send(Message::Text(serde_json::json!({ "error": "Redis MONITOR connection closed" }).to_string().into())).await;
                    break;
                };
                let event = Message::Text(serde_json::json!({ "message": message }).to_string().into());
                match tokio::time::timeout(std::time::Duration::from_secs(5), sender.send(event)).await {
                    Ok(Ok(())) => {}
                    _ => break,
                }
            }
        }
    }
}

async fn handle_socket(socket: WebSocket, state: Arc<AppState>, connection_id: String) {
    // Create PubSub connection
    let pubsub = match dbx_core::redis_ops::redis_create_pubsub_core(&state, &connection_id).await {
        Ok(p) => p,
        Err(e) => {
            let (mut sender, _) = socket.split();
            let _ = sender.send(Message::Text(format!(r#"{{"error":"{e}"}}"#).into())).await;
            return;
        }
    };

    let (mut sink, mut stream) = pubsub.split();
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // Channel for WebSocket commands -> PubSub sink
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    // Task: Read WebSocket commands
    let ws_read = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            match msg {
                Message::Text(text) => {
                    if cmd_tx.send(text.to_string()).is_err() {
                        break;
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    // Task: Apply commands to PubSub sink
    let sink_handle = tokio::spawn(async move {
        while let Some(text) = cmd_rx.recv().await {
            if let Err(e) = handle_command(&mut sink, &text).await {
                log::warn!("PubSub command error: {e}");
            }
        }
    });

    // Forward Redis messages to WebSocket (uses ws_sender, no mutex contention)
    while let Some(msg) = stream.next().await {
        let payload: String = msg.get_payload().unwrap_or_default();
        let channel = msg.get_channel_name().to_string();
        let pattern: Option<String> = msg.get_pattern().ok();
        let json = serde_json::json!({
            "channel": channel,
            "pattern": pattern,
            "payload": payload,
        });
        let text = serde_json::to_string(&json).unwrap_or_default();
        if ws_sender.send(Message::Text(text.into())).await.is_err() {
            break;
        }
    }

    ws_read.abort();
    sink_handle.abort();
}

#[derive(Deserialize)]
#[serde(tag = "type")]
enum PubSubCommand {
    #[serde(rename = "subscribe")]
    Subscribe { channels: Vec<String> },
    #[serde(rename = "psubscribe")]
    Psubscribe { patterns: Vec<String> },
    #[serde(rename = "unsubscribe")]
    Unsubscribe { channels: Vec<String> },
    #[serde(rename = "punsubscribe")]
    Punsubscribe { patterns: Vec<String> },
}

async fn handle_command(sink: &mut redis::aio::PubSubSink, text: &str) -> Result<(), String> {
    let cmd: PubSubCommand = serde_json::from_str(text).map_err(|e| format!("Invalid PubSub command: {e}"))?;

    match cmd {
        PubSubCommand::Subscribe { channels } => {
            for ch in &channels {
                sink.subscribe(ch).await.map_err(|e| format!("Subscribe error: {e}"))?;
            }
        }
        PubSubCommand::Psubscribe { patterns } => {
            for pat in &patterns {
                sink.psubscribe(pat).await.map_err(|e| format!("PSubscribe error: {e}"))?;
            }
        }
        PubSubCommand::Unsubscribe { channels } => {
            for ch in &channels {
                sink.unsubscribe(ch).await.map_err(|e| format!("Unsubscribe error: {e}"))?;
            }
        }
        PubSubCommand::Punsubscribe { patterns } => {
            for pat in &patterns {
                sink.punsubscribe(pat).await.map_err(|e| format!("PUnsubscribe error: {e}"))?;
            }
        }
    }
    Ok(())
}

/// Start the embedded web server for PubSub WebSocket support.
/// Runs on a background task using the shared AppState.
pub fn start_pubsub_server(state: Arc<AppState>) -> PubSubServerState {
    let token: Arc<str> = Arc::from(random_token());
    let router = build_pubsub_router(state, token.clone());
    let listener = match bind_pubsub_listener(pubsub_server_port()) {
        Ok(listener) => listener,
        Err(error) => {
            log::warn!("{error}");
            return PubSubServerState::unavailable();
        }
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(error) => {
            log::warn!("Failed to read PubSub server address: {error}");
            return PubSubServerState::unavailable();
        }
    };
    if let Err(error) = listener.set_nonblocking(true) {
        log::warn!("Failed to configure PubSub server listener: {error}");
        return PubSubServerState::unavailable();
    }

    start_pubsub_server_with_listener(listener, addr, router, token)
}

fn start_pubsub_server_with_listener(
    listener: TcpListener,
    addr: std::net::SocketAddr,
    router: Router,
    token: Arc<str>,
) -> PubSubServerState {
    let shutdown = CancellationToken::new();
    let shutdown_signal = shutdown.clone();
    let task = tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(error) => {
                log::warn!("Failed to start PubSub server on {addr}: {error}");
                return;
            }
        };
        log::info!("PubSub WebSocket server listening on {addr}");
        if let Err(error) =
            axum::serve(listener, router).with_graceful_shutdown(shutdown_signal.cancelled_owned()).await
        {
            log::warn!("PubSub server stopped with error: {error}");
        }
    });

    PubSubServerState { port: Some(addr.port()), token, shutdown, task: Mutex::new(Some(task)) }
}

#[cfg(test)]
mod tests {
    use super::{bind_pubsub_listener, pubsub_request_allowed, start_pubsub_server_with_listener};
    use axum::http::{header, HeaderMap, HeaderValue};
    use axum::Router;
    use std::net::{Ipv4Addr, TcpListener};
    use std::sync::Arc;
    use std::time::Duration;

    fn origin(value: &'static str) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(header::ORIGIN, HeaderValue::from_static(value));
        headers
    }

    #[test]
    fn websocket_requires_token_and_app_origin() {
        let token = "a".repeat(64);
        assert!(pubsub_request_allowed(&origin("tauri://localhost"), &token, &token));
        assert!(pubsub_request_allowed(&origin("http://tauri.localhost"), &token, &token));
        assert!(pubsub_request_allowed(&HeaderMap::new(), &token, &token));
        assert!(!pubsub_request_allowed(&origin("https://evil.example"), &token, &token));
        assert!(!pubsub_request_allowed(&origin("tauri://localhost"), &token, ""));
        assert!(!pubsub_request_allowed(&origin("tauri://localhost"), &token, &"b".repeat(64)));
        assert!(!pubsub_request_allowed(&HeaderMap::new(), "", ""));
    }

    #[test]
    fn falls_back_to_an_available_local_port_when_the_preferred_port_is_in_use() {
        let occupied = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let preferred_port = occupied.local_addr().unwrap().port();

        let listener = bind_pubsub_listener(preferred_port).unwrap();

        assert_eq!(listener.local_addr().unwrap().ip(), Ipv4Addr::LOCALHOST);
        assert_ne!(listener.local_addr().unwrap().port(), preferred_port);
    }

    #[tokio::test]
    async fn shutdown_releases_listener_before_process_exit() {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let state = start_pubsub_server_with_listener(listener, addr, Router::new(), Arc::from("token"));

        state.shutdown(Duration::from_secs(1)).await;

        let rebound = TcpListener::bind(addr).unwrap();
        assert_eq!(rebound.local_addr().unwrap(), addr);
    }
}
