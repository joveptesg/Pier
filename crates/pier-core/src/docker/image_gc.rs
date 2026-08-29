//! Garbage collection for Docker images Pier no longer needs.
//!
//! `docker image prune -f` removes only *dangling* (untagged) images, but
//! nothing Pier builds is ever dangling: the deploy path tags every build
//! `pier-<service>:<sha>` and compose names its builds
//! `pier-<service>-<compose-service>:latest`. Deleting a service therefore
//! leaves its image behind — tagged, referenced by nothing, and invisible to
//! `prune` forever. On the first host this was measured on that accounted for
//! 7.6 GB of the 7.65 GB Docker reported as reclaimable.
//!
//! What this module removes is deliberately narrower than
//! `docker image prune -a`. An image survives if anything still plausibly
//! needs it:
//!
//! 1. a container references it, in any state — a stopped service is one
//!    click away from starting again;
//! 2. it is the `image` or `previous_image_tag` of a row in `services`, so
//!    the current deploy and its rollback target both stay reachable;
//! 3. it belongs to the compose project of a service that still exists —
//!    `pier-<slug>-<svc>` where the service's compose file really declares
//!    `<svc>`. This is what keeps a temporarily-down stack from needing a
//!    full rebuild, which is exactly what `-a` would cost.
//!
//! Every ambiguity resolves toward keeping the image. Missing an orphan
//! costs disk; deleting a live one costs a rebuild.

use anyhow::Result;
use bollard::query_parameters::{ListContainersOptions, ListImagesOptions};
use serde::Serialize;
use std::collections::HashSet;

use crate::state::SharedState;

/// One image in a scan result.
///
/// `size` is the image's *unique* size — bytes that actually go away when it
/// is removed — not its apparent size, which counts base layers shared with
/// images that are staying and would badly overstate the win.
#[derive(Debug, Clone, Serialize)]
pub struct ScannedImage {
    pub id: String,
    pub name: String,
    pub size: u64,
}

/// Split of the local image set into the two things the Cleanup panel offers
/// to remove. Both lists are sorted largest-first.
#[derive(Debug, Default, Serialize)]
pub struct ImageScan {
    /// Untagged `<none>` images — exactly what `docker image prune -f` removes.
    pub dangling: Vec<ScannedImage>,
    /// Tagged images nothing references any more.
    pub orphans: Vec<ScannedImage>,
}

impl ImageScan {
    pub fn dangling_bytes(&self) -> u64 {
        self.dangling.iter().map(|i| i.size).sum()
    }

    pub fn orphan_bytes(&self) -> u64 {
        self.orphans.iter().map(|i| i.size).sum()
    }
}

/// Outcome of removing one image, so a partial failure can be reported
/// instead of aborting the whole pass.
#[derive(Debug, Serialize)]
pub struct RemovalSummary {
    pub removed: usize,
    pub reclaimed: u64,
    pub errors: Vec<String>,
}

