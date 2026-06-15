use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::HeaderMap;
use axum::response::IntoResponse;
use axum::Json;
use globset::{GlobBuilder, GlobSet, GlobSetBuilder};
use hmac::{Hmac, Mac};
use sha2::Sha256;

use crate::deploy::{self, CommitInfo};
use crate::error::{AppError, AppResult};
use crate::state::SharedState;

type HmacSha256 = Hmac<Sha256>;

/// How long the diff-fallback git fetch may run before we give up and treat the
/// push as "everything changed" (fail-safe). Keeps the webhook handler well
/// inside provider delivery timeouts even when the payload was truncated.
const DIFF_FALLBACK_TIMEOUT: Duration = Duration::from_secs(8);

/// POST /api/v1/webhooks/github — receive GitHub push webhook.
pub async fn github(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> AppResult<impl IntoResponse> {
    // Parse event type
    let event = headers
        .get("X-GitHub-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Handle installation events (GitHub App installed/updated)
    if event == "installation" || event == "installation_repositories" {
        let payload: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
            AppError::BadRequest(crate::i18n::te_args(
                "errors.webhooks.invalid_json",
                &[("error", &e.to_string())],
            ))
        })?;

        let action = payload["action"].as_str().unwrap_or("");
        if action == "created" || action == "added" {
            let installation_id = payload["installation"]["id"].as_i64().unwrap_or(0);
            let app_id = payload["installation"]["app_id"].as_i64().unwrap_or(0);

            if installation_id > 0 && app_id > 0 {
                if let Ok(db) = state.db.lock() {
                    let rows = db.execute(
                        "UPDATE git_sources SET installation_id = ?1 WHERE app_id = ?2 AND source_type = 'github-app'",
                        rusqlite::params![installation_id, app_id.to_string()],
                    ).unwrap_or(0);
                    if rows > 0 {
                        tracing::info!(
                            "GitHub App installation_id {installation_id} saved for app {app_id}"
                        );
                    }
                }
            }
        }

        return Ok(Json(
            serde_json::json!({"ok": true, "event": "installation"}),
        ));
    }

    if event != "push" {
        return Ok(Json(
            serde_json::json!({"ok": true, "skipped": "not a push event"}),
        ));
    }

    let signature = headers
        .get("X-Hub-Signature-256")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Parse payload
    let payload: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.webhooks.invalid_json",
            &[("error", &e.to_string())],
        ))
    })?;

    let repo_url = payload["repository"]["html_url"]
        .as_str()
        .or_else(|| payload["repository"]["clone_url"].as_str())
        .unwrap_or("");

    let full_ref = payload["ref"].as_str().unwrap_or("");
    let branch = full_ref.strip_prefix("refs/heads/").unwrap_or(full_ref);

    let commit_sha = payload["after"].as_str().unwrap_or("");
    let before = payload["before"].as_str();
    let commit_message = payload["head_commit"]["message"].as_str().unwrap_or("push");

    if repo_url.is_empty() || commit_sha.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.webhooks.missing_repo_or_sha",
        )));
    }

    let changed_from_payload = changed_paths_from_github(&payload);
    let sig = SignatureCheck::GitHub {
        body: &body,
        signature,
    };

    let resp = dispatch_push(
        &state,
        repo_url,
        branch,
        commit_sha,
        before,
        commit_message,
        changed_from_payload,
        sig,
    )
    .await?;
    Ok(Json(resp))
}

