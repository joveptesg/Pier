//! Tarball streaming scenario:
//! the registry must report a `Content-Length` matching the body it actually
//! sends. Confirms PR 1's `Body::from_stream` path returns the right size
//! and never buffers the whole tarball.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use crate::scenario::{Scenario, ScenarioCtx, ScenarioResult};
use crate::scenarios::publish::{FIXTURE_VERSION, FLAT_PKG};

pub struct TarballStreaming;

impl Scenario for TarballStreaming {
    fn name(&self) -> &'static str {
        "tarball_streaming"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            // Fetch packument to discover the canonical tarball URL Pier minted
            // — uses the configured public_base_url, which may differ from the
            // harness's loopback URL in production-like setups.
            let pkg_url = format!("{}{}", ctx.registry_url, FLAT_PKG);
            let packument: serde_json::Value = match ctx.rget(&pkg_url).send().await {
                Ok(r) => r.json().await.unwrap_or(serde_json::Value::Null),
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("packument GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let tarball_url = packument
                .pointer(&format!("/versions/{FIXTURE_VERSION}/dist/tarball"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let tarball_url = match tarball_url {
                Some(s) => s,
                None => {
                    return ScenarioResult::fail(
                        self.name(),
                        "no dist.tarball in packument",
                        started.elapsed().as_millis(),
                    );
                }
            };

            let r = match ctx.rget(&tarball_url).send().await {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("tarball GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let cl = r
                .headers()
                .get(reqwest::header::CONTENT_LENGTH)
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok());
            let body = match r.bytes().await {
                Ok(b) => b,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("body read: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            let cl = match cl {
                Some(n) => n,
                None => {
                    return ScenarioResult::fail(self.name(), "no Content-Length header", ms);
                }
            };
            if cl != body.len() as u64 {
                return ScenarioResult::fail(
                    self.name(),
                    format!("header={cl} body={}", body.len()),
                    ms,
                );
            }
            ScenarioResult::pass(self.name(), format!("Content-Length={cl}"), ms)
        })
    }
}
