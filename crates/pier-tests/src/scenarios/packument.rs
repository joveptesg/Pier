//! Packument-shape scenarios:
//! - `abbreviated`: `Accept: vnd.npm.install-v1+json` returns a smaller body
//!   without `readme`/etc (PR 2 Bun-friendly mode).
//! - `dist_tags_kebab`: full packument uses the canonical `dist-tags` key.
//! - `time_created_modified`: `time.created` + `time.modified` are populated.
//! - `no_is_proxy_leak`: internal `is_proxy` flag never reaches the wire.
//! - `etag_packument`: ETag + `If-None-Match` → 304 Not Modified.

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use crate::scenario::{Scenario, ScenarioCtx, ScenarioResult};
use crate::scenarios::publish::FLAT_PKG;

pub struct Abbreviated;
pub struct DistTagsKebab;
pub struct TimeCreatedModified;
pub struct NoIsProxyLeak;
pub struct EtagPackument;

impl Scenario for Abbreviated {
    fn name(&self) -> &'static str {
        "abbreviated"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let url = format!("{}{}", ctx.registry_url, FLAT_PKG);
            let full = match ctx
                .rget(&url)
                .header("Accept", "application/json")
                .send()
                .await
            {
                Ok(r) => r.text().await.unwrap_or_default(),
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("full GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let abbr = match ctx
                .rget(&url)
                .header("Accept", "application/vnd.npm.install-v1+json")
                .send()
                .await
            {
                Ok(r) => r.text().await.unwrap_or_default(),
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("abbr GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            if abbr.contains("\"readme\"") {
                return ScenarioResult::fail(
                    self.name(),
                    "abbreviated payload still contains `readme`",
                    ms,
                );
            }
            if abbr.len() >= full.len() {
                return ScenarioResult::fail(
                    self.name(),
                    format!(
                        "no size reduction (full={}B abbr={}B)",
                        full.len(),
                        abbr.len()
                    ),
                    ms,
                );
            }
            ScenarioResult::pass(
                self.name(),
                format!("full={}B abbr={}B", full.len(), abbr.len()),
                ms,
            )
        })
    }
}

impl Scenario for DistTagsKebab {
    fn name(&self) -> &'static str {
        "dist_tags_kebab"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let url = format!("{}{}", ctx.registry_url, FLAT_PKG);
            let body: serde_json::Value = match ctx.rget(&url).send().await {
                Ok(r) => r.json().await.unwrap_or(serde_json::Value::Null),
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            if body.get("dist_tags").is_some() {
                return ScenarioResult::fail(
                    self.name(),
                    "snake_case `dist_tags` leaked alongside kebab",
                    ms,
                );
            }
            if body
                .get("dist-tags")
                .and_then(|v| v.get("latest"))
                .and_then(|v| v.as_str())
                .is_none()
            {
                return ScenarioResult::fail(self.name(), "missing dist-tags.latest", ms);
            }
            ScenarioResult::pass(self.name(), "", ms)
        })
    }
}

impl Scenario for TimeCreatedModified {
    fn name(&self) -> &'static str {
        "time_created_modified"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let url = format!("{}{}", ctx.registry_url, FLAT_PKG);
            let body: serde_json::Value = match ctx.rget(&url).send().await {
                Ok(r) => r.json().await.unwrap_or(serde_json::Value::Null),
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            let created = body
                .get("time")
                .and_then(|t| t.get("created"))
                .and_then(|v| v.as_str());
            let modified = body
                .get("time")
                .and_then(|t| t.get("modified"))
                .and_then(|v| v.as_str());
            match (created, modified) {
                (Some(c), Some(_)) => ScenarioResult::pass(self.name(), format!("created={c}"), ms),
                _ => ScenarioResult::fail(self.name(), "missing time.created/modified", ms),
            }
        })
    }
}

impl Scenario for NoIsProxyLeak {
    fn name(&self) -> &'static str {
        "no_is_proxy_leak"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let url = format!("{}{}", ctx.registry_url, FLAT_PKG);
            let body: serde_json::Value = match ctx.rget(&url).send().await {
                Ok(r) => r.json().await.unwrap_or(serde_json::Value::Null),
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            if body.get("is_proxy").is_some() {
                return ScenarioResult::fail(self.name(), "is_proxy present in response", ms);
            }
            ScenarioResult::pass(self.name(), "", ms)
        })
    }
}

impl Scenario for EtagPackument {
    fn name(&self) -> &'static str {
        "etag_packument"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let url = format!("{}{}", ctx.registry_url, FLAT_PKG);
            let r = match ctx.rget(&url).send().await {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let etag = match r
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|v| v.to_str().ok())
            {
                Some(s) => s.to_string(),
                None => {
                    return ScenarioResult::fail(
                        self.name(),
                        "no ETag header",
                        started.elapsed().as_millis(),
                    );
                }
            };
            let cond = match ctx.rget(&url).header("If-None-Match", &etag).send().await {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("conditional GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            if cond.status().as_u16() != 304 {
                return ScenarioResult::fail(
                    self.name(),
                    format!("expected 304, got {}", cond.status()),
                    ms,
                );
            }
            ScenarioResult::pass(self.name(), "304 confirmed", ms)
        })
    }
}
