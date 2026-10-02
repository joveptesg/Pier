//! Where does each domain's DNS actually point?
//!
//! Let's Encrypt's HTTP-01 challenge goes to whatever the domain resolves to.
//! A domain added here whose A record names another server, nothing at all
//! (expired / unpaid), or a Cloudflare edge can't get a certificate, and
//! Traefik retries it forever without the panel ever saying why. This module
//! records the answer per domain so the UI can warn. It never blocks routing
//! or issuance — DNS may have changed a minute ago.
//!
//! Runs once per domain when it is added or activated, for every domain every
//! 6 hours, and on demand from the domains page.

use std::collections::HashSet;
use std::net::IpAddr;
use std::time::Duration;

use crate::state::SharedState;

const SWEEP_INTERVAL: Duration = Duration::from_secs(6 * 3600);
const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DnsStatus {
    /// Resolves to (at least one address of) the server hosting the service.
    Ok,
    /// Resolves, but only to addresses that aren't that server.
    Mismatch,
    /// Doesn't resolve at all.
    NxDomain,
    /// Resolves only to Cloudflare edges — the real origin is hidden.
    Proxied,
}

impl DnsStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            DnsStatus::Ok => "ok",
            DnsStatus::Mismatch => "mismatch",
            DnsStatus::NxDomain => "nxdomain",
            DnsStatus::Proxied => "proxied",
        }
    }
}

/// Background sweep: first pass a minute after boot, then every 6 hours.
pub fn start(state: SharedState) {
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_secs(60)).await;
        loop {
            match check_all(&state).await {
                Ok(n) => tracing::debug!("DNS check: {n} domain(s) checked"),
                Err(e) => tracing::warn!("DNS check failed: {e}"),
            }
            tokio::time::sleep(SWEEP_INTERVAL).await;
        }
    });
}

/// Check one domain in the background (after create / activate).
pub fn spawn_check_one(state: SharedState, domain_id: String) {
    tokio::spawn(async move {
        if let Err(e) = check_ids(&state, Some(&domain_id)).await {
            tracing::debug!("DNS check for domain {domain_id} failed: {e}");
        }
    });
}

/// Check every domain. Returns how many got a definite answer.
pub async fn check_all(state: &SharedState) -> anyhow::Result<usize> {
    check_ids(state, None).await
}

struct Target {
    id: String,
    host: String,
    /// `host` column of the remote server running the service; `None` = local.
    remote_host: Option<String>,
}

async fn check_ids(state: &SharedState, only: Option<&str>) -> anyhow::Result<usize> {
    let (targets, local_hosts) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db.prepare(
            "SELECT d.id, d.domain, CASE WHEN sv.is_local = 0 THEN sv.host END
             FROM domains d
             LEFT JOIN services s ON s.id = d.service_id
             LEFT JOIN servers sv ON sv.id = s.server_id
             WHERE ?1 IS NULL OR d.id = ?1",
        )?;
        let targets: Vec<Target> = stmt
            .query_map([only], |row| {
                Ok(Target {
                    id: row.get(0)?,
                    host: hostname(&row.get::<_, String>(1)?),
                    remote_host: row.get::<_, Option<String>>(2)?,
                })
            })?
            .filter_map(|r| r.ok())
            .filter(|t| !t.host.is_empty())
            .collect();
        (targets, local_server_hosts(&db))
    };
    if targets.is_empty() {
        return Ok(0);
    }

    let local_ips = resolve_all(&local_hosts).await;
    let results = futures_util::future::join_all(targets.into_iter().map(|t| {
        let local_ips = &local_ips;
        async move {
            let expected = match &t.remote_host {
                Some(h) => resolve_all(std::slice::from_ref(h)).await,
                None => local_ips.clone(),
            };
            let answer = lookup(&t.host).await;
            (t, expected, answer)
        }
    }))
    .await;

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let mut checked = 0;
    for (t, expected, answer) in results {
        // Resolver trouble on our side says nothing about the domain — keep
        // the previous verdict rather than flag every domain as broken.
        let Some(resolved) = answer else { continue };
        // Without knowing our own address we can't tell ok from mismatch.
        if expected.is_empty() && !resolved.is_empty() {
            continue;
        }
        let status = classify(&resolved, &expected);
        let ips = resolved
            .iter()
            .map(|ip| ip.to_string())
            .collect::<Vec<_>>()
            .join(",");
        let _ = db.execute(
            "UPDATE domains SET dns_status = ?2, dns_ips = ?3 WHERE id = ?1",
            rusqlite::params![t.id, status.as_str(), ips],
        );
        checked += 1;
    }
    Ok(checked)
}

/// Addresses (or names) the local server is known by.
fn local_server_hosts(db: &rusqlite::Connection) -> Vec<String> {
    let mut hosts: Vec<String> = [
        "server.public_ip",
        "server.public_ipv4",
        "server.public_ipv6",
    ]
    .iter()
    .filter_map(|key| {
        db.query_row("SELECT value FROM settings WHERE key = ?1", [key], |row| {
            row.get::<_, String>(0)
        })
        .ok()
    })
    .collect();
    if let Ok(h) = db.query_row(
        "SELECT host FROM servers WHERE is_local = 1 LIMIT 1",
        [],
        |row| row.get::<_, String>(0),
    ) {
        hosts.push(h);
    }
    hosts.retain(|h| !h.trim().is_empty());
    hosts
}

