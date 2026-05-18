use std::path::Path;

use anyhow::Result;
use tokio::fs;
use tokio::process::Command;

use super::Client;

/// Yarn 1.22.x classic. Uses `.npmrc` for config — the default
/// `Client::write_config` path. Berry (2/3/4) lives in `YarnBerry` below.
pub struct Yarn1;

#[async_trait::async_trait]
impl Client for Yarn1 {
    fn id(&self) -> &'static str {
        "yarn1"
    }
    fn binary(&self) -> &'static str {
        "yarn"
    }
    async fn install(
        &self,
        workdir: &Path,
        package: &str,
        registry_url: &str,
    ) -> Result<std::process::Output> {
        let out = Command::new(self.binary())
            .current_dir(workdir)
            .arg("add")
            .arg(package)
            .arg("--registry")
            .arg(registry_url)
            .arg("--no-lockfile")
            .output()
            .await?;
        Ok(out)
    }
}

/// Yarn 2.x / 3.x / 4.x (berry). Wire format is the same across all three —
/// `.yarnrc.yml` with `npmRegistryServer` + `npmAuthToken` + (the critical
/// quirk) `npmAlwaysAuth: true` so the token is sent on GETs. Corepack
/// picks the right binary based on the `packageManager` field we drop into
/// package.json.
pub struct YarnBerry {
    id: &'static str,
    version: &'static str,
}

pub const YARN2: YarnBerry = YarnBerry {
    id: "yarn2",
    version: "2.4.3",
};
pub const YARN3: YarnBerry = YarnBerry {
    id: "yarn3",
    version: "3.8.7",
};
pub const YARN4: YarnBerry = YarnBerry {
    id: "yarn4",
    version: "4.5.1",
};

// Re-exports so the registry in `clients/mod.rs` can `Box::new(yarn::Yarn4)`.
#[allow(non_upper_case_globals)]
pub const Yarn2: YarnBerry = YARN2;
#[allow(non_upper_case_globals)]
pub const Yarn3: YarnBerry = YARN3;
#[allow(non_upper_case_globals)]
pub const Yarn4: YarnBerry = YARN4;

#[async_trait::async_trait]
impl Client for YarnBerry {
    fn id(&self) -> &'static str {
        self.id
    }
    fn binary(&self) -> &'static str {
        "yarn"
    }
    async fn write_config(
        &self,
        workdir: &Path,
        registry_url: &str,
        token: &str,
    ) -> std::io::Result<()> {
        let yarnrc = format!(
            "npmRegistryServer: \"{registry_url}\"\n\
             npmAuthToken: \"{token}\"\n\
             npmAlwaysAuth: true\n\
             nodeLinker: node-modules\n"
        );
        fs::write(workdir.join(".yarnrc.yml"), yarnrc).await?;
        let pkg_json = format!(
            "{{\"name\":\"pier-tests-harness\",\"version\":\"0.0.1\",\"private\":true,\"packageManager\":\"yarn@{}\"}}",
            self.version
        );
        fs::write(workdir.join("package.json"), pkg_json).await?;
        Ok(())
    }
    async fn install(
        &self,
        workdir: &Path,
        package: &str,
        _registry_url: &str,
    ) -> Result<std::process::Output> {
        // Berry reads its config from .yarnrc.yml — passing --registry on
        // the CLI is rejected as an unknown flag.
        let out = Command::new(self.binary())
            .current_dir(workdir)
            .arg("add")
            .arg(package)
            .output()
            .await?;
        Ok(out)
    }
}
