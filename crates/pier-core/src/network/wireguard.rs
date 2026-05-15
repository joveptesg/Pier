//! WireGuard mesh orchestration: subnet parsing, IP allocation, and
//! `wg0.conf` rendering.
//!
//! What lives here:
//!
//!   * Typed mirrors of the `wireguard_config` and `wireguard_peers`
//!     rows from migration 41.
//!   * A pure-Rust IPv4 CIDR parser and host iterator — we don't pull in
//!     a CIDR crate just to chop one subnet into /32 host addresses.
//!   * An allocator that hands out the lowest free host inside the
//!     subnet, skipping the network/broadcast addresses and anything
//!     already in `wireguard_peers.assigned_ip`.
//!   * A renderer that turns one peer (us) and a list of other peers
//!     into a valid `wg0.conf` text accepted by `pier-net-helper`'s
//!     `write_config` op.
//!
//! What doesn't live here:
//!
//!   * Talking to `pier-net-helper` — that's the agent's job, via the
//!     `/api/v1/agent/mesh/{op}` proxy.
//!   * Talking to the local helper from core — that goes through the
//!     same proxy but pointed at `servers.is_local = 1`'s record.
//!   * Orchestration state machines (install_wireguard → keypair →
//!     write_config → up). Those live in `api/network.rs` so the
//!     individual steps stay testable in isolation.

use anyhow::{anyhow, Context, Result};
use std::net::Ipv4Addr;

use rusqlite::{params, Connection, OptionalExtension};

/// Singleton row from `wireguard_config` (CHECK id = 1). Loaded by the
/// API handlers up-front so allocators and renderers don't need a
/// `Connection`.
#[derive(Debug, Clone)]
pub struct MeshConfig {
    pub enabled: bool,
    pub subnet: String,
    pub listen_port: u16,
    pub persistent_keepalive: u16,
    pub updated_at: i64,
}