/// POST /api/v1/webhooks/gitlab — receive GitLab push webhook.
pub async fn gitlab(
    State(state): State<SharedState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> AppResult<impl IntoResponse> {
    let event = headers
        .get("X-Gitlab-Event")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if event != "Push Hook" {
        return Ok(Json(
            serde_json::json!({"ok": true, "skipped": "not a push event"}),
        ));
    }

    let gitlab_token = headers
        .get("X-Gitlab-Token")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    // Parse payload
    let payload: serde_json::Value = serde_json::from_slice(&body).map_err(|e| {
        AppError::BadRequest(crate::i18n::te_args(
            "errors.webhooks.invalid_json",
            &[("error", &e.to_string())],
        ))
    })?;

    let repo_url = payload["project"]["web_url"].as_str().unwrap_or("");

    let full_ref = payload["ref"].as_str().unwrap_or("");
    let branch = full_ref.strip_prefix("refs/heads/").unwrap_or(full_ref);

    let commit_sha = payload["after"].as_str().unwrap_or("");
    let before = payload["before"].as_str();
    let commit_message = payload["commits"]
        .as_array()
        .and_then(|c| c.last())
        .and_then(|c| c["message"].as_str())
        .unwrap_or("push");

    if repo_url.is_empty() || commit_sha.is_empty() {
        return Err(AppError::BadRequest(crate::i18n::te(
            "errors.webhooks.missing_repo_or_sha",
        )));
    }

    let changed_from_payload = changed_paths_from_gitlab(&payload);
    let sig = SignatureCheck::GitLab {
        token: gitlab_token,
    };

    let resp = dispatch_push(
        &state,
        repo_url,
        branch,
        commit_sha,
        before,
        commit_message,
        changed_from_payload,
        sig,
    )
    .await?;
    Ok(Json(resp))
}

/// Per-service webhook authenticity check. The secret is stored per service, so
/// under monorepo fan-out we verify the SAME push body/token against EACH
/// candidate service's secret — only services whose secret validates (or that
/// have none configured) are eligible to deploy.
enum SignatureCheck<'a> {
    GitHub { body: &'a [u8], signature: &'a str },
    GitLab { token: &'a str },
}

impl SignatureCheck<'_> {
    fn verify(&self, secret: &str) -> bool {
        match self {
            SignatureCheck::GitHub { body, signature } => {
                verify_github_signature(secret, body, signature).is_ok()
            }
            SignatureCheck::GitLab { token } => *token == secret,
        }
    }
}

/// The set of repo-relative paths a push changed. `Everything` is the fail-safe
/// value used whenever we can't positively determine the change set (truncated
/// payload, force-push, missing diff base, fetch failure) — it makes every
/// matched service deploy, so we never silently skip a needed redeploy.
enum ChangedPaths {
    Known(HashSet<String>),
    Everything,
}

/// One service matched to an incoming push (by repo URL + branch).
struct ServiceMatch {
    id: String,
    webhook_secret: Option<String>,
    auto_deploy: bool,
    root_path: Option<String>,
    watch_paths: Option<String>,
    last_deployed_sha: Option<String>,
    #[allow(dead_code)]
    git_branch: Option<String>,
}

