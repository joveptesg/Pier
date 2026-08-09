//! Detect and repair Postgres services whose cluster lives in an **anonymous**
//! Docker volume.
//!
//! ## The failure this exists for
//!
//! A catalog template mounts one named volume per service. The Postgres images
//! also declare a `VOLUME` of their own, and where that volume sits moved
//! between major releases:
//!
//! | image              | `VOLUME`                   | `PGDATA`                        |
//! |--------------------|----------------------------|---------------------------------|
//! | postgres/postgis 16, 17 | `/var/lib/postgresql/data` | `/var/lib/postgresql/data`      |
//! | postgres/postgis 18     | `/var/lib/postgresql`      | `/var/lib/postgresql/18/docker` |
//!
//! When the template mounts `/var/lib/postgresql` and the image declares a
//! `VOLUME` *below* that path, Docker allocates an anonymous volume for the
//! image's path and it shadows the named one. The cluster then lives outside
//! the named volume: invisible to volume-level backups, orphaned by any
//! recreate, and destroyed outright by a removal that passes `v: true`.
//!
//! That is how the `masterbyclick` database was lost on 2026-08-09 — the
//! public-port toggle recreated the container and the anonymous PGDATA volume
//! went with it.
//!
//! ## The repair
//!
//! [`diagnose`] reports whether a container is in that state. [`plan_repair`]
//! turns a diagnosis into the concrete move: copy `PGDATA` into a
//! subdirectory of the named volume and pin `PGDATA` there via the service's
//! env, which makes the layout identical on every major version. The API layer
//! performs the copy and the redeploy; everything decided here is pure so it
//! can be tested without a Docker daemon.
//!
//! Nothing here deletes the original volume. After a successful migration the
//! old anonymous volume is still on disk and is the operator's rollback.

use bollard::models::{MountPoint, MountPointTypeEnum};

/// Subdirectory of the named volume that the repaired cluster moves into.
/// Deliberately not `data` (collides with the 16/17 image `VOLUME`) and not
/// `<major>/docker` (collides with 18's), so the target is stable across
/// upgrades of the underlying image.
pub const TARGET_PGDATA_DIRNAME: &str = "pgdata";

/// Fallback when neither the container nor the image declares `PGDATA`.
/// Matches the historical default of the official images.
const DEFAULT_PGDATA: &str = "/var/lib/postgresql/data";

/// Catalog ids this repair applies to.
pub const AFFECTED_CATALOG_IDS: [&str; 3] = ["postgresql", "postgis", "timescaledb"];

/// Whether `catalog_id` is a Postgres-family catalog service.
pub fn is_affected_catalog(catalog_id: &str) -> bool {
    AFFECTED_CATALOG_IDS.contains(&catalog_id)
}

/// Where the effective `PGDATA` value came from.
///
/// The distinction matters for [`needs_env_pin`]: a path the **image** picks is
/// re-derived on every deploy and needs no help, whereas one set on the
/// container must also be recorded in the service's env — otherwise the next
/// compose regeneration drops it and Postgres starts somewhere else.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PgdataSource {
    /// Explicit `PGDATA` on the container (compose `environment`).
    Container,
    /// Inherited from the image's own `PGDATA`.
    Image,
    /// Neither declared one; the historical default is assumed.
    Default,
}

/// Where a container's cluster currently lives, and whether that is safe.
#[derive(Debug, Clone, PartialEq)]
pub struct PgdataDiagnosis {
    /// Effective `PGDATA` of the running container.
    pub pgdata: String,
    /// Which layer set [`Self::pgdata`].
    pub pgdata_source: PgdataSource,
    /// Name of the volume that actually backs `PGDATA`, if any.
    pub backing_volume: Option<String>,
    /// True when the backing volume is anonymous — the unsafe case.
    pub backing_is_anonymous: bool,
    /// The named volume mounted at or above `PGDATA`, if the service has one.
    /// This is where a repair would move the cluster.
    pub named_volume: Option<NamedVolume>,
}

/// A named volume attached to the container, with its mount point.
#[derive(Debug, Clone, PartialEq)]
pub struct NamedVolume {
    pub name: String,
    pub mount_point: String,
}

