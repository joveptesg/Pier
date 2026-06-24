//! HTTP endpoints for WireGuard mesh management.
//!
//! Scope of this file:
//!
//! * `GET  /api/v1/network/mesh` — current mesh config + peers.
//! * `PUT  /api/v1/network/mesh` — change subnet/port (mesh disabled only).
//! * `POST /api/v1/network/mesh/enable` — allocate IPs for every server
//!   and flip `enabled=1`. Does NOT install WireGuard or push configs —
//!   that's the job of a future provision/apply pass.
//! * `POST /api/v1/network/mesh/disable` — `wg-quick down` on every
//!   peer (best-effort, offline nodes flagged but don't block), then
//!   drop `wireguard_peers` and flip `enabled=0`. Optional
//!   `uninstall_helper` payload also purges the WireGuard package.
//!
//! Out of scope for now (lives in the next commit):
//!
//! * Driving `install_wireguard` / `generate_keypair` / `write_config`
//!   on each node via the agent's `/api/v1/agent/mesh/{op}` proxy.
//! * Switching `PIER_HOST` from public IP to the mesh IP.
//! * Auto-redeploying stacks with refreshed `extra_hosts`.
//!
//! Keeping the surface this thin makes the data model reviewable in
//! isolation; mistakes here only stall the wizard, they can't break a
//! live mesh because there isn't one yet.

use axum::extract::State;
use axum::response::IntoResponse;
use axum::Json;
use serde::Deserialize;

use crate::error::{AppError, AppResult};
use crate::network::mesh_call::{dispatch, MeshOpResult};
use crate::network::wireguard::{allocate_ip, render_wg_conf, MeshConfig, Peer, Subnet};
use crate::state::SharedState;

/// `GET /api/v1/network/mesh`
pub async fn get_mesh(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
    let peers = Peer::load_all(&db).map_err(AppError::Internal)?;
    Ok(Json(serde_json::json!({
        "config": {
            "enabled": cfg.enabled,
            "subnet": cfg.subnet,
            "listen_port": cfg.listen_port,
            "persistent_keepalive": cfg.persistent_keepalive,
            "updated_at": cfg.updated_at,
        },
        "peers": peers.iter().map(|p| serde_json::json!({
            "server_id": p.server_id,
            "server_name": p.server_name,
            "is_local": p.is_local,
            "assigned_ip": p.assigned_ip.to_string(),
            "public_key": p.public_key,
            "endpoint": p.endpoint,
            "status": p.status,
            "error_message": p.error_message,
            "last_handshake": p.last_handshake,
        })).collect::<Vec<_>>(),
    })))
}

#[derive(Deserialize)]
pub struct UpdateMeshRequest {
    /// CIDR. Validated server-side; only /30 or shorter prefixes allowed.
    #[serde(default)]
    pub subnet: Option<String>,
    /// UDP port for the WireGuard listener on each node.
    #[serde(default)]
    pub listen_port: Option<u16>,
    /// 0 to disable keepalive entirely (only safe if no node is behind NAT).
    #[serde(default)]
    pub persistent_keepalive: Option<u16>,
}

/// `PUT /api/v1/network/mesh`
///
/// Changing the subnet while peers exist would orphan their assigned IPs
/// and require a full re-roll, so we refuse mutation while `enabled=1`
/// or while any `wireguard_peers` rows survive. The UI surfaces this as
/// "disable mesh first to change subnet."
pub async fn put_mesh(
    State(state): State<SharedState>,
    Json(body): Json<UpdateMeshRequest>,
) -> AppResult<impl IntoResponse> {
    if let Some(ref s) = body.subnet {
        // Validate up front so we don't write garbage to the DB.
        Subnet::parse(s).map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.network.invalid_subnet",
                &[("error", &e.to_string())],
            ))
        })?;
    }
    if let Some(p) = body.listen_port {
        if p < 1024 {
            return Err(AppError::BadRequest(crate::i18n::te(
                "errors.network.listen_port_privileged",
            )));
        }
    }

    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
    let peer_count: i64 = db.query_row("SELECT COUNT(*) FROM wireguard_peers", [], |r| r.get(0))?;
    if cfg.enabled || peer_count > 0 {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.network.mesh_config_locked",
        )));
    }

    let now = chrono::Utc::now().timestamp();
    db.execute(
        "UPDATE wireguard_config
         SET subnet               = COALESCE(?1, subnet),
             listen_port          = COALESCE(?2, listen_port),
             persistent_keepalive = COALESCE(?3, persistent_keepalive),
             updated_at           = ?4
         WHERE id = 1",
        rusqlite::params![
            body.subnet,
            body.listen_port.map(|p| p as i64),
            body.persistent_keepalive.map(|k| k as i64),
            now,
        ],
    )?;

    Ok(Json(serde_json::json!({"ok": true})))
}

