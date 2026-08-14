//! Single source of truth for the `pier-net-helper` systemd unit.
//!
//! The helper's unix socket inherits the creating process's egid, so the
//! `Group=` line in its unit is the *only* thing that decides whether
//! `/run/pier/net.sock` comes out `root:pier` (pier-core can connect) or
//! `root:root` (pier-core gets `EACCES`). Issue #9 happened because four
//! copies of that unit existed — `scripts/install.sh`,
//! `scripts/pier-net-helper.service`, [`super::super::api::install`]'s
//! retrofit script, and [`super::super::api::servers`]'s agent bootstrap —
//! and three of them said `Group=root`.
//!
//! So there is now exactly one copy: `scripts/pier-net-helper.service`,
//! embedded here at compile time. Every generated installer emits
//! [`HELPER_UNIT`] verbatim; `install.sh` copies the same file when it
//! ships next to it and otherwise falls back to a heredoc that the tests
//! below keep honest.

/// The canonical `pier-net-helper.service`, embedded from `scripts/`.
pub const HELPER_UNIT: &str = include_str!("../../../../scripts/pier-net-helper.service");

/// Heredoc delimiter used by generated installers. Deliberately unlikely
/// to appear in the unit body, and always quoted at the call site so the
/// shell does not expand `$` or backticks inside the unit.
pub const HELPER_UNIT_HEREDOC: &str = "PIER_HELPER_UNIT_EOF";

/// Shell snippet every installer must run *before* enabling the unit.
///
/// `Group=pier` makes systemd refuse to start the unit with
/// `status=216/GROUP` when the group does not exist. On a core node the
/// group normally arrives as a side effect of `useradd pier`, but that is
/// skipped on upgrades (the user already exists) and absent entirely on
/// agent-only nodes, which have no `pier` user at all.
pub const ENSURE_PIER_GROUP_SH: &str =
    "getent group pier >/dev/null 2>&1 || groupadd --system pier\n";

/// Renders `cat > /etc/systemd/system/pier-net-helper.service <<'EOF' … EOF`,
/// preceded by the group-creation guard. Shared by both generated
/// installers so they cannot drift apart again.
pub fn install_unit_sh() -> String {
    format!(
        "{ensure}cat > /etc/systemd/system/pier-net-helper.service <<'{eof}'\n\
         {unit}{eof}\n\
         chmod 644 /etc/systemd/system/pier-net-helper.service\n",
        ensure = ENSURE_PIER_GROUP_SH,
        eof = HELPER_UNIT_HEREDOC,
        unit = HELPER_UNIT,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `scripts/install.sh` ships a self-contained fallback heredoc for the
    /// `curl | bash` path, where the `.service` file may not be on disk.
    /// That copy is the one place duplication survives, so pin it here.
    const INSTALL_SH: &str = include_str!("../../../../scripts/install.sh");

    #[test]
    fn canonical_unit_grants_pier_group_access() {
        assert!(
            HELPER_UNIT.contains("\nGroup=pier\n"),
            "Group=pier is what makes the socket root:pier; without it pier-core gets EACCES"
        );
        assert!(
            !HELPER_UNIT.contains("\nGroup=root\n"),
            "Group=root regressed — this is issue #9"
        );
    }

    #[test]
    fn canonical_unit_can_apply_self_updates() {
        assert!(
            HELPER_UNIT.contains("/opt/pier/bin"),
            "ReadWritePaths must include /opt/pier/bin or Op::SelfUpdate hits EROFS"
        );
        assert!(
            HELPER_UNIT.contains("CAP_DAC_OVERRIDE"),
            "writing the pier-owned /opt/pier/bin needs CAP_DAC_OVERRIDE even as root"
        );
        assert!(
            HELPER_UNIT.contains("CAP_CHOWN"),
            "the socket-group self-repair chown() needs CAP_CHOWN in the bounding set"
        );
    }

    #[test]
    fn canonical_unit_is_installable() {
        assert!(
            HELPER_UNIT.contains("[Install]") && HELPER_UNIT.contains("WantedBy=multi-user.target"),
            "installers run `systemctl enable`, which is a no-op without [Install]"
        );
    }

    #[test]
    fn generated_installer_creates_the_group_before_the_unit() {
        let sh = install_unit_sh();
        let group_at = sh.find("groupadd --system pier").expect("groupadd missing");
        let unit_at = sh
            .find("cat > /etc/systemd/system")
            .expect("heredoc missing");
        assert!(
            group_at < unit_at,
            "the group must exist before the unit referencing it is written"
        );
        assert!(sh.contains("Group=pier"));
        assert!(sh.ends_with("chmod 644 /etc/systemd/system/pier-net-helper.service\n"));
    }

    #[test]
    fn install_sh_fallback_matches_the_canonical_unit() {
        // Spelled with a variable there (`$PIER_GROUP`), so match the
        // operation rather than the literal group name.
        assert!(
            INSTALL_SH.contains("groupadd --system"),
            "install.sh must create the pier group explicitly, not rely on useradd's side effect"
        );
        // Every [Service] directive of the canonical unit has to appear in
        // install.sh's fallback heredoc verbatim, or the two installers
        // produce different units again.
        for line in HELPER_UNIT.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') || line.starts_with('[') {
                continue;
            }
            assert!(
                INSTALL_SH.contains(line),
                "install.sh fallback is out of sync with scripts/pier-net-helper.service: \
                 missing `{line}`"
            );
        }
    }
}