impl MeshConfig {
    pub fn load(conn: &Connection) -> Result<Self> {
        let row = conn
            .query_row(
                "SELECT enabled, subnet, listen_port, persistent_keepalive, updated_at
                 FROM wireguard_config WHERE id = 1",
                [],
                |row| {
                    Ok(Self {
                        enabled: row.get::<_, i64>(0)? != 0,
                        subnet: row.get(1)?,
                        listen_port: row.get::<_, i64>(2)? as u16,
                        persistent_keepalive: row.get::<_, i64>(3)? as u16,
                        updated_at: row.get(4)?,
                    })
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("wireguard_config singleton missing — re-run migrations"))?;
        Ok(row)
    }
}

/// Mirror of one `wireguard_peers` row. `public_key` is `None` until the
/// node's helper has generated a keypair and reported the public half.
#[derive(Debug, Clone)]
pub struct Peer {
    pub server_id: String,
    pub server_name: String,
    pub is_local: bool,
    pub assigned_ip: Ipv4Addr,
    pub public_key: Option<String>,
    /// `<public-ip-or-hostname>:<listen-port>` — what other peers `Endpoint =`.
    pub endpoint: String,
    pub status: String,
    pub error_message: Option<String>,
    pub last_handshake: Option<i64>,
}

impl Peer {
    /// Load all peers joined against `servers` so callers get the name
    /// and is_local flag without a second query.
    pub fn load_all(conn: &Connection) -> Result<Vec<Peer>> {
        let mut stmt = conn.prepare(
            "SELECT wp.server_id, s.name, s.is_local, wp.assigned_ip, wp.public_key,
                    wp.endpoint, wp.status, wp.error_message, wp.last_handshake
             FROM wireguard_peers wp
             JOIN servers s ON s.id = wp.server_id
             ORDER BY wp.assigned_ip",
        )?;
        let rows = stmt
            .query_map([], |row| {
                let ip_str: String = row.get(3)?;
                let assigned_ip = ip_str.parse::<Ipv4Addr>().map_err(|e| {
                    rusqlite::Error::FromSqlConversionFailure(
                        3,
                        rusqlite::types::Type::Text,
                        Box::new(e),
                    )
                })?;
                Ok(Peer {
                    server_id: row.get(0)?,
                    server_name: row.get(1)?,
                    is_local: row.get::<_, i64>(2)? != 0,
                    assigned_ip,
                    public_key: row.get(4)?,
                    endpoint: row.get(5)?,
                    status: row.get(6)?,
                    error_message: row.get(7)?,
                    last_handshake: row.get(8)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }
}

// ---------------------------------------------------------------------------
// Subnet parsing + IP allocation
// ---------------------------------------------------------------------------

/// Parsed IPv4 CIDR. We deliberately don't accept anything past /30 — a
/// /31 has no host addresses and a /32 has one; neither is useful for a
/// mesh. A /24 holds 254 hosts, plenty for any sensible Pier cluster.
#[derive(Debug, Clone, Copy)]
pub struct Subnet {
    pub network: Ipv4Addr,
    pub prefix: u8,
}

impl Subnet {
    pub fn parse(s: &str) -> Result<Self> {
        let (addr, prefix) = s
            .split_once('/')
            .ok_or_else(|| anyhow!("missing /prefix in {s:?}"))?;
        let network: Ipv4Addr = addr
            .trim()
            .parse()
            .with_context(|| format!("bad IPv4 in {s:?}"))?;
        let prefix: u8 = prefix
            .trim()
            .parse()
            .with_context(|| format!("bad prefix in {s:?}"))?;
        if prefix > 30 {
            return Err(anyhow!(
                "prefix /{prefix} leaves no usable hosts; pick /30 or shorter"
            ));
        }

        // Normalise the address to the network base (zero the host bits)
        // so callers can be sloppy about whether they pass "10.42.0.0/24"
        // or "10.42.0.5/24" — both should mean the same subnet.
        let mask = u32::MAX.checked_shl((32 - prefix) as u32).unwrap_or(0);
        let network_u32 = u32::from(network) & mask;
        Ok(Self {
            network: Ipv4Addr::from(network_u32),
            prefix,
        })
    }

    /// First usable host address (network + 1). For /24 of 10.42.0.0/24,
    /// that's `10.42.0.1`. We assign this to the local core node so
    /// `core_url=https://10.42.0.1:8443` is stable.
    pub fn first_host(&self) -> Ipv4Addr {
        let n = u32::from(self.network);
        Ipv4Addr::from(n + 1)
    }

    /// Last usable host address (broadcast - 1).
    pub fn last_host(&self) -> Ipv4Addr {
        let host_bits = 32 - self.prefix as u32;
        let mask = u32::MAX.checked_shr(self.prefix as u32).unwrap_or(0);
        let _ = host_bits; // silence on platforms where the shr is folded away
        let broadcast = u32::from(self.network) | mask;
        Ipv4Addr::from(broadcast - 1)
    }

    /// Iterate every usable host inside the subnet, lowest first.
    pub fn hosts(&self) -> SubnetHosts {
        SubnetHosts {
            cur: u32::from(self.first_host()),
            end: u32::from(self.last_host()),
        }
    }
}

pub struct SubnetHosts {
    cur: u32,
    end: u32,
}

impl Iterator for SubnetHosts {
    type Item = Ipv4Addr;
    fn next(&mut self) -> Option<Self::Item> {
        if self.cur > self.end {
            return None;
        }
        let ip = Ipv4Addr::from(self.cur);
        self.cur = self.cur.wrapping_add(1);
        Some(ip)
    }
}

/// Hand out the lowest host address in `subnet` not already in `used`.
/// Errors when the subnet is exhausted — a 10.42.0.0/24 can hold up to
/// 253 peers, so this only fires on truly large clusters or a misuse of
/// the API (re-assigning without freeing).
pub fn allocate_ip(subnet: &Subnet, used: &[Ipv4Addr]) -> Result<Ipv4Addr> {
    for candidate in subnet.hosts() {
        if !used.contains(&candidate) {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "subnet {}/{} is exhausted (no free host addresses)",
        subnet.network,
        subnet.prefix
    ))
}

// ---------------------------------------------------------------------------
// wg0.conf rendering
// ---------------------------------------------------------------------------

/// Render a `wg0.conf` for `local`'s view of the mesh, listing every
/// other peer in `peers` (the local node is filtered out). Peers without
/// a `public_key` are skipped — they haven't reported their key yet, and
/// `wg-quick` would refuse a `[Peer]` block without one. The renderer
/// guarantees only directives in the helper's whitelist
/// (PrivateKey/Address/ListenPort/PublicKey/Endpoint/AllowedIPs/PersistentKeepalive),
/// so `pier-net-helper`'s `validate_wg_config` will always accept the
/// output.
///
/// `private_key` is the local node's WireGuard private key (the
/// counterpart to `local.public_key`). It's passed in separately so the
/// `Peer` struct can stay public-only.
pub fn render_wg_conf(
    local: &Peer,
    peers: &[Peer],
    config: &MeshConfig,
    private_key: &str,
) -> String {
    let mut out = String::with_capacity(256 + peers.len() * 192);
    out.push_str("[Interface]\n");
    out.push_str(&format!("PrivateKey = {private_key}\n"));
    out.push_str(&format!("Address = {}/32\n", local.assigned_ip));
    out.push_str(&format!("ListenPort = {}\n", config.listen_port));

    for peer in peers {
        if peer.server_id == local.server_id {
            continue;
        }
        let Some(pk) = peer.public_key.as_deref() else {
            continue;
        };
        out.push_str("\n[Peer]\n");
        out.push_str(&format!("# {}\n", peer.server_name));
        out.push_str(&format!("PublicKey = {pk}\n"));
        out.push_str(&format!("Endpoint = {}\n", peer.endpoint));
        out.push_str(&format!("AllowedIPs = {}/32\n", peer.assigned_ip));
        if config.persistent_keepalive > 0 {
            out.push_str(&format!(
                "PersistentKeepalive = {}\n",
                config.persistent_keepalive
            ));
        }
    }
    out
}

/// Persist a newly-allocated row into `wireguard_peers`. Called once per
/// server at "Enable Mesh" time and again for every new server added
/// while mesh is active. Currently unused at the call site — the
/// `enable_mesh` handler inlines its own INSERT inside a single
/// transaction — but kept here as the canonical place for callers that
/// add nodes one at a time (e.g. the future "Add server to mesh" flow).
#[allow(dead_code)]
pub fn insert_peer(
    conn: &Connection,
    server_id: &str,
    assigned_ip: Ipv4Addr,
    endpoint: &str,
) -> Result<()> {
    let now = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT INTO wireguard_peers
            (server_id, assigned_ip, endpoint, status, created_at)
         VALUES (?1, ?2, ?3, 'pending', ?4)",
        params![server_id, assigned_ip.to_string(), endpoint, now],
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests — pure subnet/render logic, no DB.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subnet_parses_and_normalises() {
        let s = Subnet::parse("10.42.0.5/24").unwrap();
        assert_eq!(s.network, Ipv4Addr::new(10, 42, 0, 0));
        assert_eq!(s.prefix, 24);
        assert_eq!(s.first_host(), Ipv4Addr::new(10, 42, 0, 1));
        assert_eq!(s.last_host(), Ipv4Addr::new(10, 42, 0, 254));
    }

    #[test]
    fn subnet_rejects_undersized_prefix() {
        assert!(Subnet::parse("10.42.0.0/31").is_err());
        assert!(Subnet::parse("10.42.0.0/32").is_err());
    }

    #[test]
    fn subnet_rejects_garbage() {
        assert!(Subnet::parse("nope").is_err());
        assert!(Subnet::parse("10.42.0.0").is_err());
        assert!(Subnet::parse("10.42.0.0/abc").is_err());
    }

    #[test]
    fn hosts_iter_skips_network_and_broadcast() {
        let s = Subnet::parse("10.42.0.0/30").unwrap();
        // /30 → 4 addrs, 2 hosts (.1 and .2)
        let hosts: Vec<_> = s.hosts().collect();
        assert_eq!(
            hosts,
            vec![Ipv4Addr::new(10, 42, 0, 1), Ipv4Addr::new(10, 42, 0, 2)]
        );
    }

    #[test]
    fn allocator_hands_out_lowest_free() {
        let s = Subnet::parse("10.42.0.0/24").unwrap();
        let used = vec![
            Ipv4Addr::new(10, 42, 0, 1),
            Ipv4Addr::new(10, 42, 0, 2),
            Ipv4Addr::new(10, 42, 0, 4),
        ];
        let next = allocate_ip(&s, &used).unwrap();
        assert_eq!(next, Ipv4Addr::new(10, 42, 0, 3));
    }

    #[test]
    fn allocator_errors_on_exhausted_subnet() {
        let s = Subnet::parse("10.42.0.0/30").unwrap();
        let used = vec![Ipv4Addr::new(10, 42, 0, 1), Ipv4Addr::new(10, 42, 0, 2)];
        assert!(allocate_ip(&s, &used).is_err());
    }

    #[test]
    fn render_emits_interface_block_only_when_no_keyed_peers() {
        let cfg = MeshConfig {
            enabled: true,
            subnet: "10.42.0.0/24".into(),
            listen_port: 51820,
            persistent_keepalive: 25,
            updated_at: 0,
        };
        let local = peer("a", "vps1", Ipv4Addr::new(10, 42, 0, 1));
        let pending = Peer {
            public_key: None,
            ..peer("b", "vps2", Ipv4Addr::new(10, 42, 0, 2))
        };
        let out = render_wg_conf(&local, &[local.clone(), pending], &cfg, "PRIV1");
        assert!(out.contains("[Interface]"));
        assert!(out.contains("PrivateKey = PRIV1"));
        assert!(out.contains("Address = 10.42.0.1/32"));
        assert!(out.contains("ListenPort = 51820"));
        assert!(!out.contains("[Peer]"), "pending peer must be skipped");
    }

    #[test]
    fn render_emits_one_peer_block_per_keyed_remote() {
        let cfg = MeshConfig {
            enabled: true,
            subnet: "10.42.0.0/24".into(),
            listen_port: 51820,
            persistent_keepalive: 25,
            updated_at: 0,
        };
        let local = peer("a", "vps1", Ipv4Addr::new(10, 42, 0, 1));
        let mut p2 = peer("b", "vps2", Ipv4Addr::new(10, 42, 0, 2));
        p2.public_key = Some("PUB2".into());
        let mut p3 = peer("c", "vps3", Ipv4Addr::new(10, 42, 0, 3));
        p3.public_key = Some("PUB3".into());
        let out = render_wg_conf(&local, &[local.clone(), p2, p3], &cfg, "PRIV1");
        assert_eq!(out.matches("[Peer]").count(), 2, "{out}");
        assert!(out.contains("PublicKey = PUB2"));
        assert!(out.contains("AllowedIPs = 10.42.0.2/32"));
        assert!(out.contains("PublicKey = PUB3"));
        assert!(out.contains("AllowedIPs = 10.42.0.3/32"));
        assert!(out.contains("PersistentKeepalive = 25"));
    }

    #[test]
    fn render_skips_self_even_when_keyed() {
        let cfg = MeshConfig {
            enabled: true,
            subnet: "10.42.0.0/24".into(),
            listen_port: 51820,
            persistent_keepalive: 25,
            updated_at: 0,
        };
        let mut local = peer("a", "vps1", Ipv4Addr::new(10, 42, 0, 1));
        local.public_key = Some("PUB1".into());
        let out = render_wg_conf(&local, &[local.clone()], &cfg, "PRIV1");
        assert!(
            !out.contains("[Peer]"),
            "self must never appear as a peer block: {out}"
        );
    }

    fn peer(id: &str, name: &str, ip: Ipv4Addr) -> Peer {
        Peer {
            server_id: id.into(),
            server_name: name.into(),
            is_local: false,
            assigned_ip: ip,
            public_key: None,
            endpoint: format!("198.51.100.{}:51820", ip.octets()[3]),
            status: "pending".into(),
            error_message: None,
            last_handshake: None,
        }
    }
}
