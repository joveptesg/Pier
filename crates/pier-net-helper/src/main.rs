//! pier-net-helper — privileged WireGuard helper for Pier.
//!
//! Runs as root on every node in the Pier mesh. Listens on a unix socket
//! (`/run/pier/net.sock`, mode `0660 root:pier`) and accepts a tightly
//! whitelisted set of operations from `pier-agent` / `pier-core` running
//! under the unprivileged `pier` user. The whitelist intentionally has no
//! `exec`, no `shell`, no arbitrary path — every op compiles down to a
//! fixed `Command::new("…")` invocation with caller-supplied arguments
//! limited to validated shapes.
//!
//! Wire protocol: line-delimited JSON over the unix socket. One connection
//! carries one request and gets one response, then closes. Schema:
//!
//! ```text
//! → {"id":"r1","op":"install_wireguard","params":{}}\n
//! ← {"id":"r1","ok":true,"result":{}}\n
//! ```
//!
//! Dead-man's switch: `apply` writes the new config, runs `wg syncconf`,
//! and schedules an auto-rollback to the previous config after
//! `rollback_after_sec` seconds. If `commit` arrives in time the rollback
//! is cancelled. This makes "applied a config that breaks my own
//! connectivity" recoverable without SSH: core's failure to send `commit`
//! is itself the signal that the new config is bad.
//!
//! Activation: `install.sh` drops the binary + systemd unit but does NOT
//! call `install_wireguard`. WireGuard is `apt install`ed lazily on the
//! first apply, kicked off from the UI's "Enable Mesh" wizard.

mod protocol;

#[cfg(unix)]
mod imp {
    use std::path::PathBuf;
    use std::sync::Arc;

    use anyhow::{anyhow, Context, Result};
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
    use tokio::net::{UnixListener, UnixStream};
    use tokio::sync::Mutex;
    use tokio::task::JoinHandle;
    use tracing::{error, info, warn};

    use crate::protocol::{
        inject_private_key, validate_wg_config, validate_wg_config_no_privkey, Op, Request,
        Response,
    };

    /// Default unix-socket path. Created by the helper; the systemd unit
    /// owns the parent `/run/pier` via `RuntimeDirectory=pier`.
    const DEFAULT_SOCKET_PATH: &str = "/run/pier/net.sock";

    /// Path WireGuard reads on `wg-quick up wg0`. Hard-coded — the helper
    /// only ever manages a single `wg0` interface on the Pier mesh.
    const WG_CONFIG_PATH: &str = "/etc/wireguard/wg0.conf";
    const WG_CONFIG_BAK_PATH: &str = "/etc/wireguard/wg0.conf.bak";
    /// Node-local WireGuard private key. Generated and kept here by the
    /// helper (mode 0600 root); the private half NEVER crosses the socket.
    /// `op_write_config` injects it into the `[Interface]` block locally.
    const WG_PRIVKEY_PATH: &str = "/etc/wireguard/wg0.privkey";

    /// Hard ceiling on the dead-man's switch so a caller can't park a
    /// rollback for hours and let a broken config sit. 10 minutes is more
    /// than enough for any legitimate apply-then-commit handshake.
    const MAX_ROLLBACK_AFTER_SEC: u64 = 600;

    /// Process-wide state. The mutex on `rollback` is the entire reason a
    /// helper is more than a glorified `sudoers` whitelist — only one
    /// in-flight rollback at a time, and `commit` / new `apply` cancels
    /// the previous one cleanly.
    struct HelperState {
        rollback: Mutex<Option<JoinHandle<()>>>,
    }

    impl HelperState {
        fn new() -> Self {
            Self {
                rollback: Mutex::new(None),
            }
        }
    }

