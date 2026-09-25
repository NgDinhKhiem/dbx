use std::sync::Arc;
use std::time::Duration;

use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::SaltString;
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use axum::body::Body;
use axum::extract::State;
use axum::http::header::{CONTENT_TYPE, RETRY_AFTER, SET_COOKIE};
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::security::PeerAddr;
use crate::state::WebState;

/// Request body limit for `/api/auth/*`.
pub const AUTH_BODY_LIMIT_BYTES: usize = 16 * 1024;
/// Upper bound for one `/api/auth/*` request, including Argon2 queueing.
pub const AUTH_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

/// Auth endpoints reachable without a session. Everything else under `/api`,
/// including `auth/change-password`, requires one.
const PUBLIC_AUTH_PATHS: &[&str] = &["auth/login", "auth/check", "auth/setup", "auth/logout"];

#[derive(Deserialize)]
pub struct LoginRequest {
    pub password: String,
}

#[derive(Deserialize)]
pub struct SetupRequest {
    pub password: String,
    #[serde(default)]
    pub setup_token: Option<String>,
}

#[derive(Deserialize)]
pub struct ChangePasswordRequest {
    pub old_password: String,
    pub new_password: String,
}

#[derive(Serialize)]
pub struct AuthCheckResponse {
    pub authenticated: bool,
    pub required: bool,
    pub setup_required: bool,
    /// Setup must include the one-time token printed in the server log
    /// (always false for a browser on the server itself).
    pub setup_token_required: bool,
}

/// Session cancellation token placed in request extensions by
/// [`auth_middleware`], so WebSocket handlers can close with the session.
#[derive(Clone, Default)]
pub struct SessionRevocation(pub Option<CancellationToken>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for SessionRevocation {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut axum::http::request::Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(parts.extensions.get::<SessionRevocation>().cloned().unwrap_or_default())
    }
}

fn session_cookie_path(state: &WebState) -> &str {
    state.public_base_path.as_str()
}

/// Path relative to the `/api` router. Middleware on the nested router sees
/// paths with `/api` (and the public base path) already stripped, so every
/// path it sees is an API path; nothing is treated as a static file.
pub(crate) fn nested_api_path(path: &str) -> &str {
    path.strip_prefix('/').unwrap_or(path)
}

fn json_error(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(serde_json::json!({ "error": message, "code": code }))).into_response()
}

fn rate_limited(retry_after: Duration) -> Response {
    let remaining = retry_after.as_secs().max(1);
    let mut response = (
        StatusCode::TOO_MANY_REQUESTS,
        Json(serde_json::json!({"error": format!("Please try again in {remaining}s")})),
    )
        .into_response();
    if let Ok(value) = HeaderValue::from_str(&remaining.to_string()) {
        response.headers_mut().insert(RETRY_AFTER, value);
    }
    response
}

fn cookie_attributes(state: &WebState, peer: PeerAddr, headers: &HeaderMap) -> String {
    let secure = if state.security.cookie_is_secure(peer.ip(), headers) { "; Secure" } else { "" };
    format!("Path={}; HttpOnly; SameSite=Lax{secure}", session_cookie_path(state))
}

fn session_response(state: &WebState, peer: PeerAddr, headers: &HeaderMap, token: &str) -> Response {
    let max_age = state.sessions.config().max_age.as_secs();
    let cookie = format!("dbx_session={token}; {}; Max-Age={max_age}", cookie_attributes(state, peer, headers));
    (StatusCode::OK, [(SET_COOKIE, cookie)], Json(serde_json::json!({"ok": true}))).into_response()
}

/// Runs Argon2 work on the blocking pool, bounded by `argon2_permits`. The
/// permit is held until the work finishes even if the request is dropped.
async fn run_argon2<T, F>(state: &WebState, work: F) -> Result<T, StatusCode>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let permit = state.argon2_permits.clone().acquire_owned().await.map_err(|_| StatusCode::SERVICE_UNAVAILABLE)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
}

async fn verify_password(state: &WebState, password: String, hash: String) -> Result<bool, StatusCode> {
    run_argon2(state, move || -> Result<bool, StatusCode> {
        let parsed = PasswordHash::new(&hash).map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        Ok(Argon2::default().verify_password(password.as_bytes(), &parsed).is_ok())
    })
    .await?
}

