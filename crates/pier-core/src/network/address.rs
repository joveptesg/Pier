//! URL/endpoint formatting helpers that handle IPv6 literals.
//!
//! Plain `format!("https://{host}:{port}")` is wrong for IPv6:
//! `https://2a01:4f9::1:8443/...` is unparseable because everything
//! after the second `:` looks like a port. The correct authority for
//! v6 is `https://[2a01:4f9::1]:8443/...` (RFC 3986 §3.2.2). Same
//! bracket convention applies to WireGuard's `Endpoint =` directive.
//!
//! Storage convention: `servers.host` and `wireguard_peers.endpoint`
//! hold the **bare** address (no brackets). We add brackets at format
//! time so the DB rows stay platform-neutral and operator-readable.

/// Heuristic: looks like an IPv6 literal? We can't use `Ipv6Addr::
/// from_str` directly because the column may also hold hostnames and
/// IPv4 addresses; instead we look for the unambiguous markers.
///
/// True if:
/// - contains `::` (compressed form, always valid v6), OR
/// - has two or more `:` characters AND no `.` (IPv4-mapped forms
///   like `::ffff:192.0.2.1` are caught by the `::` rule above).
///
/// False for everything else — hostnames (`vps2.example.com`),
/// IPv4 (`192.0.2.1`), and bracket-prefixed strings (caller should
/// have stripped brackets at storage time).
pub fn is_ipv6_literal(host: &str) -> bool {
    if host.contains("::") {
        return true;
    }
    let colons = host.bytes().filter(|b| *b == b':').count();
    colons >= 2 && !host.contains('.')
}

/// Normalize an operator-entered address for storage in `servers.host`
/// / `wireguard_peers.endpoint`. Strips surrounding `[...]` brackets
/// so the value round-trips through validation cleanly.
pub fn normalize_host(host: &str) -> String {
    let trimmed = host.trim();
    if trimmed.starts_with('[') && trimmed.ends_with(']') {
        trimmed[1..trimmed.len() - 1].to_string()
    } else {
        trimmed.to_string()
    }
}

/// Format the host portion for use inside a URL authority. IPv6
/// literals are wrapped in `[...]`; everything else is returned as-is.
pub fn host_for_url(host: &str) -> String {
    if is_ipv6_literal(host) {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Build a host:port authority string suitable for a URL or a
/// WireGuard `Endpoint = ...` directive.
pub fn authority(host: &str, port: i64) -> String {
    format!("{}:{port}", host_for_url(host))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_compressed_v6() {
        assert!(is_ipv6_literal("::1"));
        assert!(is_ipv6_literal("fe80::1"));
        assert!(is_ipv6_literal("2a01:4f9::1"));
    }

    #[test]
    fn detects_uncompressed_v6() {
        assert!(is_ipv6_literal("2001:0db8:0000:0000:0000:ff00:0042:8329"));
        assert!(is_ipv6_literal("2001:db8::"));
    }

    #[test]
    fn rejects_v4_and_hostname() {
        assert!(!is_ipv6_literal("192.0.2.1"));
        assert!(!is_ipv6_literal("vps2.example.com"));
        assert!(!is_ipv6_literal("localhost"));
    }

    #[test]
    fn normalise_strips_brackets() {
        assert_eq!(normalize_host("[fe80::1]"), "fe80::1");
        assert_eq!(normalize_host(" 2a01::1 "), "2a01::1");
        assert_eq!(normalize_host("vps2.example.com"), "vps2.example.com");
    }

    #[test]
    fn authority_brackets_v6() {
        assert_eq!(authority("fe80::1", 8443), "[fe80::1]:8443");
        assert_eq!(authority("192.0.2.1", 8443), "192.0.2.1:8443");
        assert_eq!(authority("vps2.example.com", 8443), "vps2.example.com:8443");
    }
}
