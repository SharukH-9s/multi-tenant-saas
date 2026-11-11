//! Client IP resolution for rate limiting.
//!
//! When `trusted_proxy_hops == 0`, always uses the TCP peer address.
//! When `N > 0`, reads the N-th-from-right address in `X-Forwarded-For`
//! (the right side is appended by proxies you control; the left side is client-controlled).
//!
//! IPv6 addresses are keyed by the **/64 prefix**: an attacker usually owns a whole /64
//! and could rotate addresses within it.
//!
//! Falls back to the shared `"unknown"` bucket when no address can be determined.
//! "unknown" is strict (never permissive): it uses the full IP/email capacity, not elevated.

use std::net::{IpAddr, Ipv6Addr, SocketAddr};

use axum::{
    extract::{ConnectInfo, FromRequestParts},
    http::request::Parts,
};
use common_errors::AppError;

use crate::app_state::AppState;

/// A normalized client IP string suitable for use in a Redis rate-limit key.
///
/// - IPv4: dotted decimal (e.g. `"1.2.3.4"`).
/// - IPv6: /64 prefix as `"hhhh:hhhh:hhhh:hhhh"` (lowercase hex, no leading zeros stripped).
/// - Unknown/fallback: `"unknown"` (counts against one shared bucket).
#[derive(Debug, Clone)]
pub struct ClientIp(pub String);

/// Resolves the effective client IP from the TCP peer address and the
/// `X-Forwarded-For` header, respecting the configured number of trusted proxy hops.
///
/// # Arguments
/// * `peer`          — The raw TCP peer IP (may be absent in tests using `oneshot`).
/// * `forwarded_for` — The raw value of `X-Forwarded-For` (comma-separated list, left = client).
/// * `hops`          — Number of proxies you trust in front of this service (0 = ignore header).
pub fn resolve_client_ip(peer: Option<IpAddr>, forwarded_for: Option<&str>, hops: u8) -> String {
    let ip = if hops == 0 {
        // Strict: never trust the XFF header when hops == 0.
        peer
    } else {
        // Pick the N-th entry from the right of XFF, where N = hops.
        // The last `hops` entries were appended by trusted proxies; the entry just before them
        // is the first address outside your infrastructure.
        forwarded_for
            .and_then(|xff| pick_xff_entry(xff, hops))
            .or(peer)
    };

    match ip {
        Some(addr) => normalize_ip(addr),
        None => "unknown".to_string(),
    }
}

/// Picks the entry at position `len - 1 - hops` from a comma-separated XFF value.
///
/// If the list doesn't have enough entries (the whole list came from proxies), returns `None`.
fn pick_xff_entry(xff: &str, hops: u8) -> Option<IpAddr> {
    let entries: Vec<&str> = xff.split(',').map(str::trim).collect();
    let n = hops as usize;
    if entries.len() <= n {
        return None; // All entries came from our proxies; can't trust any.
    }
    let idx = entries.len() - 1 - n;
    entries[idx].parse().ok()
}

/// Returns a normalized IP string for use in Redis keys.
///
/// IPv6 is bucketed to the /64 prefix to prevent prefix-rotation attacks.
fn normalize_ip(addr: IpAddr) -> String {
    match addr {
        IpAddr::V4(v4) => v4.to_string(),
        IpAddr::V6(v6) => ipv6_prefix_64(v6),
    }
}

/// Returns the /64 prefix of an IPv6 address as `"hhhh:hhhh:hhhh:hhhh"`.
fn ipv6_prefix_64(addr: Ipv6Addr) -> String {
    let s = addr.segments();
    format!("{:04x}:{:04x}:{:04x}:{:04x}", s[0], s[1], s[2], s[3])
}

/// Axum extractor that resolves the client IP using `AppState::config.auth.trusted_proxy_hops`.
///
/// Requires the router to be served with `into_make_service_with_connect_info::<SocketAddr>()`.
/// Falls back gracefully when no `ConnectInfo` is available (e.g. in tests that use `oneshot`),
/// returning `"unknown"` which is counted against a shared bucket.
impl FromRequestParts<AppState> for ClientIp {
    type Rejection = AppError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let peer_ip = parts
            .extensions
            .get::<ConnectInfo<SocketAddr>>()
            .map(|ci| ci.0.ip());

        let xff = parts
            .headers
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);

        let hops = state.config.auth.trusted_proxy_hops;
        let ip = resolve_client_ip(peer_ip, xff.as_deref(), hops);
        Ok(ClientIp(ip))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    fn v4(s: &str) -> Option<IpAddr> {
        Some(IpAddr::V4(s.parse::<Ipv4Addr>().unwrap()))
    }

    fn v6(s: &str) -> Option<IpAddr> {
        Some(IpAddr::V6(s.parse::<Ipv6Addr>().unwrap()))
    }

    // hops=0 — always use TCP peer, ignore XFF header entirely.
    #[test]
    fn hops_zero_ignores_xff() {
        let ip = resolve_client_ip(v4("1.2.3.4"), Some("9.9.9.9"), 0);
        assert_eq!(ip, "1.2.3.4");
    }

    // hops=1 — the rightmost XFF entry was added by our proxy; pick the one before it.
    #[test]
    fn hops_one_picks_from_right() {
        // XFF = "client, proxy_added"  → pick index 0 → "client" (1.2.3.4)
        let ip = resolve_client_ip(v4("127.0.0.1"), Some("1.2.3.4, 10.0.0.1"), 1);
        assert_eq!(ip, "1.2.3.4");
    }

    // hops=2 — two proxies add entries; pick the one before those two.
    #[test]
    fn hops_two_picks_further_left() {
        // XFF = "client, mid, proxy" → len=3, idx=3-1-2=0 → "client" (5.6.7.8)
        let ip = resolve_client_ip(v4("127.0.0.1"), Some("5.6.7.8, 192.168.1.1, 10.0.0.2"), 2);
        assert_eq!(ip, "5.6.7.8");
    }

    // If XFF doesn't have enough entries, fall back to TCP peer.
    #[test]
    fn not_enough_xff_entries_falls_back_to_peer() {
        // XFF has 1 entry but hops=2 → falls back to peer (1.2.3.4)
        let ip = resolve_client_ip(v4("1.2.3.4"), Some("9.9.9.9"), 2);
        assert_eq!(ip, "1.2.3.4");
    }

    // IPv6 addresses are bucketed to the /64 prefix.
    #[test]
    fn ipv6_is_keyed_by_64_prefix() {
        let ip = resolve_client_ip(v6("2001:db8:1234:5678:abcd:ef01:2345:6789"), None, 0);
        assert_eq!(ip, "2001:0db8:1234:5678");
    }

    // No peer, no XFF → "unknown".
    #[test]
    fn no_peer_no_xff_returns_unknown() {
        let ip = resolve_client_ip(None, None, 0);
        assert_eq!(ip, "unknown");
    }

    // Invalid XFF entry falls back to TCP peer.
    #[test]
    fn invalid_xff_entry_falls_back_to_peer() {
        let ip = resolve_client_ip(v4("1.2.3.4"), Some("not-an-ip"), 1);
        assert_eq!(ip, "1.2.3.4");
    }

    // No peer, invalid XFF → "unknown".
    #[test]
    fn no_peer_invalid_xff_returns_unknown() {
        let ip = resolve_client_ip(None, Some("not-an-ip"), 1);
        assert_eq!(ip, "unknown");
    }
}