pub async fn hash_password(state: &WebState, password: String) -> Result<String, StatusCode> {
    run_argon2(state, move || -> Result<String, StatusCode> {
        let salt = SaltString::generate(&mut OsRng);
        Argon2::default()
            .hash_password(password.as_bytes(), &salt)
            .map(|hash| hash.to_string())
            .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)
    })
    .await?
}

fn setup_token_matches(expected: Option<&str>, provided: Option<&str>) -> bool {
    match (expected, provided.map(str::trim)) {
        (Some(expected), Some(provided)) if !expected.is_empty() => {
            crate::session::constant_time_eq(expected.as_bytes(), provided.as_bytes())
        }
        _ => false,
    }
}

pub async fn login(
    State(state): State<Arc<WebState>>,
    peer: PeerAddr,
    headers: HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Response, StatusCode> {
    let Some(hash) = state.password_hash.read().await.clone() else {
        return Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response());
    };

    let client = state.security.client_ip(peer.ip(), &headers);
    let attempt = match state.login_rate_limit.try_begin(client) {
        Ok(attempt) => attempt,
        Err(retry_after) => return Ok(rate_limited(retry_after)),
    };

    if !verify_password(&state, body.password, hash).await? {
        attempt.fail();
        return Err(StatusCode::UNAUTHORIZED);
    }
    attempt.succeed();

    let token = state.create_session();
    Ok(session_response(&state, peer, &headers, &token))
}

pub async fn setup(
    State(state): State<Arc<WebState>>,
    peer: PeerAddr,
    headers: HeaderMap,
    Json(body): Json<SetupRequest>,
) -> Result<Response, StatusCode> {
    if state.password_disabled {
        return Err(StatusCode::FORBIDDEN);
    }

    // Only allow setup when no password is configured
    if state.password_hash.read().await.is_some() {
        return Err(StatusCode::FORBIDDEN);
    }

    if body.password.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    // Remote clients must prove access to the server log (setup token); a
    // browser on the server itself may skip it.
    if !state.security.is_local_setup_client(peer.ip(), &headers) {
        let client = state.security.client_ip(peer.ip(), &headers);
        let attempt = match state.login_rate_limit.try_begin(client) {
            Ok(attempt) => attempt,
            Err(retry_after) => return Ok(rate_limited(retry_after)),
        };
        if !setup_token_matches(state.setup_token.as_deref(), body.setup_token.as_deref()) {
            attempt.fail();
            return Ok(json_error(
                StatusCode::FORBIDDEN,
                "SETUP_TOKEN_INVALID",
                "A valid setup token is required. Find the one-time setup token in the DBX server log.",
            ));
        }
        attempt.succeed();
    }

    let hash = hash_password(&state, body.password).await?;
    {
        // Holding the write lock makes concurrent setups race-free.
        let mut password_hash = state.password_hash.write().await;
        if password_hash.is_some() {
            return Err(StatusCode::FORBIDDEN);
        }
        state.app.storage.save_password_hash(&hash).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        *password_hash = Some(hash);
    }

    // Auto-login: create session
    let token = state.create_session();
    Ok(session_response(&state, peer, &headers, &token))
}

pub async fn check(State(state): State<Arc<WebState>>, req: Request<Body>) -> Json<AuthCheckResponse> {
    if state.password_disabled {
        return Json(AuthCheckResponse {
            authenticated: true,
            required: false,
            setup_required: false,
            setup_token_required: false,
        });
    }
    let has_password = state.password_hash.read().await.is_some();
    if !has_password {
        let peer = PeerAddr::from_extensions(req.extensions());
        return Json(AuthCheckResponse {
            authenticated: false,
            required: false,
            setup_required: true,
            setup_token_required: !state.security.is_local_setup_client(peer.ip(), req.headers()),
        });
    }
    let authenticated = match extract_session_token(&req) {
        Some(token) => state.validate_session(&token).is_some(),
        None => false,
    };
    Json(AuthCheckResponse { authenticated, required: true, setup_required: false, setup_token_required: false })
}