/// What a repair would do. `None` from [`plan_repair`] means "nothing to do".
#[derive(Debug, Clone, PartialEq)]
pub struct RepairPlan {
    /// Volume to copy the cluster out of.
    pub source_volume: String,
    /// Path of the cluster inside `source_volume`.
    pub source_path_in_volume: String,
    /// Named volume to copy the cluster into.
    pub target_volume: String,
    /// Path of the cluster inside `target_volume` after the copy.
    pub target_path_in_volume: String,
    /// Value to persist as the service's `PGDATA` env var.
    pub target_pgdata: String,
}

/// Docker names anonymous volumes with 64 lowercase hex characters. Named
/// volumes coming from compose are `<project>_<key>`, which never matches.
fn is_anonymous_volume_name(name: &str) -> bool {
    name.len() == 64
        && name
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}

/// Read `PGDATA` out of a list of `KEY=VALUE` env strings.
fn pgdata_from_env(env: &[String]) -> Option<String> {
    env.iter()
        .find_map(|e| e.strip_prefix("PGDATA=").map(str::to_string))
        .filter(|v| !v.is_empty())
}

/// True when `ancestor` is `path` or a parent directory of it. Compares whole
/// path segments, so `/var/lib/postgresql` does not "contain" `/var/lib/postgresql-old`.
fn covers(ancestor: &str, path: &str) -> bool {
    let a = ancestor.trim_end_matches('/');
    let p = path.trim_end_matches('/');
    if a.is_empty() {
        return true; // "/" covers everything
    }
    p == a || p.starts_with(&format!("{a}/"))
}

/// Determine which volume actually backs the container's `PGDATA`.
///
/// `container_env` and `image_env` are the raw `KEY=VALUE` lists from
/// `ContainerConfig.Env` and the image config; the container's own value wins,
/// falling back to the image's, then to the historical default.
///
/// The backing volume is the mount whose destination covers `PGDATA` with the
/// **longest** path — a mount at `/var/lib/postgresql/data` shadows one at
/// `/var/lib/postgresql` for anything underneath it, exactly as Docker layers
/// them at runtime.
pub fn diagnose(
    container_env: &[String],
    image_env: &[String],
    mounts: &[MountPoint],
) -> PgdataDiagnosis {
    let (pgdata, pgdata_source) = match pgdata_from_env(container_env) {
        Some(v) => (v, PgdataSource::Container),
        None => match pgdata_from_env(image_env) {
            Some(v) => (v, PgdataSource::Image),
            None => (DEFAULT_PGDATA.to_string(), PgdataSource::Default),
        },
    };

    let mut backing: Option<(&str, &str)> = None; // (destination, volume name)
    let mut named: Option<NamedVolume> = None;

    for m in mounts {
        if m.typ != Some(MountPointTypeEnum::VOLUME) {
            continue;
        }
        let (Some(name), Some(dest)) = (m.name.as_deref(), m.destination.as_deref()) else {
            continue;
        };
        if name.is_empty() || dest.is_empty() || !covers(dest, &pgdata) {
            continue;
        }

        // Longest covering destination wins — that is the one Docker mounts
        // over the others.
        if backing.is_none_or(|(prev, _)| dest.len() > prev.len()) {
            backing = Some((dest, name));
        }

        // Track the deepest NAMED volume separately: it is the repair target.
        if !is_anonymous_volume_name(name)
            && named
                .as_ref()
                .is_none_or(|n| dest.len() > n.mount_point.len())
        {
            named = Some(NamedVolume {
                name: name.to_string(),
                mount_point: dest.to_string(),
            });
        }
    }

    let (backing_volume, backing_is_anonymous) = match backing {
        Some((_, name)) => (Some(name.to_string()), is_anonymous_volume_name(name)),
        None => (None, false),
    };

    PgdataDiagnosis {
        pgdata,
        pgdata_source,
        backing_volume,
        backing_is_anonymous,
        named_volume: named,
    }
}