/// `POST /api/v1/network/mesh/enable`
///
/// Allocates a private /32 to every server (local + agents + peers) and
/// flips `enabled=1`. The next step — actually installing WireGuard on
/// each node and pushing wg0.conf — is a separate request so the
/// operator can review the IP plan before anything privileged runs.
pub async fn enable_mesh(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
    if cfg.enabled {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.network.mesh_already_enabled",
        )));
    }
    let subnet = Subnet::parse(&cfg.subnet).map_err(|e| {
        AppError::Internal(anyhow::anyhow!(
            "stored subnet {} is invalid: {e}",
            cfg.subnet
        ))
    })?;

    // Allocate inside a single transaction so a partial failure leaves
    // the DB in a clean "enabled=0, no peers" state.
    let tx = db
        .unchecked_transaction()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("begin tx: {e}")))?;

    // Already-allocated IPs (should be empty since we refused PUT while
    // peers exist, but read anyway to be defensive).
    let mut used: Vec<std::net::Ipv4Addr> = Vec::new();
    {
        let mut stmt = tx.prepare("SELECT assigned_ip FROM wireguard_peers")?;
        for row in stmt.query_map([], |r| r.get::<_, String>(0))? {
            if let Ok(ip) = row?.parse() {
                used.push(ip);
            }
        }
    }

    // List every server eligible for the mesh. `local` always gets the
    // first host so the operator can hard-code `https://10.42.0.1:PORT`
    // in env files and runbooks.
    let mut servers: Vec<(String, String, bool)> = Vec::new();
    {
        let mut stmt = tx.prepare(
            "SELECT id, host, is_local FROM servers ORDER BY is_local DESC, created_at ASC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)? != 0,
            ))
        })?;
        for row in rows {
            servers.push(row?);
        }
    }
    // Defensive sort: local first, others in deterministic order. The
    // `ORDER BY is_local DESC` already does this, but if a future migration
    // adds more `local` rows we don't want surprises.
    servers.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));

    let now = chrono::Utc::now().timestamp();
    for (server_id, host, _is_local) in &servers {
        let ip = allocate_ip(&subnet, &used).map_err(AppError::Internal)?;
        used.push(ip);
        // IPv6 hosts must be bracketed in the WireGuard `Endpoint =`
        // directive (`[fe80::1]:51820`). The helper handles both v4
        // and hostnames unchanged.
        let endpoint = crate::network::address::authority(host, cfg.listen_port.into());
        tx.execute(
            "INSERT INTO wireguard_peers
                (server_id, assigned_ip, endpoint, status, created_at)
             VALUES (?1, ?2, ?3, 'pending', ?4)",
            rusqlite::params![server_id, ip.to_string(), endpoint, now],
        )?;
    }

    tx.execute(
        "UPDATE wireguard_config SET enabled = 1, updated_at = ?1 WHERE id = 1",
        rusqlite::params![now],
    )?;
    tx.commit()
        .map_err(|e| AppError::Internal(anyhow::anyhow!("commit: {e}")))?;

    Ok(Json(serde_json::json!({
        "ok": true,
        "peers_allocated": servers.len(),
    })))
}

