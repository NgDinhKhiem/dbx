use bytes::Bytes;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, StatusCode, Url};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::{timeout, Duration};
use uuid::Uuid;

const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const TCP_READ_BUFFER_SIZE: usize = 16 * 1024;
const HTTP_READ_WAIT_MS: u64 = 1_000;
const HTTP_REQUEST_TIMEOUT: Duration = Duration::from_secs(15);
/// Tunnel URL query parameter that explicitly allows a plain `http://` script
/// on a non-loopback host (e.g. `http://10.0.0.5/dbx_tunnel.php?allow_insecure_http=1`).
/// It is consumed by DBX and not forwarded to the script.
const ALLOW_INSECURE_HTTP_PARAM: &str = "allow_insecure_http";

#[derive(Default)]
pub struct HttpTunnelManager {
    tunnels: Mutex<HashMap<String, (JoinHandle<()>, u16)>>,
}

impl HttpTunnelManager {
    pub fn new() -> Self {
        Self { tunnels: Mutex::new(HashMap::new()) }
    }

    pub async fn start_tunnel(
        &self,
        connection_id: &str,
        tunnel_url: &str,
        token: &str,
        connect_timeout_secs: u64,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<u16, String> {
        if let Some(local_port) = self.local_port(connection_id).await {
            return Ok(local_port);
        }
        let local_port = portpicker::pick_unused_port().ok_or("No available port")?;
        let listener = TcpListener::bind(("127.0.0.1", local_port))
            .await
            .map_err(|e| format!("Failed to bind HTTP tunnel local port: {e}"))?;

        let endpoint =
            Arc::new(HttpTunnelEndpoint::new(tunnel_url, token, connect_timeout_secs, remote_host, remote_port)?);
        let handle = tokio::spawn(http_tunnel_forward_loop(listener, endpoint));

        let mut tunnels = self.tunnels.lock().await;
        if let Some((_, existing_port)) = tunnels.get(connection_id) {
            handle.abort();
            return Ok(*existing_port);
        }

        tunnels.insert(connection_id.to_string(), (handle, local_port));
        Ok(local_port)
    }

    pub async fn local_port(&self, connection_id: &str) -> Option<u16> {
        self.tunnels.lock().await.get(connection_id).map(|(_, port)| *port)
    }

    pub async fn stop_tunnel(&self, connection_id: &str) {
        if let Some((handle, _)) = self.tunnels.lock().await.remove(connection_id) {
            handle.abort();
        }
    }

    pub async fn stop_tunnels_with_prefix(&self, connection_id_prefix: &str) {
        let mut tunnels = self.tunnels.lock().await;
        let keys: Vec<String> = tunnels.keys().filter(|key| key.starts_with(connection_id_prefix)).cloned().collect();
        for key in keys {
            if let Some((handle, _)) = tunnels.remove(&key) {
                handle.abort();
            }
        }
    }

    pub async fn stop_all_tunnels(&self) {
        let tunnels = std::mem::take(&mut *self.tunnels.lock().await);
        for (handle, _) in tunnels.into_values() {
            handle.abort();
        }
    }
}

struct HttpTunnelEndpoint {
    url: String,
    token: String,
    connect_timeout: Duration,
    target_host: String,
    target_port: u16,
    client: Client,
}

impl HttpTunnelEndpoint {
    fn new(
        tunnel_url: &str,
        token: &str,
        connect_timeout_secs: u64,
        remote_host: &str,
        remote_port: u16,
    ) -> Result<Self, String> {
        validate_script_url(tunnel_url)?;
        Ok(Self {
            url: tunnel_url.trim().to_string(),
            token: token.trim().to_string(),
            connect_timeout: effective_connect_timeout(connect_timeout_secs),
            target_host: remote_host.to_string(),
            target_port: remote_port,
            client: Client::new(),
        })
    }
}

async fn http_tunnel_forward_loop(listener: TcpListener, endpoint: Arc<HttpTunnelEndpoint>) {
    // Bridge tasks (including their HTTP read polling) are owned by this
    // JoinSet: `stop_tunnel` aborts this task, dropping the set aborts them.
    let mut bridges = JoinSet::new();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((inbound, _)) = accepted else {
                    break;
                };
                let endpoint = endpoint.clone();
                bridges.spawn(async move {
                    let _ = bridge_tcp_to_http_script(inbound, endpoint).await;
                });
            }
            Some(_) = bridges.join_next(), if !bridges.is_empty() => {}
        }
    }
    // A listener failure stops new connections but keeps established ones.
    while bridges.join_next().await.is_some() {}
}