/// Fan out one push to every service backing the repo+branch.
///
/// For each matched service in order: verify the push against the service's own
/// secret, honor its `auto_deploy` flag, then deploy it only if the push
/// touched a path it watches. Returns a JSON summary of started/skipped
/// services (the detail is also useful for debugging monorepo path rules).
#[allow(clippy::too_many_arguments)]
async fn dispatch_push(
    state: &SharedState,
    repo_url: &str,
    branch: &str,
    after: &str,
    before: Option<&str>,
    message: &str,
    changed_from_payload: Option<HashSet<String>>,
    sig: SignatureCheck<'_>,
) -> AppResult<serde_json::Value> {
    let services = find_services_by_repo(state, repo_url, branch)?;
    if services.is_empty() {
        tracing::debug!("No matching service for {repo_url} branch {branch}");
        return Ok(serde_json::json!({"ok": true, "skipped": "no matching service"}));
    }

    // Compute the change set ONCE for the whole push (shared across services).
    let changed =
        compute_changed_paths(state, changed_from_payload, before, after, &services).await;

    let mut started: Vec<String> = Vec::new();
    let mut skipped: Vec<serde_json::Value> = Vec::new();

    for svc in services {
        // 1. Per-service signature — load-bearing for multi-tenant repos.
        let sig_ok = match svc.webhook_secret.as_deref() {
            Some(secret) if !secret.is_empty() => sig.verify(secret),
            _ => true, // no secret configured → accept (matches prior behavior)
        };
        if !sig_ok {
            skipped.push(serde_json::json!({"id": svc.id, "reason": "signature mismatch"}));
            continue;
        }

        // 2. auto_deploy toggle.
        if !svc.auto_deploy {
            skipped.push(serde_json::json!({"id": svc.id, "reason": "auto_deploy disabled"}));
            continue;
        }

        // 3. Path triggers — only redeploy if a watched path changed.
        if !service_wants(&svc, &changed) {
            skipped.push(serde_json::json!({"id": svc.id, "reason": "no watched path changed"}));
            continue;
        }

        // 4. Spawn an independent pipeline for this service.
        let commit = CommitInfo {
            sha: after.to_string(),
            message: message.to_string(),
            branch: branch.to_string(),
        };
        let state_clone = Arc::clone(state);
        let sid = svc.id.clone();
        tokio::spawn(async move {
            deploy::run_pipeline(state_clone, sid, commit, "webhook").await;
        });
        started.push(svc.id);
    }

    // Layer C: declared dependents of the directly-affected services redeploy
    // too. An explicit operator-declared edge IS the consent, so we don't
    // re-check each dependent's auto_deploy or webhook signature here. Only
    // git-backed dependents are deployable; the rest are skipped. Each redeploys
    // on its OWN branch with a synthetic `dep-…` marker (its source didn't
    // change — only a dependency did), so it never overwrites last_deployed_sha.
    let mut dependents: Vec<String> = Vec::new();
    if !started.is_empty() {
        for (dep_id, branch) in deployable_dependents(state, &started) {
            let short = &after[..after.len().min(8)];
            let commit = CommitInfo {
                sha: format!("dep-{short}"),
                message: "Redeployed: a declared dependency changed".to_string(),
                branch,
            };
            let state_clone = Arc::clone(state);
            let sid = dep_id.clone();
            tokio::spawn(async move {
                deploy::run_pipeline(state_clone, sid, commit, "dependency").await;
            });
            dependents.push(dep_id);
        }
    }

    Ok(serde_json::json!({
        "ok": true,
        "started": started,
        "dependents": dependents,
        "skipped": skipped,
    }))
}

/// Resolve the deployable dependents of the given seed services: the reverse-
/// dependency closure (excluding the seeds), filtered to services that have git
/// configured (others can't be redeployed from a push). Returns `(id, branch)`.
fn deployable_dependents(state: &SharedState, seeds: &[String]) -> Vec<(String, String)> {
    let db = match state.db.lock() {
        Ok(db) => db,
        Err(_) => return Vec::new(),
    };
    let all = match deploy::deps::expand_with_dependents(&db, seeds) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("dependency expansion failed: {e}");
            return Vec::new();
        }
    };
    let seed_set: HashSet<&String> = seeds.iter().collect();
    let mut out = Vec::new();
    for dep_id in all.iter() {
        if seed_set.contains(dep_id) {
            continue;
        }
        let row = db
            .query_row(
                "SELECT git_repo_url, git_branch FROM services WHERE id = ?1",
                [dep_id],
                |r| {
                    Ok((
                        r.get::<_, Option<String>>(0)?,
                        r.get::<_, Option<String>>(1)?,
                    ))
                },
            )
            .ok();
        if let Some((Some(url), branch)) = row {
            if !url.is_empty() {
                out.push((dep_id.clone(), branch.unwrap_or_else(|| "main".to_string())));
            }
        }
    }
    out
}