/// `POST /api/v1/network/mesh/configure`
///
/// Atomically install WireGuard on every node, mint per-node keypairs,
/// and push wg0.conf to bring the tunnel up. Sequenced so partial
/// failures stop early and leave a recoverable state:
///
/// 1. For each peer, in `wireguard_peers.assigned_ip` order: first
///    `install_wireguard` (idempotent apt-get install), then
///    `generate_keypair` — the helper returns (priv, pub); we persist
///    pub on `wireguard_peers.public_key` and hold priv in this
///    handler's stack until step 3.
/// 2. Render each peer's wg0.conf locally (peers without public_key
///    yet won't get a [Peer] block, but by now every row has one).
/// 3. For each peer, `write_config` then `up` (first activation). We
///    do this AFTER all keys are minted so the very first wg0.conf
///    written already lists every peer — no two-pass "first config
///    with no peers, second config with peers" round-trip.
///
/// Partial failure: on any step error for a peer, that peer's status
/// flips to 'error' with the helper's message, and the endpoint aborts.
/// Peers already marked 'active' keep their config (the helper's `up`
/// only fails if the kernel says so, which is rare and never a "I broke
/// myself" scenario). The operator clears errors by hitting
/// `disable` then `enable` and re-running configure.
///
/// **Private-key handling**: helper returns the private key over the
/// transport (HTTPS+Bearer for remote nodes, unix socket for local).
/// Core holds it for the duration of this handler only and renders it
/// straight into the wg0.conf text passed to `write_config`. Nothing is
/// persisted to disk by core; the only at-rest copy lives in
/// /etc/wireguard/wg0.conf on the target node (mode 0600 root). A
/// future improvement (Phase 0.3d) will move keypair generation behind
/// the helper so the private side never crosses the wire.
pub async fn configure_mesh(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    // Snapshot config + peers up front so we don't hold the DB lock
    // across the long-running helper round-trips.
    let (cfg, mut peers) = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let cfg = MeshConfig::load(&db).map_err(AppError::Internal)?;
        if !cfg.enabled {
            return Err(AppError::BadRequest(crate::i18n::te(
                "errors.network.enable_before_configure",
            )));
        }
        let peers = Peer::load_all(&db).map_err(AppError::Internal)?;
        if peers.is_empty() {
            return Err(AppError::BadRequest(crate::i18n::te(
                "errors.network.no_peers_allocated",
            )));
        }
        (cfg, peers)
    };

    let mut per_peer_result: Vec<serde_json::Value> = Vec::with_capacity(peers.len());

    // -- Phase 1: install + keypair per peer ----------------------------
    for peer in peers.iter_mut() {
        let sid = peer.server_id.clone();

        // Skip already-keyed peers. The on-node wg0.privkey is the durable
        // "is keyed" anchor (the helper's generate_keypair is idempotent);
        // the DB public_key is its core-side mirror. No core-RAM state.
        if peer.public_key.is_some() {
            per_peer_result.push(per_peer(&sid, "keyed", None));
            continue;
        }

        // 1a. install_wireguard
        match dispatch(&state, &sid, "install_wireguard", &serde_json::json!({})).await {
            Ok(MeshOpResult { ok: true, .. }) => {}
            Ok(other) => {
                let msg = other.error.unwrap_or_else(|| "install rejected".into());
                mark_error(&state, &sid, &format!("install_wireguard: {msg}"))?;
                per_peer_result.push(per_peer(&sid, "error", Some(&msg)));
                return Ok(Json(summary(per_peer_result, "aborted")));
            }
            Err(e) => {
                let msg = format!("install_wireguard transport: {e:#}");
                mark_error(&state, &sid, &msg)?;
                per_peer_result.push(per_peer(&sid, "error", Some(&msg)));
                return Ok(Json(summary(per_peer_result, "aborted")));
            }
        }

        // 1b. generate_keypair — already-keyed peers won't re-mint, but
        //     since we got here, this peer wasn't keyed by us yet.
        let kp = match dispatch(&state, &sid, "generate_keypair", &serde_json::json!({})).await {
            Ok(MeshOpResult {
                ok: true,
                result: Some(v),
                ..
            }) => v,
            Ok(other) => {
                let msg = other.error.unwrap_or_else(|| "no keypair returned".into());
                mark_error(&state, &sid, &format!("generate_keypair: {msg}"))?;
                per_peer_result.push(per_peer(&sid, "error", Some(&msg)));
                return Ok(Json(summary(per_peer_result, "aborted")));
            }
            Err(e) => {
                let msg = format!("generate_keypair transport: {e:#}");
                mark_error(&state, &sid, &msg)?;
                per_peer_result.push(per_peer(&sid, "error", Some(&msg)));
                return Ok(Json(summary(per_peer_result, "aborted")));
            }
        };

        let pub_key = kp
            .get("public_key")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                AppError::Internal(anyhow::anyhow!(
                    "helper returned no public_key field on generate_keypair"
                ))
            })?
            .to_string();

        // Persist pub_key + status='keyed'. The private key never leaves
        // the node — the helper keeps it in wg0.privkey.
        {
            let db = state
                .db
                .lock()
                .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
            db.execute(
                "UPDATE wireguard_peers
                 SET public_key = ?2, status = 'keyed', error_message = NULL
                 WHERE server_id = ?1",
                rusqlite::params![sid, pub_key],
            )?;
        }
        peer.public_key = Some(pub_key);
        per_peer_result.push(per_peer(&sid, "keyed", None));
    }

    // -- Phase 2: render + write_config + up per peer -------------------
    //
    // Every peer now has a public_key, so each rendered config lists
    // every other peer in one shot. No two-pass dance.
    for peer in &peers {
        let sid = peer.server_id.clone();
        let conf = render_wg_conf(peer, &peers, &cfg);

        // write_config — helper validates the directive whitelist and
        // rejects anything PreUp/PostUp/etc, so corrupt renders fail
        // closed rather than silently shelling out.
        match dispatch(
            &state,
            &sid,
            "write_config",
            &serde_json::json!({ "content": conf }),
        )
        .await
        {
            Ok(MeshOpResult { ok: true, .. }) => {}
            Ok(other) => {
                let msg = other
                    .error
                    .unwrap_or_else(|| "write_config rejected".into());
                mark_error(&state, &sid, &format!("write_config: {msg}"))?;
                per_peer_result.push(per_peer(&sid, "error", Some(&msg)));
                return Ok(Json(summary(per_peer_result, "aborted")));
            }
            Err(e) => {
                let msg = format!("write_config transport: {e:#}");
                mark_error(&state, &sid, &msg)?;
                per_peer_result.push(per_peer(&sid, "error", Some(&msg)));
                return Ok(Json(summary(per_peer_result, "aborted")));
            }
        }

        // up — first-time activation; on a re-run after partial failure
        // this is `wg-quick up wg0`, which is a no-op if already up but
        // returns non-zero exit, so we treat helper-level errors as
        // soft failures here only if the wg0 interface is already up.
        match dispatch(&state, &sid, "up", &serde_json::json!({})).await {
            Ok(MeshOpResult { ok: true, .. }) => {
                let now = chrono::Utc::now().timestamp();
                let db = state
                    .db
                    .lock()
                    .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
                db.execute(
                    "UPDATE wireguard_peers
                     SET status = 'active', error_message = NULL,
                         deployed_at = ?2
                     WHERE server_id = ?1",
                    rusqlite::params![sid, now],
                )?;
            }
            Ok(other) => {
                let msg = other.error.unwrap_or_else(|| "up rejected".into());
                mark_error(&state, &sid, &format!("up: {msg}"))?;
                per_peer_result.push(per_peer(&sid, "error", Some(&msg)));
                return Ok(Json(summary(per_peer_result, "aborted")));
            }
            Err(e) => {
                let msg = format!("up transport: {e:#}");
                mark_error(&state, &sid, &msg)?;
                per_peer_result.push(per_peer(&sid, "error", Some(&msg)));
                return Ok(Json(summary(per_peer_result, "aborted")));
            }
        }

        // Replace or push the per-peer entry to reflect final state.
        if let Some(entry) = per_peer_result.iter_mut().find(|v| v["server_id"] == sid) {
            *entry = per_peer(&sid, "active", None);
        } else {
            per_peer_result.push(per_peer(&sid, "active", None));
        }
    }

    Ok(Json(summary(per_peer_result, "ok")))
}

