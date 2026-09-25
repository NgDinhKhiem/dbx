//! HTTP hardening for DBX Web: trusted proxies and client IPs, cookie `Secure`
//! policy, Host allowlist (DNS rebinding), WebSocket Origin checks and
//! security response headers (including the SPA Content-Security-Policy).
//!
//! Environment variables:
//! - `DBX_TRUSTED_PROXIES`: comma-separated IPs/CIDRs whose `X-Forwarded-For`,
//!   `X-Real-IP`, `X-Forwarded-Proto` and `X-Forwarded-Host` headers are trusted.
//! - `DBX_COOKIE_SECURE`: `auto` (default), `true` or `false`.
//! - `DBX_ALLOWED_HOSTS`: comma-separated host names accepted on `/api`
//!   (`*.example.com` / `.example.com` match subdomains, `*` disables the check).
//! - `DBX_PUBLIC_ORIGIN`: comma-separated browser origins accepted for WebSockets
//!   in addition to the request's own host.
//! - `DBX_CSP`: overrides the SPA Content-Security-Policy; empty disables it.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::header::{
    CONTENT_SECURITY_POLICY, CONTENT_TYPE, HOST, ORIGIN, REFERRER_POLICY, X_CONTENT_TYPE_OPTIONS, X_FRAME_OPTIONS,
};
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, Request, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;

use crate::state::WebState;

/// Maps IPv4-mapped IPv6 addresses (`::ffff:a.b.c.d`) to IPv4.
pub fn canonical_ip(ip: IpAddr) -> IpAddr {
    match ip {
        IpAddr::V6(v6) => v6.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(v6)),
        v4 => v4,
    }
}

/// The TCP peer address, when the server was started with connect info.
#[derive(Clone, Copy, Debug, Default)]
pub struct PeerAddr(pub Option<SocketAddr>);

impl PeerAddr {
    pub fn ip(&self) -> Option<IpAddr> {
        self.0.map(|address| canonical_ip(address.ip()))
    }

    pub fn from_extensions(extensions: &axum::http::Extensions) -> Self {
        Self(extensions.get::<ConnectInfo<SocketAddr>>().map(|info| info.0))
    }
}

impl<S: Send + Sync> FromRequestParts<S> for PeerAddr {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self::from_extensions(&parts.extensions))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct IpNet {
    address: IpAddr,
    prefix: u8,
}

impl IpNet {
    fn parse(value: &str) -> Result<Self, String> {
        let value = value.trim();
        let (address, prefix) = match value.split_once('/') {
            Some((address, prefix)) => (address, Some(prefix)),
            None => (value, None),
        };
        let address = canonical_ip(
            address
                .trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .map_err(|_| format!("invalid IP address in DBX_TRUSTED_PROXIES: {value}"))?,
        );
        let max = if address.is_ipv4() { 32 } else { 128 };
        let prefix = match prefix {
            Some(prefix) => prefix
                .parse::<u8>()
                .ok()
                .filter(|prefix| *prefix <= max)
                .ok_or_else(|| format!("invalid prefix length in DBX_TRUSTED_PROXIES: {value}"))?,
            None => max,
        };
        Ok(Self { address, prefix })
    }