/// Closes the script-side session (best effort) when a bridge is aborted by
/// `stop_tunnel` before it could close the session itself.
struct AbortedSessionCloser {
    endpoint: Arc<HttpTunnelEndpoint>,
    session: String,
    armed: bool,
}

impl Drop for AbortedSessionCloser {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let endpoint = self.endpoint.clone();
        let session = std::mem::take(&mut self.session);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = close_session(&endpoint, &session).await;
            });
        }
    }
}

async fn bridge_tcp_to_http_script(mut inbound: TcpStream, endpoint: Arc<HttpTunnelEndpoint>) -> Result<(), String> {
    let session = Uuid::new_v4().simple().to_string();
    open_session(&endpoint, &session).await?;
    let mut closer = AbortedSessionCloser { endpoint: endpoint.clone(), session: session.clone(), armed: true };

    let (mut tcp_reader, mut tcp_writer) = inbound.split();
    let tcp_to_http = async {
        let mut buf = vec![0_u8; TCP_READ_BUFFER_SIZE];
        loop {
            let n = tcp_reader.read(&mut buf).await.map_err(|e| format!("Failed to read local TCP stream: {e}"))?;
            if n == 0 {
                return Ok::<(), String>(());
            }
            write_session(&endpoint, &session, Bytes::copy_from_slice(&buf[..n])).await?;
        }
    };

    let http_to_tcp = async {
        loop {
            let bytes = read_session(&endpoint, &session).await?;
            if !bytes.is_empty() {
                tcp_writer.write_all(&bytes).await.map_err(|e| format!("Failed to write local TCP stream: {e}"))?;
            }
        }
    };

    let result = tokio::select! {
        result = tcp_to_http => result,
        result = http_to_tcp => result,
    };
    closer.armed = false;
    let _ = close_session(&endpoint, &session).await;
    result
}

async fn open_session(endpoint: &HttpTunnelEndpoint, session: &str) -> Result<(), String> {
    let url = script_url(
        &endpoint.url,
        "open",
        session,
        Some((&endpoint.target_host, endpoint.target_port)),
        Some(endpoint.connect_timeout.as_secs()),
        None,
    )?;
    let response = timeout(
        endpoint.connect_timeout + HTTP_REQUEST_TIMEOUT,
        authorized_request(endpoint, endpoint.client.post(url)).send(),
    )
    .await
    .map_err(|_| "HTTP tunnel script open timed out".to_string())?
    .map_err(|e| format!("Failed to open HTTP tunnel script session: {e}"))?;
    ensure_success(response, "open HTTP tunnel script session").await
}

async fn write_session(endpoint: &HttpTunnelEndpoint, session: &str, data: Bytes) -> Result<(), String> {
    let url = script_url(&endpoint.url, "write", session, None, None, None)?;
    let response = timeout(
        HTTP_REQUEST_TIMEOUT,
        authorized_request(endpoint, endpoint.client.post(url))
            .header(CONTENT_TYPE, "application/octet-stream")
            .body(data)
            .send(),
    )
    .await
    .map_err(|_| "HTTP tunnel script write timed out".to_string())?
    .map_err(|e| format!("Failed to write HTTP tunnel script session: {e}"))?;
    ensure_success(response, "write HTTP tunnel script session").await
}

async fn read_session(endpoint: &HttpTunnelEndpoint, session: &str) -> Result<Bytes, String> {
    let url = script_url(&endpoint.url, "read", session, None, None, Some(HTTP_READ_WAIT_MS))?;
    let response = timeout(HTTP_REQUEST_TIMEOUT, authorized_request(endpoint, endpoint.client.post(url)).send())
        .await
        .map_err(|_| "HTTP tunnel script read timed out".to_string())?
        .map_err(|e| format!("Failed to read HTTP tunnel script session: {e}"))?;
    let status = response.status();
    if status == StatusCode::NO_CONTENT {
        return Ok(Bytes::new());
    }
    if status == StatusCode::OK {
        return response.bytes().await.map_err(|e| format!("Failed to read HTTP tunnel script response: {e}"));
    }
    if status == StatusCode::GONE {
        return Err("HTTP tunnel script session closed".to_string());
    }
    Err(error_response_message(response, "read HTTP tunnel script session").await)
}