/// Whether the service's stored env has to be updated to keep the current
/// layout after the next compose regeneration.
///
/// The cluster is already safe — it sits on a named volume — but the path that
/// puts it there lives only on the running container. `volumes:` and
/// `environment:` are rebuilt from the catalog template plus the service's
/// stored env on every deploy, so an unrecorded `PGDATA` silently reverts to
/// the image default and Postgres comes up on an empty directory.
///
/// This is the state a container left behind by a hand-edited compose file, or
/// by a repair whose env write did not land. `service_env_pgdata` is the value
/// currently stored for the service, if any.
pub fn needs_env_pin(d: &PgdataDiagnosis, service_env_pgdata: Option<&str>) -> bool {
    // Only meaningful when the path is the container's own choice: an
    // image-provided PGDATA is re-derived on every deploy.
    if d.pgdata_source != PgdataSource::Container || d.backing_is_anonymous {
        return false;
    }
    // Nothing to preserve unless a named volume is what backs it.
    if d.named_volume.is_none() || d.backing_volume.is_none() {
        return false;
    }
    service_env_pgdata != Some(d.pgdata.as_str())
}

/// Turn a diagnosis into a concrete repair, or `None` when there is nothing to
/// fix or nothing to fix it with.
///
/// Returns `None` when the cluster already sits on a named volume (the healthy
/// case, including Postgres 18 whose `VOLUME` matches the template mount), and
/// also when the container has no named volume at all — there would be nowhere
/// to move the data, and inventing a volume would hide a misconfigured service
/// rather than repair it.
pub fn plan_repair(d: &PgdataDiagnosis) -> Option<RepairPlan> {
    if !d.backing_is_anonymous {
        return None;
    }
    let source_volume = d.backing_volume.clone()?;
    let named = d.named_volume.as_ref()?;

    // The anonymous volume is mounted exactly at PGDATA, so the cluster sits at
    // its root. Copy it to <named mount>/pgdata.
    let target_pgdata = format!(
        "{}/{}",
        named.mount_point.trim_end_matches('/'),
        TARGET_PGDATA_DIRNAME
    );

    Some(RepairPlan {
        source_volume,
        source_path_in_volume: "/".to_string(),
        target_volume: named.name.clone(),
        target_path_in_volume: format!("/{TARGET_PGDATA_DIRNAME}"),
        target_pgdata,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vol(name: &str, dest: &str) -> MountPoint {
        MountPoint {
            typ: Some(MountPointTypeEnum::VOLUME),
            name: Some(name.to_string()),
            destination: Some(dest.to_string()),
            rw: Some(true),
            ..Default::default()
        }
    }

    fn bind(dest: &str) -> MountPoint {
        MountPoint {
            typ: Some(MountPointTypeEnum::BIND),
            name: Some(String::new()),
            destination: Some(dest.to_string()),
            rw: Some(true),
            ..Default::default()
        }
    }

    const ANON: &str = "5b80769d153726ed25e232bfb46166a013b9339c406888c0d84836c8d46274ba";
    const ANON2: &str = "aaaa769d153726ed25e232bfb46166a013b9339c406888c0d84836c8d4627400";

    #[test]
    fn anonymous_name_detection() {
        assert!(is_anonymous_volume_name(ANON));
        assert!(!is_anonymous_volume_name("pier-postgis_data"));
        assert!(!is_anonymous_volume_name(
            "qkwcog0cck8scog4wk8ksoog_postgres-data"
        ));
        // 63 chars, and uppercase hex — neither is Docker's generated form.
        assert!(!is_anonymous_volume_name(&ANON[..63]));
        assert!(!is_anonymous_volume_name(&ANON.to_uppercase()));
    }

    #[test]
    fn covers_compares_whole_segments() {
        assert!(covers("/var/lib/postgresql", "/var/lib/postgresql/data"));
        assert!(covers("/var/lib/postgresql", "/var/lib/postgresql"));
        assert!(covers("/var/lib/postgresql/", "/var/lib/postgresql/data"));
        // Prefix-but-not-parent must not match.
        assert!(!covers(
            "/var/lib/postgresql",
            "/var/lib/postgresql-old/data"
        ));
        assert!(!covers("/var/lib/postgresql/data", "/var/lib/postgresql"));
    }

    /// The exact production shape of the 2026-08-09 incident: postgis 17, the
    /// template mounts the parent, the image's VOLUME sits below it.
    #[test]
    fn detects_incident_layout_as_anonymous() {
        let d = diagnose(
            &[],
            &["PGDATA=/var/lib/postgresql/data".to_string()],
            &[
                vol("pier-postgis_data", "/var/lib/postgresql"),
                vol(ANON, "/var/lib/postgresql/data"),
            ],
        );
        assert_eq!(d.pgdata, "/var/lib/postgresql/data");
        assert_eq!(d.backing_volume.as_deref(), Some(ANON));
        assert!(d.backing_is_anonymous, "must flag the anonymous backing");

        let plan = plan_repair(&d).expect("repair must be possible");
        assert_eq!(plan.source_volume, ANON);
        assert_eq!(plan.target_volume, "pier-postgis_data");
        assert_eq!(plan.target_pgdata, "/var/lib/postgresql/pgdata");
    }

    /// Postgres 18: the image's VOLUME is the same path the template mounts, so
    /// the named volume backs PGDATA and nothing needs repairing.
    #[test]
    fn postgres_18_layout_is_healthy() {
        let d = diagnose(
            &[],
            &["PGDATA=/var/lib/postgresql/18/docker".to_string()],
            &[vol("pier-pg_data", "/var/lib/postgresql")],
        );
        assert_eq!(d.pgdata, "/var/lib/postgresql/18/docker");
        assert_eq!(d.backing_volume.as_deref(), Some("pier-pg_data"));
        assert!(!d.backing_is_anonymous);
        assert_eq!(plan_repair(&d), None, "healthy service must not be touched");
    }

    /// An already-repaired service: PGDATA pinned into the named volume. Must
    /// be a no-op so the button is idempotent and a second click is harmless.
    #[test]
    fn repaired_service_is_a_no_op() {
        let d = diagnose(
            &["PGDATA=/var/lib/postgresql/pgdata".to_string()],
            &["PGDATA=/var/lib/postgresql/data".to_string()],
            &[
                vol("pier-postgis_data", "/var/lib/postgresql"),
                // The now-unused anonymous volume may still be attached.
                vol(ANON, "/var/lib/postgresql/data"),
            ],
        );
        assert_eq!(d.pgdata, "/var/lib/postgresql/pgdata");
        assert_eq!(d.backing_volume.as_deref(), Some("pier-postgis_data"));
        assert!(!d.backing_is_anonymous);
        assert_eq!(plan_repair(&d), None);
    }

    #[test]
    fn container_env_overrides_image_env() {
        let d = diagnose(
            &["PGDATA=/custom/path".to_string()],
            &["PGDATA=/var/lib/postgresql/data".to_string()],
            &[],
        );
        assert_eq!(d.pgdata, "/custom/path");
    }

    #[test]
    fn falls_back_to_default_when_nothing_declares_pgdata() {
        let d = diagnose(&[], &[], &[]);
        assert_eq!(d.pgdata, DEFAULT_PGDATA);
        assert_eq!(d.backing_volume, None);
        assert!(!d.backing_is_anonymous);
        assert_eq!(plan_repair(&d), None);
    }

    /// Deepest mount wins: Docker layers the anonymous volume over the named
    /// one, so a shallower named mount must not be mistaken for the backing.
    #[test]
    fn deepest_covering_mount_is_the_backing_volume() {
        let d = diagnose(
            &["PGDATA=/var/lib/postgresql/data/inner".to_string()],
            &[],
            &[
                vol("named_outer", "/var/lib"),
                vol("named_mid", "/var/lib/postgresql"),
                vol(ANON, "/var/lib/postgresql/data"),
            ],
        );
        assert_eq!(d.backing_volume.as_deref(), Some(ANON));
        assert!(d.backing_is_anonymous);
        // Repair targets the deepest NAMED volume, not the outermost one.
        let plan = plan_repair(&d).unwrap();
        assert_eq!(plan.target_volume, "named_mid");
        assert_eq!(plan.target_pgdata, "/var/lib/postgresql/pgdata");
    }

    /// No named volume anywhere: we refuse rather than invent one, because a
    /// service with only anonymous storage is misconfigured in a way the
    /// operator needs to see.
    #[test]
    fn anonymous_only_service_has_no_repair_target() {
        let d = diagnose(
            &["PGDATA=/var/lib/postgresql/data".to_string()],
            &[],
            &[vol(ANON, "/var/lib/postgresql/data")],
        );
        assert!(d.backing_is_anonymous);
        assert_eq!(d.named_volume, None);
        assert_eq!(plan_repair(&d), None, "nowhere safe to move the cluster");
    }

    #[test]
    fn bind_mounts_are_not_repair_candidates() {
        // A bind-mounted PGDATA is the operator's own arrangement — host path,
        // their backups, not ours to move.
        let d = diagnose(
            &["PGDATA=/var/lib/postgresql/data".to_string()],
            &[],
            &[bind("/var/lib/postgresql/data")],
        );
        assert_eq!(d.backing_volume, None);
        assert!(!d.backing_is_anonymous);
        assert_eq!(plan_repair(&d), None);
    }

    #[test]
    fn unrelated_anonymous_volume_does_not_trigger_repair() {
        // buildkit-style anonymous cache volume on a path that has nothing to
        // do with PGDATA must be ignored.
        let d = diagnose(
            &["PGDATA=/var/lib/postgresql/pgdata".to_string()],
            &[],
            &[
                vol("pier-pg_data", "/var/lib/postgresql"),
                vol(ANON2, "/var/lib/buildkit"),
            ],
        );
        assert!(!d.backing_is_anonymous);
        assert_eq!(plan_repair(&d), None);
    }

    /// The state a hand-repaired service is left in: the container points at
    /// the named volume, but nothing recorded that in the service env, so the
    /// next compose regeneration would drop it.
    #[test]
    fn container_set_pgdata_missing_from_service_env_needs_pinning() {
        let d = diagnose(
            &["PGDATA=/var/lib/postgresql/pgdata".to_string()],
            &["PGDATA=/var/lib/postgresql/data".to_string()],
            &[vol("pier-postgis_data", "/var/lib/postgresql")],
        );
        assert_eq!(d.pgdata_source, PgdataSource::Container);
        assert!(!d.backing_is_anonymous, "data itself is already safe");
        assert_eq!(plan_repair(&d), None, "no copy needed");
        assert!(needs_env_pin(&d, None), "env must be pinned");
        assert!(needs_env_pin(&d, Some("/var/lib/postgresql/other")));
        assert!(
            !needs_env_pin(&d, Some("/var/lib/postgresql/pgdata")),
            "already recorded — nothing to do"
        );
    }

    /// Postgres 18 gets its PGDATA from the image, which is re-derived on every
    /// deploy. Writing it into the service env would be noise, not a fix.
    #[test]
    fn image_provided_pgdata_never_needs_pinning() {
        let d = diagnose(
            &[],
            &["PGDATA=/var/lib/postgresql/18/docker".to_string()],
            &[vol("pier-pg_data", "/var/lib/postgresql")],
        );
        assert_eq!(d.pgdata_source, PgdataSource::Image);
        assert!(!needs_env_pin(&d, None));
    }

    /// A cluster still in an anonymous volume needs the full copy, not an env
    /// tweak — pinning alone would leave the data exactly where it is at risk.
    #[test]
    fn at_risk_service_is_not_merely_an_env_pin() {
        let d = diagnose(
            &["PGDATA=/var/lib/postgresql/data".to_string()],
            &[],
            &[
                vol("pier-postgis_data", "/var/lib/postgresql"),
                vol(ANON, "/var/lib/postgresql/data"),
            ],
        );
        assert!(d.backing_is_anonymous);
        assert!(plan_repair(&d).is_some());
        assert!(!needs_env_pin(&d, None), "must be repaired by copying");
    }

    #[test]
    fn affected_catalog_ids() {
        assert!(is_affected_catalog("postgresql"));
        assert!(is_affected_catalog("postgis"));
        assert!(is_affected_catalog("timescaledb"));
        assert!(!is_affected_catalog("mysql"));
        assert!(!is_affected_catalog("redis"));
    }
}
