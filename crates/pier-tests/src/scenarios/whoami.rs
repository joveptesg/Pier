use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use crate::scenario::{Scenario, ScenarioCtx, ScenarioResult};

pub struct WhoamiAuth;
pub struct WhoamiNoAuth;

impl Scenario for WhoamiAuth {
    fn name(&self) -> &'static str {
        "whoami_auth"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let url = format!("{}-/whoami", ctx.registry_url);
            let r = match ctx.rget(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("request error: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            if !r.status().is_success() {
                return ScenarioResult::fail(self.name(), format!("http={}", r.status()), ms);
            }
            ScenarioResult::pass(self.name(), "", ms)
        })
    }
}

impl Scenario for WhoamiNoAuth {
    fn name(&self) -> &'static str {
        "whoami_noauth"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let url = format!("{}-/whoami", ctx.registry_url);
            let r = match ctx.http.get(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("request error: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            if r.status().as_u16() != 401 {
                return ScenarioResult::fail(
                    self.name(),
                    format!("expected 401, got {}", r.status()),
                    ms,
                );
            }
            ScenarioResult::pass(self.name(), "", ms)
        })
    }
}