async fn close_session(endpoint: &HttpTunnelEndpoint, session: &str) -> Result<(), String> {
    let url = script_url(&endpoint.url, "close", session, None, None, None)?;
    let response = timeout(HTTP_REQUEST_TIMEOUT, authorized_request(endpoint, endpoint.client.post(url)).send())
        .await
        .map_err(|_| "HTTP tunnel script close timed out".to_string())?
        .map_err(|e| format!("Failed to close HTTP tunnel script session: {e}"))?;
    ensure_success(response, "close HTTP tunnel script session").await
}

fn authorized_request(endpoint: &HttpTunnelEndpoint, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    if endpoint.token.is_empty() {
        return builder;
    }
    match HeaderValue::from_str(&endpoint.token) {
        Ok(token) => builder
            .header("X-DBX-Tunnel-Token", token.clone())
            .header(AUTHORIZATION, format!("Bearer {}", endpoint.token)),
        Err(_) => builder,
    }
}

async fn ensure_success(response: reqwest::Response, action: &str) -> Result<(), String> {
    if response.status().is_success() {
        Ok(())
    } else {
        Err(error_response_message(response, action).await)
    }
}

async fn error_response_message(response: reqwest::Response, action: &str) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    if body.trim().is_empty() {
        format!("Failed to {action}: HTTP {status}")
    } else {
        format!("Failed to {action}: HTTP {status}: {}", body.trim())
    }
}

fn script_url(
    tunnel_url: &str,
    action: &str,
    session: &str,
    target: Option<(&str, u16)>,
    connect_timeout_secs: Option<u64>,
    wait_ms: Option<u64>,
) -> Result<Url, String> {
    let mut url = validate_script_url(tunnel_url)?;
    strip_insecure_http_opt_in(&mut url);
    url.query_pairs_mut().append_pair("dbx_action", action).append_pair("dbx_session", session);
    if let Some((host, port)) = target {
        url.query_pairs_mut().append_pair("dbx_target_host", host).append_pair("dbx_target_port", &port.to_string());
    }
    if let Some(connect_timeout_secs) = connect_timeout_secs {
        url.query_pairs_mut()
            .append_pair("dbx_connect_timeout", &effective_connect_timeout_secs(connect_timeout_secs).to_string());
    }
    if let Some(wait_ms) = wait_ms {
        url.query_pairs_mut().append_pair("dbx_wait_ms", &wait_ms.to_string());
    }
    Ok(url)
}

fn validate_script_url(tunnel_url: &str) -> Result<Url, String> {
    let trimmed = tunnel_url.trim();
    if trimmed.is_empty() {
        return Err("HTTP tunnel script URL is required".to_string());
    }
    let url = Url::parse(trimmed).map_err(|e| format!("HTTP tunnel script URL is invalid: {e}"))?;
    match url.scheme() {
        "https" => Ok(url),
        // Plain HTTP sends the tunnel token and the database wire protocol in
        // cleartext, so it is only accepted for loopback or with an explicit opt-in.
        "http" if is_loopback_url(&url) || insecure_http_opted_in(&url) => Ok(url),
        "http" => Err(format!(
            "HTTP tunnel script URL uses plain http:// for a non-local host, which would send the tunnel token and all database traffic unencrypted. \
             Use an https:// URL, or append ?{ALLOW_INSECURE_HTTP_PARAM}=1 to the tunnel URL to allow plain HTTP explicitly (only on a trusted network)."
        )),
        other => Err(format!("Unsupported HTTP tunnel script URL scheme: {other}")),
    }
}