    pub async fn run() -> Result<()> {
        tracing_subscriber::fmt()
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| "info".into()),
            )
            .init();

        let socket_path = std::env::var("PIER_NET_HELPER_SOCKET")
            .map(PathBuf::from)
            .unwrap_or_else(|_| PathBuf::from(DEFAULT_SOCKET_PATH));

        if socket_path.exists() {
            std::fs::remove_file(&socket_path)
                .with_context(|| format!("removing stale {}", socket_path.display()))?;
        }
        if let Some(parent) = socket_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating parent of {}", socket_path.display()))?;
        }

        let listener = UnixListener::bind(&socket_path)
            .with_context(|| format!("binding {}", socket_path.display()))?;

        // 0660 — root owns the socket, `pier` group reads/writes. The
        // pier-agent process must be in the `pier` group for its
        // `connect()` to succeed; world perms are explicitly off.
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&socket_path, std::fs::Permissions::from_mode(0o660))
            .with_context(|| format!("chmod 0660 {}", socket_path.display()))?;

        info!("pier-net-helper listening on {}", socket_path.display());

        let state = Arc::new(HelperState::new());

        loop {
            let (stream, _addr) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    warn!("accept failed: {e}");
                    continue;
                }
            };
            let st = state.clone();
            tokio::spawn(async move {
                if let Err(e) = handle_conn(stream, st).await {
                    warn!("connection error: {e}");
                }
            });
        }
    }

    async fn handle_conn(stream: UnixStream, state: Arc<HelperState>) -> Result<()> {
        let (read_half, mut write_half) = stream.into_split();
        let mut reader = BufReader::new(read_half);
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .context("reading request line")?;
        if n == 0 {
            return Ok(()); // peer closed without sending anything
        }

        let req: Request = match serde_json::from_str(line.trim_end()) {
            Ok(r) => r,
            Err(e) => {
                let resp = Response {
                    id: "",
                    ok: false,
                    result: None,
                    error: Some(format!("bad request: {e}")),
                };
                let mut bytes = serde_json::to_vec(&resp)?;
                bytes.push(b'\n');
                write_half.write_all(&bytes).await?;
                return Ok(());
            }
        };

        let id = req.id.clone();
        let resp = match dispatch(req.op, &state).await {
            Ok(value) => Response {
                id: &id,
                ok: true,
                result: Some(value),
                error: None,
            },
            Err(e) => Response {
                id: &id,
                ok: false,
                result: None,
                error: Some(format!("{e:#}")),
            },
        };

        let mut bytes = serde_json::to_vec(&resp)?;
        bytes.push(b'\n');
        write_half.write_all(&bytes).await?;
        Ok(())
    }

    async fn dispatch(op: Op, state: &Arc<HelperState>) -> Result<serde_json::Value> {
        match op {
            Op::InstallWireguard => op_install_wireguard().await,
            Op::GenerateKeypair => op_generate_keypair().await,
            Op::WriteConfig { content } => op_write_config(&content).await,
            Op::Apply { rollback_after_sec } => op_apply(state, rollback_after_sec).await,
            Op::Commit => op_commit(state).await,
            Op::Rollback => op_rollback(state).await,
            Op::Up => op_up().await,
            Op::Down => op_down().await,
            Op::Status => op_status().await,
            Op::Uninstall => op_uninstall(state).await,
        }
    }

    // ------------------------------------------------------------------
    // Op implementations.
    //
    // Each op:
    //   - validates inputs *before* running anything privileged
    //   - returns a typed JSON value the caller can deserialize
    //   - never logs the WireGuard private key (it shows up only inside
    //     the helper's response to GenerateKeypair, which travels over a
    //     0660 unix socket on the same host).
    // ------------------------------------------------------------------

    async fn op_install_wireguard() -> Result<serde_json::Value> {
        let status = tokio::process::Command::new("apt-get")
            .env("DEBIAN_FRONTEND", "noninteractive")
            .args(["install", "-y", "wireguard", "wireguard-tools"])
            .status()
            .await
            .context("spawning apt-get")?;
        if !status.success() {
            return Err(anyhow!(
                "apt-get install wireguard failed (exit {})",
                status.code().unwrap_or(-1)
            ));
        }
        Ok(serde_json::json!({}))
    }

    /// Derive the WireGuard public key from a private key via `wg pubkey`.
    /// The key is piped on stdin so it never touches argv (no `ps` leak).
    async fn derive_pubkey(private_key: &str) -> Result<String> {
        use std::process::Stdio;
        let mut child = tokio::process::Command::new("wg")
            .arg("pubkey")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawning `wg pubkey`")?;
        {
            let mut stdin = child
                .stdin
                .take()
                .ok_or_else(|| anyhow!("no stdin handle for wg pubkey"))?;
            stdin.write_all(private_key.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.shutdown().await?;
        }
        let out = child
            .wait_with_output()
            .await
            .context("waiting on `wg pubkey`")?;
        if !out.status.success() {
            return Err(anyhow!(
                "wg pubkey failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Ok(String::from_utf8(out.stdout)?.trim().to_string())
    }

    /// Generate (or reuse) this node's WireGuard keypair. The private key is
    /// persisted to `wg0.privkey` (0600 root) and NEVER returned — only the
    /// public half crosses the socket. Idempotent: a re-run after a partial
    /// configure returns the same public key instead of churning the key
    /// (which would invalidate every peer's view of this node).
    async fn op_generate_keypair() -> Result<serde_json::Value> {
        let existing = tokio::fs::read_to_string(WG_PRIVKEY_PATH)
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let private_key = if let Some(key) = existing {
            key
        } else {
            let priv_out = tokio::process::Command::new("wg")
                .arg("genkey")
                .output()
                .await
                .context("running `wg genkey`")?;
            if !priv_out.status.success() {
                return Err(anyhow!(
                    "wg genkey failed: {}",
                    String::from_utf8_lossy(&priv_out.stderr)
                ));
            }
            let key = String::from_utf8(priv_out.stdout)?.trim().to_string();
            if key.is_empty() {
                return Err(anyhow!("wg genkey produced empty output"));
            }
            // Persist atomically with 0600 before we ever return.
            use std::os::unix::fs::PermissionsExt;
            let new_path = format!("{WG_PRIVKEY_PATH}.new");
            tokio::fs::write(&new_path, format!("{key}\n"))
                .await
                .with_context(|| format!("writing {new_path}"))?;
            tokio::fs::set_permissions(&new_path, std::fs::Permissions::from_mode(0o600))
                .await
                .with_context(|| format!("chmod 0600 {new_path}"))?;
            tokio::fs::rename(&new_path, WG_PRIVKEY_PATH)
                .await
                .with_context(|| format!("renaming {new_path} → {WG_PRIVKEY_PATH}"))?;
            key
        };

        let public_key = derive_pubkey(&private_key).await?;
        Ok(serde_json::json!({ "public_key": public_key }))
    }

    async fn op_write_config(content: &str) -> Result<serde_json::Value> {
        // The core renders wg0.conf WITHOUT a PrivateKey line. Reject any
        // private key arriving over the wire (defense in depth), then inject
        // the node-local key from wg0.privkey before writing.
        validate_wg_config_no_privkey(content).context("rejecting wg0.conf")?;
        let private_key = tokio::fs::read_to_string(WG_PRIVKEY_PATH)
            .await
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!("no local WireGuard private key — run generate_keypair first")
            })?;
        let content =
            inject_private_key(content, &private_key).context("injecting node-local PrivateKey")?;
        // Belt and suspenders: the final text (now WITH the injected
        // PrivateKey) must still pass the full directive whitelist.
        validate_wg_config(&content).context("rejecting injected wg0.conf")?;

        // Atomic replace: write to wg0.conf.new, fsync via rename. This
        // matters because `wg-quick up` / `wg syncconf` read the file
        // unbuffered.
        let new_path = format!("{WG_CONFIG_PATH}.new");
        tokio::fs::write(&new_path, &content)
            .await
            .with_context(|| format!("writing {new_path}"))?;
        use std::os::unix::fs::PermissionsExt;
        tokio::fs::set_permissions(&new_path, std::fs::Permissions::from_mode(0o600))
            .await
            .with_context(|| format!("chmod 0600 {new_path}"))?;
        tokio::fs::rename(&new_path, WG_CONFIG_PATH)
            .await
            .with_context(|| format!("renaming {new_path} → {WG_CONFIG_PATH}"))?;
        Ok(serde_json::json!({}))
    }

    /// `wg syncconf wg0 <(wg-quick strip /etc/wireguard/wg0.conf)`,
    /// implemented without a shell. `wg syncconf` accepts a config path,
    /// so we generate the stripped form to a temp file and pass that.
    async fn wg_syncconf() -> Result<()> {
        let stripped = tokio::process::Command::new("wg-quick")
            .args(["strip", WG_CONFIG_PATH])
            .output()
            .await
            .context("running `wg-quick strip`")?;
        if !stripped.status.success() {
            return Err(anyhow!(
                "wg-quick strip failed: {}",
                String::from_utf8_lossy(&stripped.stderr)
            ));
        }
        let tmp = tempfile_path();
        tokio::fs::write(&tmp, stripped.stdout)
            .await
            .with_context(|| format!("writing stripped conf to {tmp}"))?;
        let status = tokio::process::Command::new("wg")
            .args(["syncconf", "wg0", &tmp])
            .status()
            .await
            .context("running `wg syncconf wg0`")?;
        let _ = tokio::fs::remove_file(&tmp).await;
        if !status.success() {
            return Err(anyhow!("wg syncconf exit {}", status.code().unwrap_or(-1)));
        }
        // `wg syncconf` updates peers/keys but — unlike `wg-quick up` — does
        // NOT manage routes. A peer added by syncconf (e.g. a paired core's
        // node on a re-configure) would have no route, so its /32 falls through
        // to the default gateway and traffic never enters the tunnel. Add the
        // AllowedIPs routes ourselves; `ip route replace` is idempotent.
        sync_routes().await;
        Ok(())
    }

    /// Ensure a `dev wg0` route exists for every peer AllowedIP. Best-effort:
    /// individual `ip route replace` failures (e.g. a malformed CIDR) are
    /// ignored so one bad peer can't strand the rest.
    async fn sync_routes() {
        let out = match tokio::process::Command::new("wg")
            .args(["show", "wg0", "allowed-ips"])
            .output()
            .await
        {
            Ok(o) if o.status.success() => o,
            _ => return, // interface gone or wg unavailable — nothing to do
        };
        let text = String::from_utf8_lossy(&out.stdout);
        for line in text.lines() {
            // "<pubkey>\t<cidr> <cidr> ..." — skip the pubkey, route each CIDR.
            for cidr in line.split_whitespace().skip(1) {
                if cidr == "(none)" {
                    continue;
                }
                let _ = tokio::process::Command::new("ip")
                    .args(["route", "replace", cidr, "dev", "wg0"])
                    .status()
                    .await;
            }
        }
    }

    /// Predictable, race-resistant temp path inside the helper's
    /// RuntimeDirectory. We don't use `/tmp` because the systemd unit
    /// gives us `PrivateTmp=true`, which would surprise debuggers
    /// expecting the file there.
    fn tempfile_path() -> String {
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        format!("/run/pier/wg-stripped-{ts}.conf")
    }

    async fn op_apply(
        state: &Arc<HelperState>,
        rollback_after_sec: u64,
    ) -> Result<serde_json::Value> {
        if rollback_after_sec == 0 || rollback_after_sec > MAX_ROLLBACK_AFTER_SEC {
            return Err(anyhow!(
                "rollback_after_sec must be 1..={MAX_ROLLBACK_AFTER_SEC}"
            ));
        }

        // Cancel any previous rollback task — it was bound to a config
        // that's about to be replaced and would otherwise undo this apply.
        cancel_pending_rollback(state).await;

        // Snapshot current config so we can roll back to it.
        match tokio::fs::copy(WG_CONFIG_PATH, WG_CONFIG_BAK_PATH).await {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // First-ever apply on this host: nothing to roll back to.
                // The rollback path will then "rollback to absent", i.e.
                // remove the new config and bring the interface down.
                let _ = tokio::fs::remove_file(WG_CONFIG_BAK_PATH).await;
            }
            Err(e) => {
                return Err(anyhow!("snapshotting wg0.conf: {e}"));
            }
        }

        wg_syncconf().await?;

        // Arm the rollback timer. The task fires after the timeout and
        // restores the .bak unless cancelled by a subsequent commit() or
        // new apply().
        let st = state.clone();
        let handle = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(rollback_after_sec)).await;
            warn!(
                "dead-man's switch firing: no commit within {rollback_after_sec}s, \
                 rolling back wg0.conf"
            );
            if let Err(e) = perform_rollback().await {
                error!("dead-man rollback failed: {e:#}");
            }
            let mut guard = st.rollback.lock().await;
            *guard = None;
        });

        *state.rollback.lock().await = Some(handle);
        Ok(serde_json::json!({
            "armed_rollback_after_sec": rollback_after_sec,
        }))
    }

    async fn op_commit(state: &Arc<HelperState>) -> Result<serde_json::Value> {
        cancel_pending_rollback(state).await;
        let _ = tokio::fs::remove_file(WG_CONFIG_BAK_PATH).await;
        Ok(serde_json::json!({}))
    }

    async fn op_rollback(state: &Arc<HelperState>) -> Result<serde_json::Value> {
        cancel_pending_rollback(state).await;
        perform_rollback().await?;
        Ok(serde_json::json!({}))
    }

    async fn perform_rollback() -> Result<()> {
        match tokio::fs::metadata(WG_CONFIG_BAK_PATH).await {
            Ok(_) => {
                tokio::fs::rename(WG_CONFIG_BAK_PATH, WG_CONFIG_PATH)
                    .await
                    .context("restoring wg0.conf.bak")?;
                wg_syncconf().await?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // No backup — this was a first-apply that hadn't been
                // committed. Remove the broken config and bring the
                // interface down.
                let _ = tokio::fs::remove_file(WG_CONFIG_PATH).await;
                let _ = tokio::process::Command::new("wg-quick")
                    .args(["down", "wg0"])
                    .status()
                    .await;
            }
            Err(e) => return Err(anyhow!("stat bak: {e}")),
        }
        Ok(())
    }

    async fn cancel_pending_rollback(state: &Arc<HelperState>) {
        let mut guard = state.rollback.lock().await;
        if let Some(handle) = guard.take() {
            handle.abort();
        }
    }

    async fn op_up() -> Result<serde_json::Value> {
        // If wg0 is already up, `wg-quick up` errors with "already exists".
        // That makes re-configuring a *running* mesh (e.g. adding a paired
        // core's nodes, or any second configure pass) abort. Detect the live
        // interface and apply the freshly written config via `wg syncconf`
        // instead, so `up` is idempotent and non-disruptive.
        let already_up = tokio::process::Command::new("wg")
            .args(["show", "wg0"])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false);
        if already_up {
            wg_syncconf().await?;
            return Ok(serde_json::json!({ "synced": true }));
        }
        let status = tokio::process::Command::new("wg-quick")
            .args(["up", "wg0"])
            .status()
            .await
            .context("running `wg-quick up wg0`")?;
        if !status.success() {
            return Err(anyhow!(
                "wg-quick up wg0 exit {}",
                status.code().unwrap_or(-1)
            ));
        }
        Ok(serde_json::json!({}))
    }

    async fn op_down() -> Result<serde_json::Value> {
        // `down` is best-effort: the interface may already be down after a
        // restart, and we don't want to error out in that case.
        let _ = tokio::process::Command::new("wg-quick")
            .args(["down", "wg0"])
            .status()
            .await;
        Ok(serde_json::json!({}))
    }

    async fn op_status() -> Result<serde_json::Value> {
        let out = tokio::process::Command::new("wg")
            .args(["show", "wg0", "dump"])
            .output()
            .await
            .context("running `wg show wg0 dump`")?;
        if !out.status.success() {
            // Most common case: interface is down. Surface that as a
            // structured "down" rather than a hard error so the UI can
            // distinguish "no mesh yet" from "wg crashed".
            let stderr = String::from_utf8_lossy(&out.stderr).to_string();
            return Ok(serde_json::json!({
                "interface_up": false,
                "stderr": stderr,
            }));
        }
        let text = String::from_utf8(out.stdout)?;
        Ok(serde_json::json!({
            "interface_up": true,
            "dump": text,
        }))
    }

    async fn op_uninstall(state: &Arc<HelperState>) -> Result<serde_json::Value> {
        cancel_pending_rollback(state).await;
        let _ = tokio::process::Command::new("wg-quick")
            .args(["down", "wg0"])
            .status()
            .await;
        let _ = tokio::fs::remove_file(WG_CONFIG_PATH).await;
        let _ = tokio::fs::remove_file(WG_CONFIG_BAK_PATH).await;
        // Leaving the mesh discards the node identity so a future re-enable
        // mints a fresh key. (op_down deliberately keeps wg0.privkey so a
        // disable/enable cycle preserves the node's identity.)
        let _ = tokio::fs::remove_file(WG_PRIVKEY_PATH).await;
        Ok(serde_json::json!({}))
    }
}

#[cfg(unix)]
#[tokio::main]
async fn main() -> anyhow::Result<()> {
    imp::run().await
}

#[cfg(not(unix))]
fn main() {
    eprintln!(
        "pier-net-helper is Linux-only — it talks to `wg`, `wg-quick`, \
         and systemd via a unix socket. Build on a Linux target."
    );
    std::process::exit(1);
}