/// Literal IPs pass through; names are resolved. Loopback/unspecified dropped.
async fn resolve_all(hosts: &[String]) -> HashSet<IpAddr> {
    let mut out = HashSet::new();
    for h in hosts {
        let h = h.trim().trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = h.parse::<IpAddr>() {
            out.insert(ip);
        } else if let Some(ips) = lookup(h).await {
            out.extend(ips);
        }
    }
    out.retain(|ip| !ip.is_loopback() && !ip.is_unspecified());
    out
}

/// `Some(addrs)` for a definite answer (empty = name doesn't exist),
/// `None` when the resolver itself failed or timed out.
async fn lookup(host: &str) -> Option<Vec<IpAddr>> {
    match tokio::time::timeout(LOOKUP_TIMEOUT, tokio::net::lookup_host((host, 80))).await {
        Ok(Ok(addrs)) => {
            let mut ips: Vec<IpAddr> = addrs.map(|a| a.ip()).collect();
            ips.sort();
            ips.dedup();
            Some(ips)
        }
        Ok(Err(e)) if is_no_such_name(&e.to_string()) => Some(Vec::new()),
        _ => None,
    }
}

/// glibc's getaddrinfo messages for "the name has no addresses", as opposed
/// to "the resolver couldn't be reached" (EAI_AGAIN / EAI_FAIL).
fn is_no_such_name(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("name or service not known")
        || m.contains("no address associated")
        || m.contains("nodename nor servname")
        || m.contains("no such host")
}

pub fn classify(resolved: &[IpAddr], expected: &HashSet<IpAddr>) -> DnsStatus {
    if resolved.is_empty() {
        DnsStatus::NxDomain
    } else if resolved.iter().any(|ip| expected.contains(ip)) {
        DnsStatus::Ok
    } else if resolved.iter().all(is_cloudflare) {
        DnsStatus::Proxied
    } else {
        DnsStatus::Mismatch
    }
}

fn hostname(domain: &str) -> String {
    domain
        .split('/')
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

/// Cloudflare's published edge ranges (cloudflare.com/ips).
const CLOUDFLARE_V4: &[(u32, u8)] = &[
    (0xADF5_3000, 20), // 173.245.48.0/20
    (0x6715_F400, 22), // 103.21.244.0/22
    (0x6716_C800, 22), // 103.22.200.0/22
    (0x671F_0400, 22), // 103.31.4.0/22
    (0x8D65_4000, 18), // 141.101.64.0/18
    (0x6CA2_C000, 18), // 108.162.192.0/18
    (0xBE5D_F000, 20), // 190.93.240.0/20
    (0xBC72_6000, 20), // 188.114.96.0/20
    (0xC5EA_F000, 22), // 197.234.240.0/22
    (0xC629_8000, 17), // 198.41.128.0/17
    (0xA29E_0000, 15), // 162.158.0.0/15
    (0x6810_0000, 13), // 104.16.0.0/13
    (0x6818_0000, 14), // 104.24.0.0/14
    (0xAC40_0000, 13), // 172.64.0.0/13
    (0x8300_4800, 22), // 131.0.72.0/22
];
const CLOUDFLARE_V6: &[(u128, u8)] = &[
    (0x2400_cb00 << 96, 32),
    (0x2606_4700 << 96, 32),
    (0x2803_f800 << 96, 32),
    (0x2405_b500 << 96, 32),
    (0x2405_8100 << 96, 32),
    (0x2a06_98c0 << 96, 29),
    (0x2c0f_f248 << 96, 32),
];

fn is_cloudflare(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let a = u32::from(*v4);
            CLOUDFLARE_V4
                .iter()
                .any(|(net, len)| a & (u32::MAX << (32 - len)) == *net)
        }
        IpAddr::V6(v6) => {
            let a = u128::from(*v6);
            CLOUDFLARE_V6
                .iter()
                .any(|(net, len)| a & (u128::MAX << (128 - len)) == *net)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|s| s.parse().unwrap()).collect()
    }

    #[test]
    fn classifies() {
        let me: HashSet<IpAddr> = ips(&["178.18.249.144"]).into_iter().collect();
        assert_eq!(classify(&[], &me), DnsStatus::NxDomain);
        assert_eq!(classify(&ips(&["178.18.249.144"]), &me), DnsStatus::Ok);
        assert_eq!(
            classify(&ips(&["130.49.143.35", "178.18.249.144"]), &me),
            DnsStatus::Ok
        );
        assert_eq!(classify(&ips(&["130.49.143.35"]), &me), DnsStatus::Mismatch);
        assert_eq!(
            classify(&ips(&["172.67.195.35", "104.21.20.246"]), &me),
            DnsStatus::Proxied
        );
        assert_eq!(
            classify(&ips(&["2606:4700:3030::6815:14f6"]), &me),
            DnsStatus::Proxied
        );
    }

    #[test]
    fn cloudflare_ranges() {
        assert!(is_cloudflare(&"104.21.20.246".parse().unwrap()));
        assert!(is_cloudflare(&"172.67.195.35".parse().unwrap()));
        assert!(is_cloudflare(&"188.114.97.3".parse().unwrap()));
        assert!(!is_cloudflare(&"2.59.170.20".parse().unwrap()));
        assert!(!is_cloudflare(&"104.219.250.37".parse().unwrap()));
        assert!(!is_cloudflare(&"2001:db8::1".parse().unwrap()));
    }

    #[test]
    fn strips_path_and_case() {
        assert_eq!(hostname("API.flow-fin.ru/api/v1"), "api.flow-fin.ru");
    }

    #[test]
    fn tells_missing_names_from_resolver_failures() {
        assert!(is_no_such_name(
            "failed to lookup address information: Name or service not known"
        ));
        assert!(!is_no_such_name(
            "failed to lookup address information: Temporary failure in name resolution"
        ));
    }
}