fn is_loopback_url(url: &Url) -> bool {
    let Some(host) = url.host_str() else {
        return false;
    };
    let host = host.trim_start_matches('[').trim_end_matches(']');
    host.eq_ignore_ascii_case("localhost")
        || host.to_ascii_lowercase().ends_with(".localhost")
        || host.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

fn insecure_http_opted_in(url: &Url) -> bool {
    url.query_pairs().any(|(key, value)| {
        key == ALLOW_INSECURE_HTTP_PARAM && matches!(value.to_ascii_lowercase().as_str(), "1" | "true" | "yes")
    })
}

fn strip_insecure_http_opt_in(url: &mut Url) {
    if !url.query_pairs().any(|(key, _)| key == ALLOW_INSECURE_HTTP_PARAM) {
        return;
    }
    let kept = url
        .query_pairs()
        .filter(|(key, _)| key != ALLOW_INSECURE_HTTP_PARAM)
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    url.set_query(None);
    if !kept.is_empty() {
        url.query_pairs_mut().extend_pairs(kept);
    }
}

fn effective_connect_timeout(value: u64) -> Duration {
    Duration::from_secs(effective_connect_timeout_secs(value))
}

fn effective_connect_timeout_secs(value: u64) -> u64 {
    if value == 0 {
        DEFAULT_CONNECT_TIMEOUT.as_secs()
    } else {
        value.clamp(1, 300)
    }
}

#[cfg(test)]
mod tests {
    use super::{script_url, validate_script_url, HttpTunnelManager};
    use std::sync::Arc;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{TcpListener, TcpStream};
    use tokio::sync::{Mutex, Notify};
    use tokio::time::{timeout, Duration};

    #[test]
    fn script_url_preserves_existing_query_and_appends_action() {
        let url = script_url(
            "https://gateway.example.com/dbx_tunnel.php?keep=1",
            "open",
            "session-1",
            Some(("mysql.internal", 3306)),
            Some(10),
            None,
        )
        .unwrap();

        assert_eq!(
            url.as_str(),
            "https://gateway.example.com/dbx_tunnel.php?keep=1&dbx_action=open&dbx_session=session-1&dbx_target_host=mysql.internal&dbx_target_port=3306&dbx_connect_timeout=10"
        );
    }

    #[test]
    fn plain_http_is_rejected_for_remote_hosts_without_an_explicit_opt_in() {
        let err = validate_script_url("http://gateway.example.com/dbx_tunnel.php").unwrap_err();
        assert!(err.contains("unencrypted") && err.contains("allow_insecure_http=1"), "{err}");
        assert!(validate_script_url("http://10.0.0.5/dbx_tunnel.php?allow_insecure_http=0").is_err());

        assert!(validate_script_url("https://gateway.example.com/dbx_tunnel.php").is_ok());
        assert!(validate_script_url("http://127.0.0.1:8080/dbx_tunnel.php").is_ok());
        assert!(validate_script_url("http://[::1]/dbx_tunnel.php").is_ok());
        assert!(validate_script_url("http://localhost/dbx_tunnel.php").is_ok());
        assert!(validate_script_url("http://10.0.0.5/dbx_tunnel.php?allow_insecure_http=1").is_ok());
    }

    #[test]
    fn insecure_http_opt_in_is_not_forwarded_to_the_script() {
        let url = script_url(
            "http://10.0.0.5/dbx_tunnel.php?keep=1&allow_insecure_http=1",
            "read",
            "session-1",
            None,
            None,
            None,
        )
        .unwrap();
        assert_eq!(url.as_str(), "http://10.0.0.5/dbx_tunnel.php?keep=1&dbx_action=read&dbx_session=session-1");
    }

    #[test]
    fn script_url_rejects_non_http_schemes() {
        let err = validate_script_url("file:///tmp/tunnel.php").unwrap_err();

        assert!(err.contains("Unsupported HTTP tunnel script URL scheme"));
    }

    #[tokio::test]
    async fn manager_forwards_sequential_round_trips_and_closes_session() {
        let script = MockScript::start().await;
        let manager = HttpTunnelManager::new();
        let local_port = manager.start_tunnel("test", &script.url, "secret", 5, "mysql.internal", 3306).await.unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", local_port)).await.unwrap();
        let close_notification = script.closed.notified();

        timeout(Duration::from_secs(10), async {
            for index in 0..100_u32 {
                let payload = format!("request-{index:03}-{}", "x".repeat((index % 17) as usize));
                client.write_all(payload.as_bytes()).await.unwrap();
                let mut response = vec![0_u8; payload.len()];
                client.read_exact(&mut response).await.unwrap();

                assert_eq!(response, payload.as_bytes());
            }
        })
        .await
        .expect("100 sequential HTTP tunnel round trips timed out");

        drop(client);
        timeout(Duration::from_secs(2), close_notification).await.expect("HTTP tunnel session was not closed");

        manager.stop_tunnel("test").await;
        script.handle.abort();
    }

    #[tokio::test]
    async fn stop_tunnel_aborts_established_bridges_and_closes_their_sessions() {
        let script = MockScript::start().await;
        let manager = HttpTunnelManager::new();
        let local_port = manager.start_tunnel("test", &script.url, "secret", 5, "mysql.internal", 3306).await.unwrap();
        let mut client = TcpStream::connect(("127.0.0.1", local_port)).await.unwrap();
        client.write_all(b"ping").await.unwrap();
        let mut response = [0_u8; 4];
        timeout(Duration::from_secs(5), client.read_exact(&mut response)).await.unwrap().unwrap();
        assert_eq!(&response, b"ping");

        let close_notification = script.closed.notified();
        manager.stop_tunnel("test").await;

        let mut buf = [0_u8; 1];
        let read = timeout(Duration::from_secs(2), client.read(&mut buf))
            .await
            .expect("the bridge must be torn down when the tunnel stops");
        assert!(matches!(read, Ok(0) | Err(_)), "{read:?}");
        timeout(Duration::from_secs(2), close_notification).await.expect("aborted bridge should close its session");
        script.handle.abort();
    }

    struct MockScript {
        url: String,
        closed: Arc<Notify>,
        handle: tokio::task::JoinHandle<()>,
    }

    impl MockScript {
        async fn start() -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
            let addr = listener.local_addr().unwrap();
            let buffered = Arc::new(Mutex::new(Vec::<u8>::new()));
            let closed = Arc::new(Notify::new());
            let handle = {
                let buffered = buffered.clone();
                let closed = closed.clone();
                tokio::spawn(async move {
                    loop {
                        let Ok((stream, _)) = listener.accept().await else {
                            break;
                        };
                        let buffered = buffered.clone();
                        let closed = closed.clone();
                        tokio::spawn(async move {
                            handle_mock_http_request(stream, buffered, closed).await;
                        });
                    }
                })
            };
            Self { url: format!("http://{addr}/dbx_tunnel.php"), closed, handle }
        }
    }

    async fn handle_mock_http_request(stream: TcpStream, buffered: Arc<Mutex<Vec<u8>>>, closed: Arc<Notify>) {
        let mut reader = BufReader::new(stream);
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await.unwrap();
        let mut content_length = 0_usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).await.unwrap();
            let trimmed = line.trim_end();
            if trimmed.is_empty() {
                break;
            }
            let lower = trimmed.to_ascii_lowercase();
            if let Some(value) = lower.strip_prefix("content-length:") {
                content_length = value.trim().parse().unwrap();
            }
        }
        let mut body = vec![0_u8; content_length];
        reader.read_exact(&mut body).await.unwrap();
        let mut stream = reader.into_inner();
        if request_line.contains("dbx_action=write") {
            buffered.lock().await.extend_from_slice(&body);
            write_http_response(&mut stream, "200 OK", b"OK").await;
        } else if request_line.contains("dbx_action=read") {
            let bytes = {
                let mut buffered = buffered.lock().await;
                std::mem::take(&mut *buffered)
            };
            if bytes.is_empty() {
                write_http_response(&mut stream, "204 No Content", b"").await;
            } else {
                write_http_response(&mut stream, "200 OK", &bytes).await;
            }
        } else if request_line.contains("dbx_action=close") {
            write_http_response(&mut stream, "200 OK", b"OK").await;
            closed.notify_one();
        } else {
            write_http_response(&mut stream, "200 OK", b"OK").await;
        }
    }

    async fn write_http_response(stream: &mut TcpStream, status: &str, body: &[u8]) {
        let header = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len());
        stream.write_all(header.as_bytes()).await.unwrap();
        stream.write_all(body).await.unwrap();
    }
}