/// Find ALL services matching the given repo URL and branch.
///
/// A single repo can back many services (monorepo) — unlike the old single-row
/// lookup, this returns every match so the caller can fan out. URL is matched
/// in three normalized forms (`url`, stripped `.git`, re-suffixed `.git`).
fn find_services_by_repo(
    state: &SharedState,
    repo_url: &str,
    branch: &str,
) -> AppResult<Vec<ServiceMatch>> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    // Normalize URL: strip trailing .git
    let normalized = repo_url.trim_end_matches(".git");

    let mut stmt = db.prepare(
        "SELECT id, git_webhook_secret, auto_deploy, root_path, watch_paths, last_deployed_sha, git_branch
         FROM services
         WHERE (git_repo_url = ?1 OR git_repo_url = ?2 OR git_repo_url = ?3)
           AND (git_branch = ?4 OR git_branch IS NULL)",
    )?;

    let rows = stmt.query_map(
        rusqlite::params![repo_url, normalized, format!("{normalized}.git"), branch],
        |row| {
            Ok(ServiceMatch {
                id: row.get(0)?,
                webhook_secret: row.get(1)?,
                auto_deploy: row.get::<_, Option<bool>>(2)?.unwrap_or(true),
                root_path: row.get(3)?,
                watch_paths: row.get(4)?,
                last_deployed_sha: row.get(5)?,
                git_branch: row.get(6)?,
            })
        },
    )?;

    let mut out = Vec::new();
    for r in rows {
        out.push(r?);
    }
    Ok(out)
}

/// Resolve the change set for a push: trust the payload when it's complete,
/// otherwise fall back to a real `git diff`, and finally to `Everything`.
async fn compute_changed_paths(
    state: &SharedState,
    from_payload: Option<HashSet<String>>,
    before: Option<&str>,
    after: &str,
    services: &[ServiceMatch],
) -> ChangedPaths {
    // Primary: the webhook payload listed changed files and wasn't truncated.
    if let Some(paths) = from_payload {
        return ChangedPaths::Known(paths);
    }

    // Fallback: pick a usable diff base — payload `before`, else the most recent
    // last_deployed_sha among the matched services (they share repo+branch).
    let base = before
        .filter(|b| is_usable_base(b))
        .map(|b| b.to_string())
        .or_else(|| {
            services
                .iter()
                .find_map(|s| s.last_deployed_sha.clone())
                .filter(|b| is_usable_base(b))
        });

    let Some(base) = base else {
        return ChangedPaths::Everything;
    };
    let Some(any) = services.first() else {
        return ChangedPaths::Everything;
    };

    // Network fetch can be slow; bound it so the webhook responds in time. On
    // timeout or any failure → Everything (fail-safe).
    match tokio::time::timeout(
        DIFF_FALLBACK_TIMEOUT,
        git_diff_paths(state, &any.id, &base, after),
    )
    .await
    {
        Ok(Some(paths)) => ChangedPaths::Known(paths),
        _ => ChangedPaths::Everything,
    }
}

/// A diff base is usable only if it's a non-empty, non-all-zero hex SHA.
fn is_usable_base(sha: &str) -> bool {
    sha.len() >= 7 && sha.bytes().all(|b| b.is_ascii_hexdigit()) && !sha.chars().all(|c| c == '0')
}

/// Best-effort `git diff --name-only base..after` for a service's repo.
///
/// The deploy clone is `--depth 1` and lives elsewhere, so we fetch the two
/// endpoints into a throwaway dir using the service's authenticated clone URL.
/// Returns `None` on any failure (caller treats that as `Everything`).
async fn git_diff_paths(
    state: &SharedState,
    service_id: &str,
    base: &str,
    after: &str,
) -> Option<HashSet<String>> {
    let (clone_url, _branch) = deploy::resolve_clone_url_for_service(state, service_id)
        .await
        .ok()?;
    let tmp = state
        .config
        .data_dir
        .join("tmp")
        .join(format!("diff-{}", uuid::Uuid::new_v4()));
    let result = git_diff_in_tmp(&tmp, &clone_url, base, after).await;
    let _ = tokio::fs::remove_dir_all(&tmp).await;
    result
}

