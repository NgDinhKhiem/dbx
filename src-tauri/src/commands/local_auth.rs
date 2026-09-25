//! Shared helpers for the desktop's loopback listeners (Redis PubSub
//! WebSocket, MCP bridge): per-launch secrets, constant-time comparison and
//! the set of Origins the DBX webview uses.

use uuid::Uuid;

/// 244 bits from the OS CSPRNG (uuid v4 uses `getrandom`), hex encoded.
pub fn random_token() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

/// Compares secrets without an early exit on the first differing byte.
pub fn constant_time_eq(expected: &str, candidate: &str) -> bool {
    let expected = expected.as_bytes();
    let candidate = candidate.as_bytes();
    if expected.len() != candidate.len() {
        return false;
    }
    expected.iter().zip(candidate).fold(0_u8, |difference, (left, right)| difference | (left ^ right)) == 0
}

/// Origins of the DBX webview itself: `tauri://localhost` (macOS/Linux),
/// `http(s)://tauri.localhost` (Windows WebView2), plus the Vite dev server in
/// debug builds.
pub fn is_app_origin(origin: &str) -> bool {
    let origin = origin.trim().trim_end_matches('/').to_ascii_lowercase();
    matches!(origin.as_str(), "tauri://localhost" | "http://tauri.localhost" | "https://tauri.localhost")
        || (cfg!(debug_assertions) && matches!(origin.as_str(), "http://localhost:1420" | "http://127.0.0.1:1420"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_and_unique() {
        let first = random_token();
        assert_eq!(first.len(), 64);
        assert_ne!(first, random_token());
    }

    #[test]
    fn constant_time_eq_matches_only_identical_secrets() {
        assert!(constant_time_eq("abc", "abc"));
        assert!(!constant_time_eq("abc", "abd"));
        assert!(!constant_time_eq("abc", "abcd"));
        assert!(!constant_time_eq("abc", ""));
    }

    #[test]
    fn only_webview_origins_are_app_origins() {
        assert!(is_app_origin("tauri://localhost"));
        assert!(is_app_origin("http://tauri.localhost"));
        assert!(is_app_origin("https://tauri.localhost/"));
        assert!(!is_app_origin("https://evil.example"));
        assert!(!is_app_origin("http://tauri.localhost.evil.example"));
        assert!(!is_app_origin("null"));
    }
}
