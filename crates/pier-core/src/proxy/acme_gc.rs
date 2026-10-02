//! Garbage collection for Traefik's `acme.json`.
//!
//! Deleting a domain only removes its router; the certificate stays in
//! `acme.json`, and Traefik renews every certificate in its store whether or
//! not anything still routes to it. A domain removed months ago (or one whose
//! DNS has since moved to another server) therefore keeps failing ACME
//! renewals forever, filling the log and burning Let's Encrypt's failed
//! validation budget.
//!
//! A certificate is kept when its main name or any SAN is either:
//! - a hostname in the `domains` table — active *or* deactivated, since
//!   deactivation deliberately keeps the cert for an instant re-activation, or
//! - routed by any file in `traefik/dynamic/` — this covers the platform
//!   domain and any other router Pier writes outside the `domains` table.
//!
//! Wildcard certificates are always kept. Pruning only runs while Traefik is
//! stopped (see `deploy_traefik_inner`), because a running Traefik holds the
//! store in memory and would overwrite the edit.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};

/// Hostnames of every row in `domains` plus the platform (panel) domain
/// (path prefixes stripped, lowercased).
///
/// The platform domain is listed explicitly because at boot its router file is
/// written only *after* Traefik is deployed, so the dynamic-dir scan alone
/// can't be relied on to protect the panel's own certificate.
///
/// `None` when the table can't be read or has no domains: an empty keep-list
/// would otherwise mean "delete every certificate", and a node with no
/// domains has nothing worth pruning anyway.
pub fn domain_hosts(db: &rusqlite::Connection) -> Option<Vec<String>> {
    let mut stmt = db.prepare("SELECT domain FROM domains").ok()?;
    let mut hosts: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(0))
        .ok()?
        .filter_map(|r| r.ok())
        .map(|d| normalize_host(&d))
        .filter(|h| !h.is_empty())
        .collect();
    if hosts.is_empty() {
        return None;
    }
    if let Ok(platform) = db.query_row(
        "SELECT value FROM settings WHERE key = 'proxy.platform_domain'",
        [],
        |row| row.get::<_, String>(0),
    ) {
        let platform = super::config::normalize_domain(&platform);
        if !platform.is_empty() {
            hosts.push(platform);
        }
    }
    Some(hosts)
}

/// Drop certificates nothing needs any more. Returns the main names removed.
pub fn prune_acme_store(data_dir: &Path, domain_hosts: &[String]) -> Result<Vec<String>> {
    if domain_hosts.is_empty() {
        return Ok(Vec::new());
    }
    let traefik_dir = data_dir.join("traefik");
    let acme_path = traefik_dir.join("acme.json");
    let content = match std::fs::read_to_string(&acme_path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).context("read acme.json"),
    };
    let mut acme: serde_json::Value = serde_json::from_str(&content).context("parse acme.json")?;

    let mut keep: HashSet<String> = domain_hosts.iter().map(|h| normalize_host(h)).collect();
    keep.extend(routed_hosts(&traefik_dir.join("dynamic")));

    let removed = prune_certificates(&mut acme, &keep);
    if removed.is_empty() {
        return Ok(removed);
    }

    let serialized = serde_json::to_string_pretty(&acme).context("serialize acme.json")?;
    write_private(&traefik_dir.join("acme.json.bak"), content.as_bytes())?;
    let tmp = traefik_dir.join("acme.json.tmp");
    write_private(&tmp, serialized.as_bytes())?;
    std::fs::rename(&tmp, &acme_path).context("replace acme.json")?;
    Ok(removed)
}

/// Remove certificates whose names are all absent from `keep`, across every
/// resolver in the store. Returns the main names removed.
fn prune_certificates(acme: &mut serde_json::Value, keep: &HashSet<String>) -> Vec<String> {
    let mut removed = Vec::new();
    let Some(resolvers) = acme.as_object_mut() else {
        return removed;
    };
    for resolver in resolvers.values_mut() {
        let Some(certs) = resolver
            .get_mut("Certificates")
            .and_then(|c| c.as_array_mut())
        else {
            continue;
        };
        certs.retain(|cert| {
            let names = cert_names(cert);
            // A malformed entry we can't name is left alone.
            if names.is_empty() {
                return true;
            }
            let needed = names
                .iter()
                .any(|n| n.starts_with("*.") || keep.contains(n));
            if !needed {
                removed.push(names[0].clone());
            }
            needed
        });
    }
    removed
}

/// Main name first, then SANs — all normalized.
fn cert_names(cert: &serde_json::Value) -> Vec<String> {
    let Some(domain) = cert.get("domain") else {
        return Vec::new();
    };
    let main = domain.get("main").and_then(|m| m.as_str()).unwrap_or("");
    let sans = domain
        .get("sans")
        .and_then(|s| s.as_array())
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str());
    std::iter::once(main)
        .chain(sans)
        .map(normalize_host)
        .filter(|h| !h.is_empty())
        .collect()
}

