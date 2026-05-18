use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use crate::scenario::{Scenario, ScenarioCtx, ScenarioResult};

pub struct Ping;

impl Scenario for Ping {
    fn name(&self) -> &'static str {
        "ping"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let url = format!("{}-/ping", ctx.registry_url);
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
            if !r.status().is_success() {
                return ScenarioResult::fail(self.name(), format!("http={}", r.status()), ms);
            }
            ScenarioResult::pass(self.name(), "", ms)
        })
    }
}
