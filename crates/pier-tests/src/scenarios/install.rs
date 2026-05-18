//! Real-client install scenarios.
//!
//! For each `Client` returned by `clients::all()`, runs two scenarios:
//! `install_flat_{id}` and `install_scoped_{id}`. Probing happens lazily —
//! a missing binary marks both scenarios as `Skipped` (yellow), keeping the
//! matrix actionable on minimal hosts.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use tempfile::TempDir;

use crate::clients::{self, Client};
use crate::scenario::{Scenario, ScenarioCtx, ScenarioResult, Status};
use crate::scenarios::publish::{FLAT_PKG, SCOPED_PKG};

pub struct InstallFlat {
    client: Box<dyn Client>,
    name: String,
}
pub struct InstallScoped {
    client: Box<dyn Client>,
    name: String,
}

pub fn all() -> Vec<Box<dyn Scenario>> {
    let mut out: Vec<Box<dyn Scenario>> = Vec::new();
    for c in clients::all() {
        let id = c.id().to_string();
        let flat_name = format!("install_flat_{id}");
        let scoped_name = format!("install_scoped_{id}");
        // We can't clone trait objects, so build two fresh client objects
        // per id. `clients::all()` is cheap (returns small structs).
        let flat_client = clients::all()
            .into_iter()
            .find(|x| x.id() == id)
            .expect("client list stable across calls");
        let scoped_client = clients::all()
            .into_iter()
            .find(|x| x.id() == id)
            .expect("client list stable across calls");
        out.push(Box::new(InstallFlat {
            client: flat_client,
            name: flat_name,
        }));
        out.push(Box::new(InstallScoped {
            client: scoped_client,
            name: scoped_name,
        }));
        // Drop the original probe so we don't leak the Box.
        drop(c);
    }
    out
}

impl Scenario for InstallFlat {
    fn name(&self) -> &'static str {
        leak(&self.name)
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(do_install(
            self.name(),
            self.client.as_ref(),
            ctx,
            FLAT_PKG,
            false,
        ))
    }
}

impl Scenario for InstallScoped {
    fn name(&self) -> &'static str {
        leak(&self.name)
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(do_install(
            self.name(),
            self.client.as_ref(),
            ctx,
            SCOPED_PKG,
            true,
        ))
    }
}

async fn do_install(
    name: &'static str,
    client: &dyn Client,
    ctx: &ScenarioCtx,
    package: &str,
    scoped: bool,
) -> ScenarioResult {
    let started = Instant::now();
    if clients::probe(client.binary()).await.is_none() {
        return ScenarioResult {
            name: name.to_string(),
            status: Status::Skipped,
            notes: format!("{} not on PATH", client.binary()),
            duration_ms: started.elapsed().as_millis(),
        };
    }
    let workdir = match TempDir::new() {
        Ok(d) => d,
        Err(e) => {
            return ScenarioResult::fail(
                name,
                format!("tempdir: {e}"),
                started.elapsed().as_millis(),
            );
        }
    };
    if let Err(e) = client
        .write_config(workdir.path(), &ctx.registry_url, &ctx.token)
        .await
    {
        return ScenarioResult::fail(name, format!("setup: {e}"), started.elapsed().as_millis());
    }
    let output = match client
        .install(workdir.path(), package, &ctx.registry_url)
        .await
    {
        Ok(o) => o,
        Err(e) => {
            return ScenarioResult::fail(
                name,
                format!("spawn: {e}"),
                started.elapsed().as_millis(),
            );
        }
    };
    let ms = started.elapsed().as_millis();
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let tail = stderr.lines().last().unwrap_or("").trim().to_string();
        return ScenarioResult::fail(name, format!("exit={:?} {tail}", output.status), ms);
    }
    // Confirm the package landed. pnpm uses a symlink farm under
    // node_modules/.pnpm — accept either layout.
    let nm = workdir.path().join("node_modules");
    // scoped/flat both land at node_modules/{package} when present — yarn 1
    // and pnpm sometimes use .pnpm symlink farm instead, which counts too.
    let _ = scoped;
    if nm.join(package).exists() || nm.join(".pnpm").exists() {
        let first_line = String::from_utf8_lossy(&output.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .to_string();
        ScenarioResult::pass(name, first_line, ms)
    } else {
        ScenarioResult::fail(name, format!("no node_modules/{package}"), ms)
    }
}

/// Leak a `String` into a `'static str`. We register each scenario name once
/// at startup — a few dozen 32-byte allocations live for the program's
/// lifetime, which is the right tradeoff vs. plumbing a name through the
/// trait as `String`.
fn leak(s: &str) -> &'static str {
    Box::leak(s.to_string().into_boxed_str())
}