async fn git_diff_in_tmp(
    tmp: &Path,
    clone_url: &str,
    base: &str,
    after: &str,
) -> Option<HashSet<String>> {
    tokio::fs::create_dir_all(tmp).await.ok()?;

    if !git_in(tmp, &["init", "-q"]).await?.status.success() {
        return None;
    }
    if !git_in(tmp, &["remote", "add", "origin", clone_url])
        .await?
        .status
        .success()
    {
        return None;
    }

    // First try a tight fetch of just the two SHAs. Some servers refuse fetching
    // arbitrary SHAs (uploadpack.allowReachableSHA1InWant off) → retry deeper.
    let shallow = git_in(
        tmp,
        &["fetch", "--no-tags", "--depth", "1", "origin", base, after],
    )
    .await?;
    if !shallow.status.success() {
        let deep = git_in(
            tmp,
            &["fetch", "--no-tags", "--depth", "50", "origin", after],
        )
        .await?;
        if !deep.status.success() {
            return None;
        }
        let _ = git_in(
            tmp,
            &["fetch", "--no-tags", "--depth", "50", "origin", base],
        )
        .await;
    }

    let out = git_in(tmp, &["diff", "--name-only", base, after]).await?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    Some(
        text.lines()
            .map(|l| l.trim().to_string())
            .filter(|l| !l.is_empty())
            .collect(),
    )
}

async fn git_in(dir: &Path, args: &[&str]) -> Option<std::process::Output> {
    tokio::process::Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env(
            "HOME",
            std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string()),
        )
        .output()
        .await
        .ok()
}

/// Extract the changed-path set from a GitHub push payload.
///
/// Returns `None` (→ git-diff fallback) when the payload can't be trusted as
/// complete: no commits array and no head_commit, or `commits` hit GitHub's
/// 20-entry cap (we can't prove the rest didn't touch a watched path).
fn changed_paths_from_github(payload: &serde_json::Value) -> Option<HashSet<String>> {
    let head = &payload["head_commit"];
    match payload["commits"].as_array() {
        Some(commits) => {
            if commits.len() >= 20 {
                return None; // truncated — fall back to a real diff
            }
            let mut set = HashSet::new();
            for c in commits {
                collect_commit_files(c, &mut set);
            }
            if head.is_object() {
                collect_commit_files(head, &mut set);
            }
            Some(set)
        }
        None => {
            if head.is_object() {
                let mut set = HashSet::new();
                collect_commit_files(head, &mut set);
                Some(set)
            } else {
                None
            }
        }
    }
}

/// Extract the changed-path set from a GitLab push payload. Returns `None` when
/// `commits` is missing or truncated relative to `total_commits_count`.
fn changed_paths_from_gitlab(payload: &serde_json::Value) -> Option<HashSet<String>> {
    let commits = payload["commits"].as_array()?;
    if commits.len() >= 20 {
        return None;
    }
    if let Some(total) = payload["total_commits_count"].as_u64() {
        if total as usize > commits.len() {
            return None; // payload omitted some commits
        }
    }
    let mut set = HashSet::new();
    for c in commits {
        collect_commit_files(c, &mut set);
    }
    Some(set)
}

/// Union a single commit object's `added`/`modified`/`removed` arrays into `set`.
/// Both GitHub and GitLab use the same three field names with repo-relative
/// paths (no leading slash), so this works for either provider.
fn collect_commit_files(commit: &serde_json::Value, set: &mut HashSet<String>) {
    for key in ["added", "modified", "removed"] {
        if let Some(arr) = commit[key].as_array() {
            for p in arr {
                if let Some(s) = p.as_str() {
                    let s = s.trim();
                    if !s.is_empty() {
                        set.insert(s.to_string());
                    }
                }
            }
        }
    }
}