/// Every host named in a `Host(`…`)` / `HostSNI(`…`)` matcher in the dynamic
/// config directory.
fn routed_hosts(dynamic_dir: &Path) -> HashSet<String> {
    let mut hosts = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dynamic_dir) else {
        return hosts;
    };
    for entry in entries.flatten() {
        if let Ok(text) = std::fs::read_to_string(entry.path()) {
            hosts.extend(hosts_in_rules(&text));
        }
    }
    hosts
}

fn hosts_in_rules(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for marker in ["Host(`", "HostSNI(`"] {
        let mut rest = text;
        while let Some(start) = rest.find(marker) {
            rest = &rest[start + marker.len()..];
            let Some(end) = rest.find('`') else { break };
            let host = normalize_host(&rest[..end]);
            if !host.is_empty() {
                out.push(host);
            }
            rest = &rest[end..];
        }
    }
    out
}

fn normalize_host(d: &str) -> String {
    d.split('/')
        .next()
        .unwrap_or("")
        .trim()
        .trim_end_matches('.')
        .to_ascii_lowercase()
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    std::fs::write(path, bytes).with_context(|| format!("write {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cert(main: &str, sans: &[&str]) -> serde_json::Value {
        serde_json::json!({
            "domain": { "main": main, "sans": sans },
            "certificate": "x",
            "key": "y",
            "Store": "default"
        })
    }

    fn store(certs: Vec<serde_json::Value>) -> serde_json::Value {
        serde_json::json!({
            "letsencrypt": { "Account": { "Email": "a@b.c" }, "Certificates": certs }
        })
    }

    fn mains(acme: &serde_json::Value) -> Vec<String> {
        acme["letsencrypt"]["Certificates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["domain"]["main"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn drops_only_unknown_hosts() {
        let mut acme = store(vec![
            cert("devcom.app", &[]),
            cert("foooh.ru", &[]),
            cert("API.Example.com", &[]),
            cert("old.example.com", &["kept.example.com"]),
            cert("*.wild.example.com", &[]),
        ]);
        let keep: HashSet<String> = ["devcom.app", "api.example.com", "kept.example.com"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let removed = prune_certificates(&mut acme, &keep);
        assert_eq!(removed, vec!["foooh.ru"]);
        assert_eq!(
            mains(&acme),
            vec![
                "devcom.app",
                "API.Example.com",
                "old.example.com",
                "*.wild.example.com"
            ]
        );
        assert_eq!(acme["letsencrypt"]["Account"]["Email"], "a@b.c");
    }

    #[test]
    fn path_domains_match_their_hostname() {
        assert_eq!(normalize_host("api.x.ru/v1"), "api.x.ru");
        assert_eq!(normalize_host("X.ru."), "x.ru");
    }

    #[test]
    fn extracts_hosts_from_rules() {
        let yml = "rule: \"Host(`a.com`) || Host(`B.com`) && PathPrefix(`/v1`)\"\n\
                   rule: \"HostSNI(`db.c.com`)\"";
        assert_eq!(hosts_in_rules(yml), vec!["a.com", "b.com", "db.c.com"]);
    }

    #[test]
    fn empty_keep_list_is_a_noop() {
        let dir = tempfile::tempdir().unwrap();
        let traefik = dir.path().join("traefik");
        std::fs::create_dir_all(&traefik).unwrap();
        let original = store(vec![cert("a.com", &[])]).to_string();
        std::fs::write(traefik.join("acme.json"), &original).unwrap();
        assert!(prune_acme_store(dir.path(), &[]).unwrap().is_empty());
        assert_eq!(
            std::fs::read_to_string(traefik.join("acme.json")).unwrap(),
            original
        );
    }

    #[test]
    fn prunes_on_disk_and_keeps_routed_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let traefik = dir.path().join("traefik");
        std::fs::create_dir_all(traefik.join("dynamic")).unwrap();
        std::fs::write(
            traefik.join("dynamic").join("_pier-platform.yml"),
            "rule: \"Host(`panel.example.com`)\"",
        )
        .unwrap();
        let original = store(vec![
            cert("svc.example.com", &[]),
            cert("panel.example.com", &[]),
            cert("gone.example.com", &[]),
        ])
        .to_string();
        std::fs::write(traefik.join("acme.json"), &original).unwrap();

        let removed = prune_acme_store(dir.path(), &["svc.example.com/api".to_string()]).unwrap();
        assert_eq!(removed, vec!["gone.example.com"]);

        let after: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(traefik.join("acme.json")).unwrap())
                .unwrap();
        assert_eq!(mains(&after), vec!["svc.example.com", "panel.example.com"]);
        assert_eq!(
            std::fs::read_to_string(traefik.join("acme.json.bak")).unwrap(),
            original
        );
    }
}