pub async fn change_password(
    State(state): State<Arc<WebState>>,
    peer: PeerAddr,
    headers: HeaderMap,
    Json(body): Json<ChangePasswordRequest>,
) -> Result<Response, StatusCode> {
    if state.password_managed_by_env {
        return Ok(json_error(
            StatusCode::CONFLICT,
            "PASSWORD_MANAGED_BY_ENV",
            "The access password is managed by the DBX_PASSWORD environment variable. Change DBX_PASSWORD and restart DBX Web instead.",
        ));
    }

    let Some(hash) = state.password_hash.read().await.clone() else {
        return Err(StatusCode::BAD_REQUEST);
    };

    // The auth middleware already requires a session; re-check so the handler
    // stays safe if it is ever mounted elsewhere.
    let Some(current_session) = session_token_from_headers(&headers) else {
        return Err(StatusCode::UNAUTHORIZED);
    };
    if state.validate_session(&current_session).is_none() {
        return Err(StatusCode::UNAUTHORIZED);
    }

    if body.new_password.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }

    let client = state.security.client_ip(peer.ip(), &headers);
    let attempt = match state.login_rate_limit.try_begin(client) {
        Ok(attempt) => attempt,
        Err(retry_after) => return Ok(rate_limited(retry_after)),
    };
    if !verify_password(&state, body.old_password, hash).await? {
        attempt.fail();
        return Err(StatusCode::UNAUTHORIZED);
    }
    attempt.succeed();

    let new_hash = hash_password(&state, body.new_password).await?;
    {
        let mut password_hash = state.password_hash.write().await;
        state.app.storage.save_password_hash(&new_hash).await.map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
        *password_hash = Some(new_hash);
    }
    // Everyone else must log in again with the new password.
    state.revoke_other_sessions(Some(&current_session));

    Ok((StatusCode::OK, Json(serde_json::json!({"ok": true}))).into_response())
}

pub async fn logout(State(state): State<Arc<WebState>>, peer: PeerAddr, headers: HeaderMap) -> Response {
    if let Some(token) = session_token_from_headers(&headers) {
        // 登出只清除当前登录会话的临时密码，不影响其他会话与桌面端凭据。
        state.revoke_session(&token);
    }
    let cookie = format!("dbx_session=; {}; Max-Age=0", cookie_attributes(&state, peer, &headers));
    (StatusCode::OK, [(SET_COOKIE, cookie)], Json(serde_json::json!({"ok": true}))).into_response()
}

pub fn session_token_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    let cookie_header = headers.get("cookie")?.to_str().ok()?;
    for pair in cookie_header.split(';') {
        let pair = pair.trim();
        if let Some(value) = pair.strip_prefix("dbx_session=") {
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
    }
    None
}

fn extract_session_token<B>(req: &Request<B>) -> Option<String> {
    session_token_from_headers(req.headers())
}

/// Ends Server-Sent Event streams when their session is revoked or expires.
fn end_event_stream_with_session(response: Response, revoked: CancellationToken) -> Response {
    let is_event_stream = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().starts_with("text/event-stream"));
    if !is_event_stream {
        return response;
    }
    let (parts, body) = response.into_parts();
    let stream = body.into_data_stream().take_until(revoked.cancelled_owned());
    Response::from_parts(parts, Body::from_stream(stream))
}

/// Bounds the total time of one `/api/auth/*` request.
pub async fn auth_request_timeout(req: Request<Body>, next: Next) -> Response {
    match tokio::time::timeout(AUTH_REQUEST_TIMEOUT, next.run(req)).await {
        Ok(response) => response,
        Err(_) => json_error(StatusCode::REQUEST_TIMEOUT, "AUTH_REQUEST_TIMEOUT", "Request timed out"),
    }
}