/// Classify every local image as dangling, orphaned, or in use.
///
/// Uses the Engine API rather than `docker system df`, so sizes come back as
/// exact byte counts instead of strings like `"7.647GB"` that have to be
/// parsed back — and so the numbers cannot drift from what
/// [`remove_orphan_images`] actually deletes.
pub async fn scan(state: &SharedState) -> Result<ImageScan> {
    let images = state
        .docker
        .list_images(Some(ListImagesOptions {
            all: false,
            // Needed for `shared_size`; without it the daemon returns -1 and
            // every image looks like it owns its base layers outright.
            shared_size: true,
            ..Default::default()
        }))
        .await?;

    let containers = state
        .docker
        .list_containers(Some(ListContainersOptions {
            all: true,
            ..Default::default()
        }))
        .await?;

    // Containers pin images by ID, but compose-created ones often record only
    // the name they were started from, so collect both.
    let mut in_use: HashSet<String> = HashSet::new();
    for c in &containers {
        if let Some(id) = &c.image_id {
            in_use.insert(id.clone());
        }
        if let Some(image) = &c.image {
            in_use.insert(image.clone());
            // A bare name means `:latest`, which is how repo_tags spells it.
            // Matching the implicit form rather than the whole repository
            // keeps `pier-svc:<old-sha>` collectable while `pier-svc:latest`
            // is running.
            if !image.contains(':') && !image.contains('@') {
                in_use.insert(format!("{image}:latest"));
            }
        }
    }

    let protected = load_protected(state)?;
    let mut scan = ImageScan::default();

    for img in images {
        // `shared_size` is -1 when the daemon declined to compute it; falling
        // back to the full size overstates the image rather than promising a
        // reclaim that will not materialise.
        let unique = if img.shared_size >= 0 {
            img.size - img.shared_size
        } else {
            img.size
        };
        let size = unique.max(0) as u64;

        let tags: Vec<&str> = img
            .repo_tags
            .iter()
            .map(|t| t.as_str())
            .filter(|t| *t != "<none>:<none>")
            .collect();

        if tags.is_empty() {
            scan.dangling.push(ScannedImage {
                id: img.id.clone(),
                name: "<none>:<none>".to_string(),
                size,
            });
            continue;
        }

        if in_use.contains(&img.id) || tags.iter().any(|t| in_use.contains(*t)) {
            continue;
        }
        if tags.iter().any(|t| protected.covers(t)) {
            continue;
        }

        scan.orphans.push(ScannedImage {
            id: img.id.clone(),
            name: tags[0].to_string(),
            size,
        });
    }

    scan.dangling.sort_by(|a, b| b.size.cmp(&a.size));
    scan.orphans.sort_by(|a, b| b.size.cmp(&a.size));
    Ok(scan)
}

/// Remove every image [`scan`] classifies as an orphan.
///
/// Re-scans rather than accepting a list of IDs: the panel that triggered
/// this may have been open for hours, and the caller must never be able to
/// name an arbitrary image for deletion.
///
/// Removal is not forced. `force: true` would delete an image out from under
/// a container that still references it — if the daemon refuses, the
/// protection rules missed something and that belongs in the response, not
/// under a bulldozer.
pub async fn remove_orphan_images(state: &SharedState) -> Result<RemovalSummary> {
    let orphans = scan(state).await?.orphans;

    let mut summary = RemovalSummary {
        removed: 0,
        reclaimed: 0,
        errors: Vec::new(),
    };

    for image in orphans {
        match super::images::remove_image(&state.docker, &image.id, false).await {
            Ok(()) => {
                tracing::info!("Image GC: removed {} ({} bytes)", image.name, image.size);
                summary.removed += 1;
                summary.reclaimed += image.size;
            }
            Err(e) => {
                tracing::warn!("Image GC: failed to remove {}: {e}", image.name);
                summary.errors.push(format!("{}: {e}", image.name));
            }
        }
    }

    Ok(summary)
}

/// Image names that must survive even with no container referencing them.
struct Protected {
    /// Exact tags from `services.image` / `services.previous_image_tag`.
    exact: HashSet<String>,
    /// `(compose project slug, compose file)` for every service that still
    /// exists, used to recognise compose-built images by name.
    projects: Vec<(String, String)>,
}

impl Protected {
    fn covers(&self, tag: &str) -> bool {
        if self.exact.contains(tag) {
            return true;
        }
        // `services.image` is sometimes stored without its `:latest`.
        let repo = tag.rsplit_once(':').map(|(r, _)| r).unwrap_or(tag);
        if self.exact.contains(repo) {
            return true;
        }

        self.projects.iter().any(|(slug, compose)| {
            let project = format!("pier-{slug}");
            if repo == project {
                return true;
            }
            // `pier-<slug>-<svc>` is compose's own naming for a build without
            // an explicit `image:`. Only honour it when the service's compose
            // file actually declares `<svc>`, otherwise a project named
            // `astro-back` would shield every leftover of a long-deleted
            // `astro-back-services`.
            match repo.strip_prefix(&format!("{project}-")) {
                Some(svc) => compose_declares_service(compose, svc),
                None => false,
            }
        })
    }
}