/// Decide whether a service should deploy given the push's change set.
///
/// Fail-safe by design: `Everything` always deploys; a service with no
/// effective globs (legacy single-service repo) always deploys; a glob that
/// fails to compile deploys (and warns) rather than silently skipping. We skip
/// ONLY when we positively know the change set AND it misses every watch glob.
fn service_wants(svc: &ServiceMatch, changed: &ChangedPaths) -> bool {
    match changed {
        ChangedPaths::Everything => true,
        ChangedPaths::Known(paths) => {
            let globs = effective_globs(svc);
            if globs.is_empty() {
                return true; // legacy: watch everything
            }
            match build_globset(&globs) {
                Ok(set) => paths.iter().any(|p| set.is_match(p)),
                Err(e) => {
                    tracing::warn!(
                        "service {} has an invalid watch glob ({e}); deploying to be safe",
                        svc.id
                    );
                    true
                }
            }
        }
    }
}

/// The watch globs that actually apply to a service: explicit `watch_paths`
/// lines if any, else derived from `root_path` (`{root}/**` + `{root}`), else
/// empty (= legacy match-all).
fn effective_globs(svc: &ServiceMatch) -> Vec<String> {
    if let Some(wp) = svc.watch_paths.as_deref() {
        let globs: Vec<String> = wp
            .lines()
            .map(|l| l.trim())
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .map(|l| l.to_string())
            .collect();
        if !globs.is_empty() {
            return globs;
        }
    }
    if let Some(rp) = svc.root_path.as_deref() {
        let rp = rp.trim().trim_matches('/');
        if !rp.is_empty() {
            return vec![format!("{rp}/**"), rp.to_string()];
        }
    }
    Vec::new()
}

/// Compile watch globs into a single GlobSet. `literal_separator(true)` gives
/// gitignore/CI semantics: `*` does not cross `/`, and `**` is required to
/// descend into subdirectories.
fn build_globset(globs: &[String]) -> Result<GlobSet, globset::Error> {
    let mut builder = GlobSetBuilder::new();
    for g in globs {
        let glob = GlobBuilder::new(g).literal_separator(true).build()?;
        builder.add(glob);
    }
    builder.build()
}