fn per_peer(server_id: &str, status: &str, error: Option<&str>) -> serde_json::Value {
    serde_json::json!({
        "server_id": server_id,
        "status": status,
        "error": error,
    })
}

fn summary(per_peer: Vec<serde_json::Value>, overall: &str) -> serde_json::Value {
    serde_json::json!({
        "ok": overall == "ok",
        "overall": overall,
        "peers": per_peer,
    })
}

fn mark_error(state: &SharedState, server_id: &str, msg: &str) -> AppResult<()> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
    db.execute(
        "UPDATE wireguard_peers
         SET status = 'error', error_message = ?2
         WHERE server_id = ?1",
        rusqlite::params![server_id, msg],
    )?;
    Ok(())
}

/// `GET /api/v1/network/mesh/preflight`
///
/// Fans out a `helper.status` check to every non-local server registered
/// in `servers` and returns whether each one has a usable
/// `pier-net-helper`. The UI runs this before showing the "Enable Mesh"
/// button and refuses to proceed if any node returns `helper_available=
/// false`, so the operator can copy the retrofit command up front rather
/// than discovering the gap halfway through `configure_mesh`.
///
/// Why `status` and not a dedicated `preflight` op:
/// - `status` already returns `interface_up` + a populated `{}` even when
///   wg0 is down, *as long as the helper itself is reachable*. That's
///   exactly what "is the helper installed and listening" means.
/// - A transport error from `mesh_call::dispatch` (socket missing, agent
///   down) is what we map to `helper_available=false` here.
///
/// Local node uses the unix socket directly. Remote nodes go through
/// the agent's `/api/v1/agent/mesh/{op}` proxy, which is what the same
/// status panel uses post-enable — so a green preflight here is a strong
/// guarantee the actual provision pass will work.
pub async fn peer_preflight(State(state): State<SharedState>) -> AppResult<impl IntoResponse> {
    let servers = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let mut stmt = db
            .prepare("SELECT id, name, kind, is_local FROM servers ORDER BY is_local DESC, name")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)? != 0,
                ))
            })?
            .filter_map(|r| r.ok())
            .collect::<Vec<_>>();
        rows
    };

    let mut results = Vec::with_capacity(servers.len());
    for (id, name, kind, is_local) in servers {
        // Peer-kind nodes don't expose `/api/v1/agent/mesh/{op}` yet —
        // their helper installation path is the retrofit script. Mark
        // them as unknown so the UI surfaces the install command
        // without erroring out.
        if kind == "peer" && !is_local {
            results.push(serde_json::json!({
                "server_id": id,
                "name": name,
                "kind": kind,
                "is_local": is_local,
                "helper_available": false,
                "checked": false,
                "error": "peer-kind retrofit required — run /install-helper.sh on this node",
            }));
            continue;
        }

        let outcome = dispatch(&state, &id, "status", &serde_json::json!({})).await;
        let (available, error) = match outcome {
            Ok(MeshOpResult { ok: true, .. }) => (true, None),
            Ok(MeshOpResult {
                ok: false, error, ..
            }) => (false, error.or(Some("helper rejected status".into()))),
            Err(e) => (false, Some(format!("transport: {e:#}"))),
        };
        results.push(serde_json::json!({
            "server_id": id,
            "name": name,
            "kind": kind,
            "is_local": is_local,
            "helper_available": available,
            "checked": true,
            "error": error,
        }));
    }

    let all_ok = results
        .iter()
        .all(|r| r["helper_available"].as_bool().unwrap_or(false));
    Ok(Json(serde_json::json!({
        "ok": all_ok,
        "peers": results,
    })))
}

