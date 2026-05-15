//! Wire-protocol types and pure validation logic.
//!
//! Kept out of `main.rs`'s `#[cfg(unix)]` block so it compiles — and is
//! testable — on Windows dev machines. The actual socket I/O and
//! sub-process invocations still live behind `cfg(unix)` because they
//! depend on `tokio::net::UnixListener` and `wg-quick`.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

/// All operations the helper accepts. Adding a new op means adding a new
/// variant here AND a new arm in the dispatcher — there is no catch-all
/// that runs arbitrary commands.
///
/// Wire shape (internally tagged): the `op` field selects the variant
/// and any extra fields are siblings, e.g.
/// `{"op":"apply","rollback_after_sec":60}` rather than
/// `{"op":"apply","params":{"rollback_after_sec":60}}`. Flatter to
/// produce by hand and easier to evolve when adding new ops.
#[derive(Debug, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Op {
    /// `apt-get install -y wireguard wireguard-tools`. Idempotent.
    InstallWireguard,
    /// `wg genkey | wg pubkey`. The private key never leaves the helper's
    /// response; callers store the public key and use the private key
    /// only as the `PrivateKey =` line in a subsequent `write_config`.
    GenerateKeypair,
    /// Write `wg0.conf` atomically. The content is validated against a
    /// whitelist of sections/directives before any disk write happens.
    WriteConfig { content: String },
    /// `wg syncconf` the live interface, with auto-rollback to the
    /// previous config if no `commit` arrives within
    /// `rollback_after_sec` seconds.
    Apply { rollback_after_sec: u64 },
    /// Cancel a pending rollback. Idempotent.
    Commit,
    /// Restore `wg0.conf.bak` immediately and `syncconf`. Cancels any
    /// pending rollback.
    Rollback,
    /// `wg-quick up wg0`. Used for first activation.
    Up,
    /// `wg-quick down wg0`.
    Down,
    /// `wg show wg0 dump`, parsed into a JSON shape.
    Status,
    /// `wg-quick down wg0` + remove the helper's config files. Does NOT
    /// `apt remove wireguard` — the operator may have other tunnels.
    Uninstall,
}

#[derive(Deserialize)]
pub struct Request {
    #[serde(default)]
    pub id: String,
    #[serde(flatten)]
    pub op: Op,
}

// `Response` is only referenced from the unix-only `imp` module. The
// fields are read by `serde_json::to_vec` via serialization, not by direct
// access, so `dead_code` lints on non-unix builds even though the type is
// load-bearing on Linux.
#[cfg_attr(not(unix), allow(dead_code))]
#[derive(Serialize)]
pub struct Response<'a> {
    pub id: &'a str,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Validate that `wg0.conf` content consists only of `[Interface]` /
/// `[Peer]` sections with whitelisted directive names. Anything exotic
/// — a `PreUp =` that executes arbitrary shell, an unknown section — is
/// rejected. This is the line between "helper" and "remote code exec":
/// without this check the helper would happily run any command an
/// attacker could fit into a `PostUp =` directive.
pub fn validate_wg_config(content: &str) -> Result<()> {
    const ALLOWED_INTERFACE: &[&str] =
        &["PrivateKey", "Address", "ListenPort", "DNS", "MTU", "Table"];
    const ALLOWED_PEER: &[&str] = &[
        "PublicKey",
        "PresharedKey",
        "AllowedIPs",
        "Endpoint",
        "PersistentKeepalive",
    ];

    #[derive(PartialEq)]
    enum Section {
        None,
        Interface,
        Peer,
    }
    let mut section = Section::None;

    for (lineno, raw) in content.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = match &line[1..line.len() - 1] {
                "Interface" => Section::Interface,
                "Peer" => Section::Peer,
                other => return Err(anyhow!("line {}: unknown section [{other}]", lineno + 1)),
            };
            continue;
        }
        let Some((key, _)) = line.split_once('=') else {
            return Err(anyhow!("line {}: not a key=value", lineno + 1));
        };
        let key = key.trim();
        let allowed = match section {
            Section::None => {
                return Err(anyhow!(
                    "line {}: directive outside any section",
                    lineno + 1
                ))
            }
            Section::Interface => ALLOWED_INTERFACE,
            Section::Peer => ALLOWED_PEER,
        };
        if !allowed.contains(&key) {
            return Err(anyhow!(
                "line {}: directive `{key}` not allowed in this section \
                 (helper rejects PreUp/PostUp/Script-style directives)",
                lineno + 1
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_accepts_well_formed_config() {
        let conf = "\
[Interface]
PrivateKey = abc
Address = 10.42.0.1/24
ListenPort = 51820

[Peer]
PublicKey = xyz
Endpoint = 1.2.3.4:51820
AllowedIPs = 10.42.0.2/32
PersistentKeepalive = 25
";
        validate_wg_config(conf).expect("should accept");
    }

    #[test]
    fn validate_rejects_preup_directive() {
        let conf = "[Interface]\nPrivateKey = abc\nPreUp = rm -rf /\n";
        let err = validate_wg_config(conf).expect_err("must reject");
        let msg = format!("{err:#}");
        assert!(msg.contains("PreUp"), "{msg}");
    }

    #[test]
    fn validate_rejects_unknown_section() {
        let conf = "[Beep]\nPrivateKey = abc\n";
        validate_wg_config(conf).expect_err("must reject");
    }

    #[test]
    fn validate_rejects_directive_outside_section() {
        let conf = "PrivateKey = abc\n";
        validate_wg_config(conf).expect_err("must reject");
    }

    #[test]
    fn validate_skips_comments_and_blank_lines() {
        let conf = "\
# leading comment
[Interface]
   # indented comment

PrivateKey = abc
";
        validate_wg_config(conf).expect("should accept");
    }

    #[test]
    fn op_deserialization_uses_snake_case_tag() {
        let r: Request = serde_json::from_str(r#"{"id":"r1","op":"install_wireguard"}"#).unwrap();
        assert_eq!(r.id, "r1");
        assert!(matches!(r.op, Op::InstallWireguard));
    }

    #[test]
    fn apply_op_carries_rollback_arg() {
        let r: Request =
            serde_json::from_str(r#"{"id":"r2","op":"apply","rollback_after_sec":60}"#).unwrap();
        match r.op {
            Op::Apply { rollback_after_sec } => assert_eq!(rollback_after_sec, 60),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn write_config_carries_inline_content() {
        let r: Request = serde_json::from_str(
            r#"{"id":"r3","op":"write_config","content":"[Interface]\nPrivateKey = abc\n"}"#,
        )
        .unwrap();
        match r.op {
            Op::WriteConfig { content } => assert!(content.starts_with("[Interface]")),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn rejects_directive_with_no_equals() {
        let conf = "[Interface]\nrandom text\n";
        validate_wg_config(conf).expect_err("must reject malformed line");
    }
}
