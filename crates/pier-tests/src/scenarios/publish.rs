//! Publish flat + scoped fixtures via the CouchDB-style PUT.
//!
//! These scenarios are also the pre-seed for every later packument/dist-tag
//! scenario — they run early and pass-only-on-success gates the rest.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use crate::fixture;
use crate::scenario::{Scenario, ScenarioCtx, ScenarioResult};

pub const FLAT_PKG: &str = "pier-tests-flat";
pub const SCOPED_PKG: &str = "@pier-tests/scoped";
pub const FIXTURE_VERSION: &str = "1.0.0";

pub struct PublishFlat;
pub struct PublishScoped;

impl Scenario for PublishFlat {
    fn name(&self) -> &'static str {
        "publish_flat"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move { do_publish(ctx, self.name(), FLAT_PKG).await })
    }
}

impl Scenario for PublishScoped {
    fn name(&self) -> &'static str {
        "publish_scoped"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move { do_publish(ctx, self.name(), SCOPED_PKG).await })
    }
}

async fn do_publish(ctx: &ScenarioCtx, name: &'static str, pkg: &str) -> ScenarioResult {
    let started = Instant::now();
    match fixture::publish(
        &ctx.http,
        &ctx.registry_url,
        &ctx.token,
        pkg,
        FIXTURE_VERSION,
    )
    .await
    {
        Ok(status) if status.is_success() => {
            ScenarioResult::pass(name, "", started.elapsed().as_millis())
        }
        Ok(status) => ScenarioResult::fail(
            name,
            format!("http={status}"),
            started.elapsed().as_millis(),
        ),
        Err(e) => ScenarioResult::fail(name, format!("error: {e}"), started.elapsed().as_millis()),
    }
}