    fn contains(&self, ip: IpAddr) -> bool {
        match (self.address, canonical_ip(ip)) {
            (IpAddr::V4(network), IpAddr::V4(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u32::MAX << (32 - u32::from(self.prefix)) };
                u32::from(network) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(network), IpAddr::V6(ip)) => {
                let mask = if self.prefix == 0 { 0 } else { u128::MAX << (128 - u32::from(self.prefix)) };
                u128::from(network) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct TrustedProxies(Vec<IpNet>);

impl TrustedProxies {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        let Some(value) = value else {
            return Ok(Self::default());
        };
        value
            .split(',')
            .map(str::trim)
            .filter(|entry| !entry.is_empty())
            .map(IpNet::parse)
            .collect::<Result<Vec<_>, _>>()
            .map(Self)
    }

    pub fn contains(&self, ip: IpAddr) -> bool {
        self.0.iter().any(|network| network.contains(ip))
    }

    fn trusts(&self, peer: Option<IpAddr>) -> bool {
        peer.is_some_and(|peer| self.contains(peer))
    }
}

fn parse_forwarded_ip(value: &str) -> Option<IpAddr> {
    let value = value.trim().trim_matches('"');
    if let Ok(ip) = value.parse::<IpAddr>() {
        return Some(canonical_ip(ip));
    }
    if let Ok(address) = value.parse::<SocketAddr>() {
        return Some(canonical_ip(address.ip()));
    }
    value.trim_matches(['[', ']']).parse::<IpAddr>().ok().map(canonical_ip)
}

fn first_header_value<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers.get(name)?.to_str().ok()?.split(',').next().map(str::trim).filter(|value| !value.is_empty())
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum CookieSecure {
    #[default]
    Auto,
    Always,
    Never,
}

impl CookieSecure {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value.map(|value| value.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("auto") => Ok(Self::Auto),
            Some("1" | "true" | "yes" | "on") => Ok(Self::Always),
            Some("0" | "false" | "no" | "off") => Ok(Self::Never),
            Some(other) => Err(format!("DBX_COOKIE_SECURE must be auto, true or false (got {other})")),
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HostPolicy {
    enforce: bool,
    allowed: Vec<String>,
}

impl HostPolicy {
    /// Without `DBX_ALLOWED_HOSTS` the check only runs when password auth is
    /// disabled; `*` turns it off; any other value always enforces it.
    pub fn from_value(value: Option<&str>, password_disabled: bool) -> Self {
        let entries = value
            .map(|value| {
                value
                    .split(',')
                    .map(|entry| entry.trim().trim_end_matches('.').to_ascii_lowercase())
                    .filter(|entry| !entry.is_empty())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if entries.iter().any(|entry| entry == "*") {
            return Self { enforce: false, allowed: Vec::new() };
        }
        Self { enforce: password_disabled || !entries.is_empty(), allowed: entries }
    }

    pub fn is_enforced(&self) -> bool {
        self.enforce
    }

    pub fn allows(&self, host_header: Option<&str>) -> bool {
        if !self.enforce {
            return true;
        }
        let Some(host) = host_header.and_then(host_without_port) else {
            return false;
        };
        is_ip_literal_or_localhost(&host)
            || self.allowed.iter().any(|entry| {
                if let Some(suffix) = entry.strip_prefix("*.").or_else(|| entry.strip_prefix('.')) {
                    host.ends_with(&format!(".{suffix}"))
                } else {
                    host == *entry
                }
            })
    }
}

/// Lower-cased host name from a `Host`-style `host[:port]` value.
pub fn host_without_port(value: &str) -> Option<String> {
    let value = value.trim();
    if value.is_empty() {
        return None;
    }
    let host = if let Some(rest) = value.strip_prefix('[') {
        rest.split_once(']')?.0
    } else if value.parse::<IpAddr>().is_ok() {
        value
    } else {
        match value.rsplit_once(':') {
            Some((host, port)) if port.chars().all(|ch| ch.is_ascii_digit()) => host,
            Some(_) => return None,
            None => value,
        }
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    (!host.is_empty()).then_some(host)
}

fn is_ip_literal_or_localhost(host: &str) -> bool {
    host.parse::<IpAddr>().is_ok() || host == "localhost" || host.ends_with(".localhost")
}

fn is_loopback_host(host: &str) -> bool {
    host == "localhost" || host.ends_with(".localhost") || host.parse::<IpAddr>().is_ok_and(|ip| ip.is_loopback())
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CspSetting {
    #[default]
    Default,
    Custom(String),
    Disabled,
}

impl CspSetting {
    pub fn parse(value: Option<&str>) -> Result<Self, String> {
        match value {
            None => Ok(Self::Default),
            Some(value) if value.trim().is_empty() => Ok(Self::Disabled),
            Some(value) => {
                let value = value.trim().to_string();
                HeaderValue::from_str(&value).map_err(|_| "DBX_CSP contains invalid header characters".to_string())?;
                Ok(Self::Custom(value))
            }
        }
    }
}

/// Default policy for the SPA. It has to allow what the frontend really does:
/// inline startup scripts and the plugin workbench `srcdoc` iframe (which
/// inherits this policy) need `'unsafe-inline'` and `blob:` scripts, shiki's
/// Oniguruma fallback needs `'wasm-unsafe-eval'`, Vue/reka-ui inject `<style>`
/// tags, Leaflet loads map tiles from arbitrary (custom) tile servers, plugins
/// may call declared `https:` origins, and Redis Pub/Sub uses a same-host
/// WebSocket.
pub fn default_csp(host: Option<&str>) -> String {
    let websocket_sources = host
        .filter(|host| {
            !host.is_empty()
                && host.chars().all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | ':' | '[' | ']'))
        })
        .map(|host| format!(" ws://{host} wss://{host}"))
        .unwrap_or_default();
    format!(
        "default-src 'self'; script-src 'self' 'unsafe-inline' 'wasm-unsafe-eval' blob:; \
         style-src 'self' 'unsafe-inline' blob:; img-src 'self' data: blob: https: http:; \
         font-src 'self' data: blob:; connect-src 'self'{websocket_sources} https:; worker-src 'self' blob:; \
         frame-src 'self' blob: data:; media-src 'self' data: blob:; object-src 'none'; base-uri 'self'; \
         form-action 'self'; frame-ancestors 'self'"
    )
}

#[derive(Clone, Debug, Default)]
pub struct SecurityConfig {
    pub trusted_proxies: TrustedProxies,
    pub cookie_secure: CookieSecure,
    pub host_policy: HostPolicy,
    pub public_origins: Vec<String>,
    pub csp: CspSetting,
}

fn normalize_origin(value: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(value.trim()).map_err(|_| format!("invalid origin in DBX_PUBLIC_ORIGIN: {value}"))?;
    if !matches!(url.scheme(), "http" | "https") || url.host_str().is_none() {
        return Err(format!("DBX_PUBLIC_ORIGIN entries must be http(s) origins: {value}"));
    }
    Ok(url.origin().ascii_serialization())
}

impl SecurityConfig {
    pub fn from_env(password_disabled: bool) -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok();
        Self::from_values(
            var("DBX_TRUSTED_PROXIES").as_deref(),
            var("DBX_COOKIE_SECURE").as_deref(),
            var("DBX_ALLOWED_HOSTS").as_deref(),
            var("DBX_PUBLIC_ORIGIN").as_deref(),
            var("DBX_CSP").as_deref(),
            password_disabled,
        )
    }

    pub fn from_values(
        trusted_proxies: Option<&str>,
        cookie_secure: Option<&str>,
        allowed_hosts: Option<&str>,
        public_origins: Option<&str>,
        csp: Option<&str>,
        password_disabled: bool,
    ) -> Result<Self, String> {
        let public_origins = public_origins
            .map(|value| {
                value
                    .split(',')
                    .map(str::trim)
                    .filter(|entry| !entry.is_empty())
                    .map(normalize_origin)
                    .collect::<Result<Vec<String>, String>>()
            })
            .transpose()?
            .unwrap_or_default();
        Ok(Self {
            trusted_proxies: TrustedProxies::parse(trusted_proxies)?,
            cookie_secure: CookieSecure::parse(cookie_secure)?,
            host_policy: HostPolicy::from_value(allowed_hosts, password_disabled),
            public_origins,
            csp: CspSetting::parse(csp)?,
        })
    }

    pub fn trusted_proxies_configured(&self) -> bool {
        !self.trusted_proxies.0.is_empty()
    }

    /// Client address: the peer, or the forwarded client when the peer is a
    /// trusted proxy (rightmost untrusted `X-Forwarded-For` hop, then `X-Real-IP`).
    pub fn client_ip(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> Option<IpAddr> {
        let peer = peer.map(canonical_ip);
        if !self.trusted_proxies.trusts(peer) {
            return peer;
        }
        let forwarded = headers
            .get_all("x-forwarded-for")
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .map(str::to_string)
            .collect::<Vec<_>>();
        let mut leftmost = None;
        for hop in forwarded.iter().rev() {
            let Some(ip) = parse_forwarded_ip(hop) else {
                break;
            };
            if !self.trusted_proxies.contains(ip) {
                return Some(ip);
            }
            leftmost = Some(ip);
        }
        if let Some(ip) = first_header_value(headers, "x-real-ip").and_then(parse_forwarded_ip) {
            return Some(ip);
        }
        leftmost.or(peer)
    }

    /// The host the browser used: `X-Forwarded-Host` from a trusted proxy,
    /// otherwise the `Host` header.
    pub fn effective_host<'a>(
        &self,
        peer: Option<IpAddr>,
        headers: &'a HeaderMap,
        uri_authority: Option<&'a str>,
    ) -> Option<&'a str> {
        if self.trusted_proxies.trusts(peer.map(canonical_ip)) {
            if let Some(host) = first_header_value(headers, "x-forwarded-host") {
                return Some(host);
            }
        }
        headers.get(HOST).and_then(|value| value.to_str().ok()).or(uri_authority)
    }

    pub fn is_https_request(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> bool {
        self.trusted_proxies.trusts(peer.map(canonical_ip))
            && first_header_value(headers, "x-forwarded-proto").is_some_and(|proto| proto.eq_ignore_ascii_case("https"))
    }

    pub fn cookie_is_secure(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> bool {
        match self.cookie_secure {
            CookieSecure::Always => true,
            CookieSecure::Never => false,
            CookieSecure::Auto => self.is_https_request(peer, headers),
        }
    }

    /// A browser on the server itself: direct loopback connection, no proxy
    /// headers, a loopback `Host` and (if present) a loopback `Origin`. Such
    /// clients may complete first-run setup without the setup token.
    pub fn is_local_setup_client(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> bool {
        if !peer.map(canonical_ip).is_some_and(|peer| peer.is_loopback()) {
            return false;
        }
        if ["x-forwarded-for", "x-real-ip", "forwarded", "x-forwarded-host"]
            .iter()
            .any(|name| headers.contains_key(*name))
        {
            return false;
        }
        let host_is_loopback = headers
            .get(HOST)
            .and_then(|value| value.to_str().ok())
            .and_then(host_without_port)
            .is_some_and(|host| is_loopback_host(&host));
        if !host_is_loopback {
            return false;
        }
        match headers.get(ORIGIN) {
            None => true,
            Some(origin) => origin
                .to_str()
                .ok()
                .and_then(|origin| reqwest::Url::parse(origin).ok())
                .and_then(|url| url.host_str().map(|host| host.trim_matches(['[', ']']).to_ascii_lowercase()))
                .is_some_and(|host| is_loopback_host(&host)),
        }
    }

    /// Cross-site WebSocket hijacking guard: a browser `Origin` must match the
    /// request host (or a configured `DBX_PUBLIC_ORIGIN`). Non-browser clients
    /// that send no `Origin` are allowed; session auth still applies.
    pub fn websocket_origin_allowed(&self, peer: Option<IpAddr>, headers: &HeaderMap) -> bool {
        let Some(origin) = headers.get(ORIGIN) else {
            return true;
        };
        let Some(url) = origin.to_str().ok().and_then(|origin| reqwest::Url::parse(origin).ok()) else {
            return false;
        };
        if !matches!(url.scheme(), "http" | "https") {
            return false;
        }
        let Some(origin_host) = url.host_str() else {
            return false;
        };
        if self.public_origins.contains(&url.origin().ascii_serialization()) {
            return true;
        }
        let origin_authority = match url.port() {
            Some(port) => format!("{origin_host}:{port}"),
            None => origin_host.to_string(),
        }
        .to_ascii_lowercase();
        let Some(host) = self.effective_host(peer, headers, None) else {
            return false;
        };
        let mut host = host.trim().to_ascii_lowercase();
        // Local development (Vite proxy on another localhost port).
        if is_loopback_host(origin_host.trim_matches(['[', ']']))
            && host_without_port(&host).is_some_and(|host| is_loopback_host(&host))
        {
            return true;
        }
        let default_port = if url.scheme() == "https" { ":443" } else { ":80" };
        if let Some(stripped) = host.strip_suffix(default_port) {
            host = stripped.to_string();
        }
        host == origin_authority
    }

    fn csp_header(&self, host: Option<&str>) -> Option<HeaderValue> {
        match &self.csp {
            CspSetting::Disabled => None,
            CspSetting::Custom(value) => HeaderValue::from_str(value).ok(),
            CspSetting::Default => HeaderValue::from_str(&default_csp(host)).ok(),
        }
    }
}

/// Adds `nosniff`, `Referrer-Policy`, `X-Frame-Options` to every response and
/// the CSP to HTML responses that did not set their own (plugin UI assets do).
pub async fn security_headers(
    State(state): State<Arc<WebState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    let peer = PeerAddr::from_extensions(request.extensions()).ip();
    let host = state
        .security
        .effective_host(peer, request.headers(), request.uri().authority().map(|authority| authority.as_str()))
        .map(str::to_string);
    let mut response = next.run(request).await;
    let is_html = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.trim_start().to_ascii_lowercase().starts_with("text/html"));
    let headers = response.headers_mut();
    headers.entry(X_CONTENT_TYPE_OPTIONS).or_insert(HeaderValue::from_static("nosniff"));
    headers.entry(REFERRER_POLICY).or_insert(HeaderValue::from_static("same-origin"));
    headers.entry(X_FRAME_OPTIONS).or_insert(HeaderValue::from_static("SAMEORIGIN"));
    if is_html && !headers.contains_key(CONTENT_SECURITY_POLICY) {
        if let Some(csp) = state.security.csp_header(host.as_deref()) {
            headers.insert(CONTENT_SECURITY_POLICY, csp);
        }
    }
    response
}

/// DNS-rebinding guard for `/api` (see [`HostPolicy`]).
pub async fn host_allowlist(
    State(state): State<Arc<WebState>>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if state.security.host_policy.is_enforced() {
        let peer = PeerAddr::from_extensions(request.extensions()).ip();
        let host = state.security.effective_host(
            peer,
            request.headers(),
            request.uri().authority().map(|authority| authority.as_str()),
        );
        if !state.security.host_policy.allows(host) {
            let shown = host.unwrap_or("").chars().take(255).collect::<String>();
            tracing::warn!("Rejected /api request for disallowed Host {shown:?}; add it to DBX_ALLOWED_HOSTS");
            return (
                StatusCode::FORBIDDEN,
                Json(serde_json::json!({
                    "code": "HOST_NOT_ALLOWED",
                    "error": format!("Host '{shown}' is not allowed. Add it to DBX_ALLOWED_HOSTS."),
                })),
            )
                .into_response();
        }
    }
    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in pairs {
            map.append(*name, HeaderValue::from_str(value).unwrap());
        }
        map
    }

    fn ip(value: &str) -> Option<IpAddr> {
        Some(value.parse().unwrap())
    }

    fn config_with_proxies(proxies: &str) -> SecurityConfig {
        SecurityConfig::from_values(Some(proxies), None, None, None, None, false).unwrap()
    }

    #[test]
    fn trusted_proxies_parse_ips_and_cidrs() {
        let proxies = TrustedProxies::parse(Some("10.0.0.0/8, 192.168.1.5, ::1, fd00::/8")).unwrap();
        assert!(proxies.contains("10.1.2.3".parse().unwrap()));
        assert!(proxies.contains("::ffff:10.9.9.9".parse().unwrap()));
        assert!(proxies.contains("192.168.1.5".parse().unwrap()));
        assert!(!proxies.contains("192.168.1.6".parse().unwrap()));
        assert!(proxies.contains("::1".parse().unwrap()));
        assert!(proxies.contains("fd12::1".parse().unwrap()));
        assert!(!proxies.contains("2001:db8::1".parse().unwrap()));
        assert!(TrustedProxies::parse(Some("10.0.0.0/33")).is_err());
        assert!(TrustedProxies::parse(Some("proxy.local")).is_err());
        assert!(TrustedProxies::parse(Some("0.0.0.0/0")).unwrap().contains("8.8.8.8".parse().unwrap()));
    }

    #[test]
    fn forwarded_headers_are_only_trusted_from_trusted_proxies() {
        let config = config_with_proxies("10.0.0.0/8");
        let forwarded = headers(&[("x-forwarded-for", "198.51.100.9, 10.0.0.7"), ("x-real-ip", "192.0.2.1")]);
        assert_eq!(config.client_ip(ip("203.0.113.5"), &forwarded), ip("203.0.113.5"));
        assert_eq!(config.client_ip(ip("10.0.0.2"), &forwarded), ip("198.51.100.9"));
        assert_eq!(config.client_ip(ip("10.0.0.2"), &headers(&[("x-real-ip", "192.0.2.1")])), ip("192.0.2.1"));
        assert_eq!(config.client_ip(ip("10.0.0.2"), &HeaderMap::new()), ip("10.0.0.2"));
        // A spoofed leftmost entry does not win over the hop the proxy appended.
        let spoofed = headers(&[("x-forwarded-for", "127.0.0.1, 198.51.100.10")]);
        assert_eq!(config.client_ip(ip("10.0.0.2"), &spoofed), ip("198.51.100.10"));
        assert_eq!(config.client_ip(None, &forwarded), None);
    }

    #[test]
    fn cookie_secure_modes() {
        let https = headers(&[("x-forwarded-proto", "https")]);
        let auto = config_with_proxies("10.0.0.1");
        assert!(auto.cookie_is_secure(ip("10.0.0.1"), &https));
        assert!(!auto.cookie_is_secure(ip("203.0.113.1"), &https));
        assert!(!auto.cookie_is_secure(ip("10.0.0.1"), &HeaderMap::new()));
        let always = SecurityConfig::from_values(None, Some("true"), None, None, None, false).unwrap();
        assert!(always.cookie_is_secure(None, &HeaderMap::new()));
        let never = SecurityConfig::from_values(Some("10.0.0.1"), Some("false"), None, None, None, false).unwrap();
        assert!(!never.cookie_is_secure(ip("10.0.0.1"), &https));
        assert!(CookieSecure::parse(Some("sometimes")).is_err());
    }

    #[test]
    fn host_policy_defaults_follow_password_mode() {
        let open = HostPolicy::from_value(None, false);
        assert!(!open.is_enforced());
        assert!(open.allows(Some("attacker.example")));

        let rebinding_guard = HostPolicy::from_value(None, true);
        assert!(rebinding_guard.is_enforced());
        for allowed in ["localhost:4224", "127.0.0.1:4224", "[::1]:4224", "192.168.1.20", "app.localhost"] {
            assert!(rebinding_guard.allows(Some(allowed)), "{allowed}");
        }
        for rejected in [Some("attacker.example"), Some("attacker.example:4224"), Some(""), None] {
            assert!(!rebinding_guard.allows(rejected), "{rejected:?}");
        }

        let configured = HostPolicy::from_value(Some("dbx.example.com, *.corp.example"), false);
        assert!(configured.is_enforced());
        assert!(configured.allows(Some("DBX.example.com:443")));
        assert!(configured.allows(Some("db.corp.example")));
        assert!(!configured.allows(Some("corp.example.evil")));
        assert!(!configured.allows(Some("other.example.com")));

        assert!(!HostPolicy::from_value(Some("*"), true).is_enforced());
    }

    #[test]
    fn host_without_port_handles_ipv6_and_ports() {
        assert_eq!(host_without_port("Example.com:8080").as_deref(), Some("example.com"));
        assert_eq!(host_without_port("[::1]:4224").as_deref(), Some("::1"));
        assert_eq!(host_without_port("::1").as_deref(), Some("::1"));
        assert_eq!(host_without_port("example.com.").as_deref(), Some("example.com"));
        assert_eq!(host_without_port("bad:host:x"), None);
    }

    #[test]
    fn local_setup_requires_direct_loopback_browser() {
        let config = SecurityConfig::default();
        let local = headers(&[("host", "localhost:4224")]);
        assert!(config.is_local_setup_client(ip("127.0.0.1"), &local));
        assert!(config.is_local_setup_client(ip("::1"), &headers(&[("host", "[::1]:4224")])));
        assert!(config.is_local_setup_client(
            ip("127.0.0.1"),
            &headers(&[("host", "127.0.0.1:4224"), ("origin", "http://127.0.0.1:4224")])
        ));
        assert!(!config.is_local_setup_client(ip("192.168.1.2"), &local));
        assert!(!config.is_local_setup_client(None, &local));
        // Local reverse proxy forwarding a remote client.
        assert!(!config.is_local_setup_client(
            ip("127.0.0.1"),
            &headers(&[("host", "localhost"), ("x-forwarded-for", "203.0.113.1")])
        ));
        // DNS rebinding: loopback peer, foreign host name or origin.
        assert!(!config.is_local_setup_client(ip("127.0.0.1"), &headers(&[("host", "attacker.example:4224")])));
        assert!(!config.is_local_setup_client(
            ip("127.0.0.1"),
            &headers(&[("host", "localhost:4224"), ("origin", "http://attacker.example")])
        ));
    }

    #[test]
    fn websocket_origin_must_match_host_or_public_origin() {
        let config =
            SecurityConfig::from_values(Some("10.0.0.1"), None, None, Some("https://dbx.example.com"), None, false)
                .unwrap();
        assert!(config.websocket_origin_allowed(ip("203.0.113.1"), &HeaderMap::new()));
        assert!(config.websocket_origin_allowed(
            ip("203.0.113.1"),
            &headers(&[("host", "192.168.1.5:4224"), ("origin", "http://192.168.1.5:4224")])
        ));
        assert!(config.websocket_origin_allowed(
            ip("203.0.113.1"),
            &headers(&[("host", "example.org:443"), ("origin", "https://example.org")])
        ));
        assert!(!config.websocket_origin_allowed(
            ip("203.0.113.1"),
            &headers(&[("host", "192.168.1.5:4224"), ("origin", "http://attacker.example")])
        ));
        assert!(!config.websocket_origin_allowed(
            ip("203.0.113.1"),
            &headers(&[("host", "192.168.1.5:4224"), ("origin", "null")])
        ));
        assert!(config.websocket_origin_allowed(
            ip("203.0.113.1"),
            &headers(&[("host", "internal:4224"), ("origin", "https://dbx.example.com")])
        ));
        assert!(config.websocket_origin_allowed(
            ip("127.0.0.1"),
            &headers(&[("host", "localhost:4224"), ("origin", "http://localhost:1420")])
        ));
        assert!(!config.websocket_origin_allowed(
            ip("127.0.0.1"),
            &headers(&[("host", "localhost:4224"), ("origin", "http://attacker.example:4224")])
        ));
        // X-Forwarded-Host is honoured only from a trusted proxy.
        let proxied = headers(&[
            ("host", "dbx:4224"),
            ("x-forwarded-host", "db.example.net"),
            ("origin", "https://db.example.net"),
        ]);
        assert!(config.websocket_origin_allowed(ip("10.0.0.1"), &proxied));
        assert!(!config.websocket_origin_allowed(ip("203.0.113.1"), &proxied));
    }

    #[test]
    fn csp_setting_supports_override_and_disable() {
        assert_eq!(CspSetting::parse(None).unwrap(), CspSetting::Default);
        assert_eq!(CspSetting::parse(Some("  ")).unwrap(), CspSetting::Disabled);
        assert_eq!(
            CspSetting::parse(Some("default-src 'self'")).unwrap(),
            CspSetting::Custom("default-src 'self'".into())
        );
        assert!(CspSetting::parse(Some("default-src\n'self'")).is_err());
        let csp = default_csp(Some("localhost:4224"));
        assert!(csp.contains("connect-src 'self' ws://localhost:4224 wss://localhost:4224 https:"));
        assert!(csp.contains("'wasm-unsafe-eval'"));
        assert!(csp.contains("frame-ancestors 'self'"));
        assert!(csp.contains("object-src 'none'"));
        // Host header values that could inject directives are ignored.
        assert!(!default_csp(Some("evil; script-src *")).contains("evil"));
    }
}