/// Verify GitHub webhook HMAC-SHA256 signature.
fn verify_github_signature(secret: &str, body: &[u8], signature: &str) -> AppResult<()> {
    let expected_prefix = "sha256=";
    let hex_sig = signature
        .strip_prefix(expected_prefix)
        .ok_or_else(|| AppError::Unauthorized)?;

    let sig_bytes = hex::decode(hex_sig).map_err(|_| AppError::Unauthorized)?;

    let mut mac =
        HmacSha256::new_from_slice(secret.as_bytes()).map_err(|_| AppError::Unauthorized)?;
    mac.update(body);

    mac.verify_slice(&sig_bytes)
        .map_err(|_| AppError::Unauthorized)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn svc(root: Option<&str>, watch: Option<&str>) -> ServiceMatch {
        ServiceMatch {
            id: "s1".into(),
            webhook_secret: None,
            auto_deploy: true,
            root_path: root.map(|s| s.into()),
            watch_paths: watch.map(|s| s.into()),
            last_deployed_sha: None,
            git_branch: None,
        }
    }

    fn known(paths: &[&str]) -> ChangedPaths {
        ChangedPaths::Known(paths.iter().map(|s| s.to_string()).collect())
    }

    #[test]
    fn github_payload_unions_commits_and_head() {
        let payload = json!({
            "commits": [
                {"added": ["a.txt"], "modified": [], "removed": []},
                {"added": [], "modified": ["services/api/main.rs"], "removed": []}
            ],
            "head_commit": {"added": [], "modified": ["README.md"], "removed": []}
        });
        let set = changed_paths_from_github(&payload).unwrap();
        assert!(set.contains("a.txt"));
        assert!(set.contains("services/api/main.rs"));
        assert!(set.contains("README.md"));
        assert_eq!(set.len(), 3);
    }

    #[test]
    fn github_payload_truncated_at_20_falls_back() {
        let commits: Vec<_> = (0..20)
            .map(|i| json!({"added": [format!("f{i}.txt")], "modified": [], "removed": []}))
            .collect();
        let payload = json!({ "commits": commits, "head_commit": {} });
        assert!(changed_paths_from_github(&payload).is_none());
    }

    #[test]
    fn github_payload_no_commits_no_head_falls_back() {
        let payload = json!({ "ref": "refs/heads/main" });
        assert!(changed_paths_from_github(&payload).is_none());
    }

    #[test]
    fn gitlab_payload_unions_commits() {
        let payload = json!({
            "total_commits_count": 2,
            "commits": [
                {"added": ["x"], "modified": [], "removed": []},
                {"added": [], "modified": [], "removed": ["y"]}
            ]
        });
        let set = changed_paths_from_gitlab(&payload).unwrap();
        assert!(set.contains("x"));
        assert!(set.contains("y"));
    }

    #[test]
    fn gitlab_payload_truncated_count_falls_back() {
        let payload = json!({
            "total_commits_count": 5,
            "commits": [ {"added": ["x"], "modified": [], "removed": []} ]
        });
        assert!(changed_paths_from_gitlab(&payload).is_none());
    }

    #[test]
    fn is_usable_base_rejects_zeros_and_empty() {
        assert!(!is_usable_base(""));
        assert!(!is_usable_base("0000000000000000000000000000000000000000"));
        assert!(is_usable_base("a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2"));
    }

    #[test]
    fn effective_globs_derives_from_root_path() {
        let globs = effective_globs(&svc(Some("services/bot"), None));
        assert_eq!(
            globs,
            vec!["services/bot/**".to_string(), "services/bot".to_string()]
        );
    }

    #[test]
    fn effective_globs_prefers_explicit_watch_paths() {
        let globs = effective_globs(&svc(
            Some("services/bot"),
            Some("libs/**\n# comment\nshared/x"),
        ));
        assert_eq!(globs, vec!["libs/**".to_string(), "shared/x".to_string()]);
    }

    #[test]
    fn effective_globs_empty_for_repo_root() {
        assert!(effective_globs(&svc(None, None)).is_empty());
        assert!(effective_globs(&svc(Some(""), Some("   "))).is_empty());
    }

    #[test]
    fn service_wants_everything_always_deploys() {
        assert!(service_wants(
            &svc(Some("services/web"), None),
            &ChangedPaths::Everything
        ));
    }

    #[test]
    fn service_wants_legacy_match_all() {
        // No root/watch → empty globs → deploys on any change.
        assert!(service_wants(&svc(None, None), &known(&["whatever.txt"])));
    }

    #[test]
    fn service_wants_root_path_scoping() {
        let bot = svc(Some("services/bot"), None);
        let web = svc(Some("services/web"), None);
        let changed = known(&["services/bot/src/main.rs"]);
        assert!(service_wants(&bot, &changed));
        assert!(!service_wants(&web, &changed));
    }

    #[test]
    fn service_wants_star_does_not_cross_slash() {
        // `services/bot/*` must NOT match a nested file (literal_separator).
        let s = svc(None, Some("services/bot/*"));
        assert!(!service_wants(&s, &known(&["services/bot/src/main.rs"])));
        assert!(service_wants(&s, &known(&["services/bot/Dockerfile"])));
    }

    #[test]
    fn service_wants_invalid_glob_is_failsafe() {
        // An unparseable glob deploys rather than silently skipping.
        let s = svc(None, Some("services/[unclosed"));
        assert!(service_wants(&s, &known(&["anything"])));
    }
}