#[derive(Default, Deserialize)]
pub struct DisableMeshRequest {
    /// When true, also tell each helper to `apt-get purge wireguard` and
    /// drop `/etc/wireguard` after bringing the interface down. Off by
    /// default because operators usually want to re-enable later and
    /// keeping the package installed is harmless.
    #[serde(default)]
    pub uninstall_helper: bool,
}

/// `POST /api/v1/network/mesh/disable`
///
/// Brings the mesh down end-to-end:
///   1. Snapshot the current peer list (server_ids with any non-`pending`
///      status — these are the ones with a live `wg0`).
///   2. For each peer (including the local node), dispatch `helper.down`
///      and optionally `helper.uninstall` via [`mesh_call::dispatch`].
///      Failures are *not fatal* — a peer being offline shouldn't
///      block the operator from cleaning up DB state and re-enabling
///      later. The per-peer outcome is included in the response so
///      the UI can flag stragglers.
///   3. Drop every `wireguard_peers` row and flip
///      `wireguard_config.enabled=0`.
///
/// Returns `{ok, overall: "ok"|"partial", peers: [...]}` matching the
/// shape `configure_mesh` already uses so the UI doesn't need a second
/// rendering path.
pub async fn disable_mesh(
    State(state): State<SharedState>,
    body: Option<Json<DisableMeshRequest>>,
) -> AppResult<impl IntoResponse> {
    let req = body.map(|Json(r)| r).unwrap_or_default();

    // Snapshot before we hold any helper round-trips. We tear down even
    // peers stuck in 'pending' / 'keyed' — those still ran
    // `install_wireguard` and may have an interface, even if
    // configure_mesh aborted before the final `up`.
    let peers = {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        Peer::load_all(&db).map_err(AppError::Internal)?
    };

    let mut per_peer_result: Vec<serde_json::Value> = Vec::with_capacity(peers.len());
    let mut had_failure = false;

    for peer in &peers {
        let sid = peer.server_id.clone();
        // 1. bring the interface down
        let down = dispatch(&state, &sid, "down", &serde_json::json!({})).await;
        let down_ok = matches!(&down, Ok(MeshOpResult { ok: true, .. }));
        let down_msg = match &down {
            Ok(MeshOpResult { ok: true, .. }) => None,
            Ok(other) => other.error.clone(),
            Err(e) => Some(format!("transport: {e:#}")),
        };

        // 2. optional uninstall — only attempted when down succeeded;
        //    purging the package while wg0 is still up risks leaving
        //    the routing table dirty.
        let uninstall_msg = if req.uninstall_helper && down_ok {
            match dispatch(&state, &sid, "uninstall", &serde_json::json!({})).await {
                Ok(MeshOpResult { ok: true, .. }) => None,
                Ok(other) => other.error.clone().or(Some("uninstall rejected".into())),
                Err(e) => Some(format!("uninstall transport: {e:#}")),
            }
        } else {
            None
        };

        let status = if down_ok && uninstall_msg.is_none() {
            "torndown"
        } else {
            had_failure = true;
            "error"
        };
        let err = down_msg.or(uninstall_msg);
        per_peer_result.push(per_peer(&sid, status, err.as_deref()));
    }

    // 3. DB cleanup regardless — leaving stale rows around blocks a
    //    re-enable later, and a partial teardown is still "the mesh is
    //    not the source of truth anymore". Operators who want a clean
    //    retry can re-Enable to re-allocate IPs.
    let now = chrono::Utc::now().timestamp();
    {
        let db = state
            .db
            .lock()
            .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;
        let tx = db
            .unchecked_transaction()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("begin tx: {e}")))?;
        tx.execute("DELETE FROM wireguard_peers", [])?;
        tx.execute(
            "UPDATE wireguard_config SET enabled = 0, updated_at = ?1 WHERE id = 1",
            rusqlite::params![now],
        )?;
        tx.commit()
            .map_err(|e| AppError::Internal(anyhow::anyhow!("commit: {e}")))?;
    }

    let overall = if had_failure { "partial" } else { "ok" };
    Ok(Json(summary(per_peer_result, overall)))
}