fn load_protected(state: &SharedState) -> Result<Protected> {
    let db = state
        .db
        .lock()
        .map_err(|e| anyhow::anyhow!("DB lock: {e}"))?;

    let mut stmt =
        db.prepare("SELECT name, image, previous_image_tag, compose_content FROM services")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
        ))
    })?;

    let mut exact: HashSet<String> = HashSet::new();
    let mut projects: Vec<(String, String)> = Vec::new();

    for row in rows {
        let (name, image, previous, compose) = row?;

        for tag in [image, previous].into_iter().flatten() {
            let tag = tag.trim();
            if !tag.is_empty() {
                exact.insert(tag.to_string());
            }
        }

        // Same slug rule the deploy path uses to build image tags and compose
        // project names, so the two stay in step.
        let slug = name.to_lowercase().replace(' ', "-");
        if !slug.is_empty() {
            projects.push((slug, compose.unwrap_or_default()));
        }
    }

    Ok(Protected { exact, projects })
}

/// Whether a compose file declares a top-level service named `svc`.
///
/// A deliberately small scanner rather than a YAML dependency: it only has to
/// recognise keys at the first indent level under `services:`, and it answers
/// "no" for anything it cannot parse — which keeps an unreadable compose file
/// from silently protecting every image on the host.
fn compose_declares_service(compose: &str, svc: &str) -> bool {
    let mut in_services = false;
    let mut service_indent: Option<usize> = None;

    for line in compose.lines() {
        let trimmed = line.trim_end();
        let body = trimmed.trim_start();
        if body.is_empty() || body.starts_with('#') {
            continue;
        }
        let indent = trimmed.len() - body.len();

        if indent == 0 {
            in_services = body == "services:";
            service_indent = None;
            continue;
        }
        if !in_services {
            continue;
        }

        // The first key under `services:` fixes the indent of every sibling;
        // deeper keys (`build:`, `networks:`) must not be mistaken for one.
        match service_indent {
            None => service_indent = Some(indent),
            Some(expected) if indent != expected => continue,
            Some(_) => {}
        }

        if let Some(name) = body.strip_suffix(':') {
            if name.trim() == svc {
                return true;
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    use super::compose_declares_service;

    const COMPOSE: &str = "services:\n\
                           \x20 api:\n\
                           \x20   image: pier-astro-back-api\n\
                           \x20   build:\n\
                           \x20     context: .\n\
                           \x20 celery-worker:\n\
                           \x20   restart: unless-stopped\n\
                           networks:\n\
                           \x20 pier-net:\n\
                           \x20   external: true\n";

    #[test]
    fn finds_declared_services() {
        assert!(compose_declares_service(COMPOSE, "api"));
        assert!(compose_declares_service(COMPOSE, "celery-worker"));
    }

    #[test]
    fn rejects_undeclared_services() {
        // The case this whole rule exists for: a leftover of a deleted
        // `astro-back-services` project must not be shielded by `astro-back`.
        assert!(!compose_declares_service(COMPOSE, "services-bot"));
        assert!(!compose_declares_service(COMPOSE, "bot"));
    }

    #[test]
    fn ignores_nested_and_out_of_section_keys() {
        // `build:` is nested under `api:`, `pier-net:` lives under `networks:`.
        assert!(!compose_declares_service(COMPOSE, "build"));
        assert!(!compose_declares_service(COMPOSE, "pier-net"));
    }

    #[test]
    fn empty_compose_protects_nothing() {
        assert!(!compose_declares_service("", "api"));
        assert!(!compose_declares_service("not yaml at all", "api"));
    }
}