pub async fn auth_middleware(State(state): State<Arc<WebState>>, mut req: Request<Body>, next: Next) -> Response {
    // Only the explicitly public auth endpoints skip the session check. This
    // middleware runs on the nested `/api` router, so every path is an API path.
    if PUBLIC_AUTH_PATHS.contains(&nested_api_path(req.uri().path())) {
        return next.run(req).await;
    }

    if state.password_disabled {
        return next.run(req).await;
    }

    if state.password_hash.read().await.is_none() {
        return StatusCode::UNAUTHORIZED.into_response();
    }

    // Check session token
    let Some(token) = extract_session_token(&req) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    let Some(revoked) = state.validate_session(&token) else {
        return StatusCode::UNAUTHORIZED.into_response();
    };
    req.extensions_mut().insert(SessionRevocation(Some(revoked.clone())));
    // 在下游处理器及其 await 到的池创建路径上注入当前登录会话的 owner 作用域，
    // 使 save_password=false 连接的临时密码按会话隔离（见 SessionCredentialStore）。
    let response =
        dbx_core::session_credentials::with_credential_owner(Some(token), async move { next.run(req).await }).await;
    end_event_stream_with_session(response, revoked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::WebState;
    use axum::routing::{get, post};
    use axum::Router;
    use std::net::SocketAddr;

    #[test]
    fn nested_api_path_treats_every_path_as_api() {
        assert_eq!(nested_api_path("/auth/check"), "auth/check");
        assert_eq!(nested_api_path("/connection/list"), "connection/list");
        // Paths that share a prefix with a public base path are still API paths.
        assert_eq!(nested_api_path("/app-settings/config/decrypt"), "app-settings/config/decrypt");
        assert_eq!(nested_api_path("/query/execute"), "query/execute");
        assert_eq!(nested_api_path("/data-compare/prepare"), "data-compare/prepare");
        assert!(!PUBLIC_AUTH_PATHS.contains(&nested_api_path("/auth/change-password")));
        assert!(!PUBLIC_AUTH_PATHS.contains(&nested_api_path("//auth/login")));
        assert!(!PUBLIC_AUTH_PATHS.contains(&nested_api_path("/auth/login/")));
    }

    #[test]
    fn setup_token_comparison_requires_an_exact_token() {
        assert!(setup_token_matches(Some("abc123"), Some("abc123")));
        assert!(setup_token_matches(Some("abc123"), Some(" abc123 ")));
        assert!(!setup_token_matches(Some("abc123"), Some("abc124")));
        assert!(!setup_token_matches(Some("abc123"), None));
        assert!(!setup_token_matches(None, Some("")));
        assert!(!setup_token_matches(Some(""), Some("")));
    }

    async fn test_state(configure: impl FnOnce(&mut WebState)) -> (Arc<WebState>, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let storage = dbx_core::storage::Storage::open_unmigrated(&directory.path().join("dbx.db")).await.unwrap();
        let app = Arc::new(dbx_core::connection::AppState::new(storage));
        let mut state = WebState::for_tests(app, directory.path().to_path_buf());
        configure(&mut state);
        (Arc::new(state), directory)
    }

    /// Cheap Argon2 parameters keep debug-build tests fast; verification
    /// reads the parameters from the hash string.
    fn hash(password: &str) -> String {
        let salt = SaltString::generate(&mut OsRng);
        let params = argon2::Params::new(8, 1, 1, None).unwrap();
        Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params)
            .hash_password(password.as_bytes(), &salt)
            .unwrap()
            .to_string()
    }

    fn auth_router(state: Arc<WebState>) -> Router {
        let api = Router::new()
            .route("/auth/login", post(login))
            .route("/auth/setup", post(setup))
            .route("/auth/check", get(check))
            .route("/auth/change-password", post(change_password))
            .route("/auth/logout", post(logout))
            .route("/connection/list", get(|| async { "connections" }))
            .layer(axum::middleware::from_fn_with_state(state.clone(), auth_middleware))
            .with_state(state);
        Router::new().nest("/api", api)
    }

    async fn serve(router: Router) -> (SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).await.unwrap();
        });
        (address, server)
    }

    fn session_cookie(response: &reqwest::Response) -> String {
        let value = response.headers().get(reqwest::header::SET_COOKIE).unwrap().to_str().unwrap();
        assert!(value.contains("HttpOnly"));
        assert!(value.contains("SameSite=Lax"));
        assert!(value.contains("Max-Age="));
        value.split(';').next().unwrap().to_string()
    }

    #[tokio::test]
    async fn change_password_requires_session_and_revokes_other_sessions() {
        let (state, _directory) = test_state(|state| *state.password_hash.get_mut() = Some(hash("old-secret"))).await;
        let (address, server) = serve(auth_router(state.clone())).await;
        let client = reqwest::Client::new();
        let url = |path: &str| format!("http://{address}/api{path}");
        let change = serde_json::json!({"old_password": "old-secret", "new_password": "new-secret"});

        let anonymous = client.post(url("/auth/change-password")).json(&change).send().await.unwrap();
        assert_eq!(anonymous.status(), reqwest::StatusCode::UNAUTHORIZED);

        let login_body = serde_json::json!({"password": "old-secret"});
        let first = session_cookie(&client.post(url("/auth/login")).json(&login_body).send().await.unwrap());
        let second = session_cookie(&client.post(url("/auth/login")).json(&login_body).send().await.unwrap());

        let wrong = client
            .post(url("/auth/change-password"))
            .header("cookie", &first)
            .json(&serde_json::json!({"old_password": "nope", "new_password": "x"}))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), reqwest::StatusCode::UNAUTHORIZED);

        let changed =
            client.post(url("/auth/change-password")).header("cookie", &first).json(&change).send().await.unwrap();
        assert_eq!(changed.status(), reqwest::StatusCode::OK);

        let current = client.get(url("/connection/list")).header("cookie", &first).send().await.unwrap();
        assert_eq!(current.status(), reqwest::StatusCode::OK);
        let revoked = client.get(url("/connection/list")).header("cookie", &second).send().await.unwrap();
        assert_eq!(revoked.status(), reqwest::StatusCode::UNAUTHORIZED);

        let old_login = client.post(url("/auth/login")).json(&login_body).send().await.unwrap();
        assert_eq!(old_login.status(), reqwest::StatusCode::UNAUTHORIZED);
        let new_login =
            client.post(url("/auth/login")).json(&serde_json::json!({"password": "new-secret"})).send().await.unwrap();
        assert_eq!(new_login.status(), reqwest::StatusCode::OK);
        server.abort();
    }

    #[tokio::test]
    async fn change_password_is_rejected_when_managed_by_env() {
        let (state, _directory) = test_state(|state| {
            *state.password_hash.get_mut() = Some(hash("env-secret"));
            state.password_managed_by_env = true;
        })
        .await;
        let (address, server) = serve(auth_router(state)).await;
        let client = reqwest::Client::new();
        let cookie = session_cookie(
            &client
                .post(format!("http://{address}/api/auth/login"))
                .json(&serde_json::json!({"password": "env-secret"}))
                .send()
                .await
                .unwrap(),
        );
        let response = client
            .post(format!("http://{address}/api/auth/change-password"))
            .header("cookie", cookie)
            .json(&serde_json::json!({"old_password": "env-secret", "new_password": "other"}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::CONFLICT);
        let body = response.json::<serde_json::Value>().await.unwrap();
        assert_eq!(body["code"], "PASSWORD_MANAGED_BY_ENV");
        server.abort();
    }

    #[tokio::test]
    async fn login_is_rate_limited_per_client() {
        let (state, _directory) = test_state(|state| *state.password_hash.get_mut() = Some(hash("secret"))).await;
        let (address, server) = serve(auth_router(state)).await;
        let client = reqwest::Client::new();
        for _ in 0..crate::rate_limit::MAX_FAILURES {
            let response = client
                .post(format!("http://{address}/api/auth/login"))
                .json(&serde_json::json!({"password": "wrong"}))
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::UNAUTHORIZED);
        }
        let locked = client
            .post(format!("http://{address}/api/auth/login"))
            .json(&serde_json::json!({"password": "secret"}))
            .send()
            .await
            .unwrap();
        assert_eq!(locked.status(), reqwest::StatusCode::TOO_MANY_REQUESTS);
        assert!(locked.headers().contains_key(reqwest::header::RETRY_AFTER));
        let body = locked.json::<serde_json::Value>().await.unwrap();
        assert!(body["error"].as_str().unwrap().starts_with("Please try again in "));
        server.abort();
    }

    #[tokio::test]
    async fn remote_setup_requires_the_setup_token_but_local_setup_does_not() {
        let (state, _directory) = test_state(|state| state.setup_token = Some("token-123".to_string())).await;
        let (address, server) = serve(auth_router(state.clone())).await;
        let client = reqwest::Client::new();
        let url = format!("http://{address}/api/auth/setup");

        // Looks remote: forwarded by an (untrusted) proxy.
        let check = client
            .get(format!("http://{address}/api/auth/check"))
            .header("x-forwarded-for", "203.0.113.4")
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(check["setup_required"], true);
        assert_eq!(check["setup_token_required"], true);

        let missing = client
            .post(&url)
            .header("x-forwarded-for", "203.0.113.4")
            .json(&serde_json::json!({"password": "secret"}))
            .send()
            .await
            .unwrap();
        assert_eq!(missing.status(), reqwest::StatusCode::FORBIDDEN);
        assert_eq!(missing.json::<serde_json::Value>().await.unwrap()["code"], "SETUP_TOKEN_INVALID");
        assert!(state.password_hash.read().await.is_none());

        let accepted = client
            .post(&url)
            .header("x-forwarded-for", "203.0.113.4")
            .json(&serde_json::json!({"password": "secret", "setup_token": "token-123"}))
            .send()
            .await
            .unwrap();
        assert_eq!(accepted.status(), reqwest::StatusCode::OK);
        session_cookie(&accepted);
        assert!(state.password_hash.read().await.is_some());

        // Setup is one-shot.
        let again = client.post(&url).json(&serde_json::json!({"password": "other"})).send().await.unwrap();
        assert_eq!(again.status(), reqwest::StatusCode::FORBIDDEN);
        server.abort();

        let (local_state, _local_directory) =
            test_state(|state| state.setup_token = Some("token-456".to_string())).await;
        let (address, server) = serve(auth_router(local_state.clone())).await;
        // reqwest sends `Host: 127.0.0.1:<port>` from a loopback peer.
        let check = client
            .get(format!("http://{address}/api/auth/check"))
            .send()
            .await
            .unwrap()
            .json::<serde_json::Value>()
            .await
            .unwrap();
        assert_eq!(check["setup_token_required"], false);
        let local = client
            .post(format!("http://{address}/api/auth/setup"))
            .json(&serde_json::json!({"password": "secret"}))
            .send()
            .await
            .unwrap();
        assert_eq!(local.status(), reqwest::StatusCode::OK);
        server.abort();
    }

    #[tokio::test]
    async fn logout_revokes_the_session_and_clears_the_cookie() {
        let (state, _directory) = test_state(|state| *state.password_hash.get_mut() = Some(hash("secret"))).await;
        let (address, server) = serve(auth_router(state.clone())).await;
        let client = reqwest::Client::new();
        let cookie = session_cookie(
            &client
                .post(format!("http://{address}/api/auth/login"))
                .json(&serde_json::json!({"password": "secret"}))
                .send()
                .await
                .unwrap(),
        );
        let logout =
            client.post(format!("http://{address}/api/auth/logout")).header("cookie", &cookie).send().await.unwrap();
        let cleared = logout.headers().get(reqwest::header::SET_COOKIE).unwrap().to_str().unwrap().to_string();
        assert!(cleared.contains("Max-Age=0"));
        assert!(cleared.contains("SameSite=Lax"));
        let after =
            client.get(format!("http://{address}/api/connection/list")).header("cookie", &cookie).send().await.unwrap();
        assert_eq!(after.status(), reqwest::StatusCode::UNAUTHORIZED);
        server.abort();
    }

    #[tokio::test]
    async fn event_streams_end_when_the_session_is_revoked() {
        let revoked = CancellationToken::new();
        let response = Response::builder()
            .header(CONTENT_TYPE, "text/event-stream")
            .body(Body::from_stream(futures::stream::pending::<Result<axum::body::Bytes, std::io::Error>>()))
            .unwrap();
        let response = end_event_stream_with_session(response, revoked.clone());
        revoked.cancel();
        let collected =
            tokio::time::timeout(Duration::from_secs(5), axum::body::to_bytes(response.into_body(), usize::MAX))
                .await
                .expect("stream should end after revocation")
                .unwrap();
        assert!(collected.is_empty());
    }
}
