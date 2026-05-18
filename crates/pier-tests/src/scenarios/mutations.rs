//! Mutation scenarios: dist-tag, deprecate, unpublish.
//!
//! `dist_tag_*` exercise the npm-protocol routes under `/-/package/{pkg}/dist-tags`.
//! `deprecate` + `unpublish_single` use Pier's panel API which is what the
//! UI uses internally — the protocol-level npm form is covered by the npm
//! CLI in the install matrix (P3).

use std::future::Future;
use std::pin::Pin;
use std::time::Instant;

use crate::scenario::{Scenario, ScenarioCtx, ScenarioResult};
use crate::scenarios::publish::{FIXTURE_VERSION, FLAT_PKG};

pub struct DistTagAdd;
pub struct DistTagRm;
pub struct Deprecate;
pub struct UnpublishSingle;

impl Scenario for DistTagAdd {
    fn name(&self) -> &'static str {
        "dist_tag_add"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let put_url = format!("{}-/package/{}/dist-tags/beta", ctx.registry_url, FLAT_PKG);
            let put = match ctx
                .http
                .put(&put_url)
                .header("Authorization", format!("Bearer {}", ctx.token))
                .header("Content-Type", "application/json")
                .body(format!("\"{FIXTURE_VERSION}\""))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("PUT: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            if !put.status().is_success() {
                return ScenarioResult::fail(
                    self.name(),
                    format!("PUT http={}", put.status()),
                    started.elapsed().as_millis(),
                );
            }

            let list_url = format!("{}-/package/{}/dist-tags", ctx.registry_url, FLAT_PKG);
            let tags: serde_json::Value = match ctx.rget(&list_url).send().await {
                Ok(r) => r.json().await.unwrap_or(serde_json::Value::Null),
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("GET dist-tags: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            if tags.get("beta").and_then(|v| v.as_str()) != Some(FIXTURE_VERSION) {
                return ScenarioResult::fail(self.name(), format!("tags={tags}"), ms);
            }
            ScenarioResult::pass(self.name(), "", ms)
        })
    }
}

impl Scenario for DistTagRm {
    fn name(&self) -> &'static str {
        "dist_tag_rm"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let url = format!("{}-/package/{}/dist-tags/beta", ctx.registry_url, FLAT_PKG);
            let r = match ctx
                .http
                .delete(&url)
                .header("Authorization", format!("Bearer {}", ctx.token))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("DELETE: {e}"),
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

impl Scenario for Deprecate {
    fn name(&self) -> &'static str {
        "deprecate"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let enc = urlencoding::encode(FLAT_PKG);
            let post_url = format!(
                "{}/api/v1/registry/packages/{enc}/versions/{FIXTURE_VERSION}/deprecate",
                ctx.base_url
            );
            let post = match ctx
                .http
                .post(&post_url)
                .header("Authorization", format!("Bearer {}", ctx.token))
                .header("Content-Type", "application/json")
                .body(r#"{"message":"pier-tests deprecate"}"#.to_string())
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("POST deprecate: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            if !post.status().is_success() {
                return ScenarioResult::fail(
                    self.name(),
                    format!("http={}", post.status()),
                    started.elapsed().as_millis(),
                );
            }

            let manifest_url = format!("{}{}/{}", ctx.registry_url, FLAT_PKG, FIXTURE_VERSION);
            let manifest: serde_json::Value = match ctx.rget(&manifest_url).send().await {
                Ok(r) => r.json().await.unwrap_or(serde_json::Value::Null),
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("GET manifest: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            let depr = manifest.get("deprecated").and_then(|v| v.as_str());
            if depr != Some("pier-tests deprecate") {
                return ScenarioResult::fail(self.name(), format!("deprecated={depr:?}"), ms);
            }
            ScenarioResult::pass(self.name(), "", ms)
        })
    }
}

impl Scenario for UnpublishSingle {
    fn name(&self) -> &'static str {
        "unpublish_single"
    }
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>> {
        Box::pin(async move {
            let started = Instant::now();
            let enc = urlencoding::encode(FLAT_PKG);
            let del_url = format!(
                "{}/api/v1/registry/packages/{enc}/versions/{FIXTURE_VERSION}",
                ctx.base_url
            );
            let del = match ctx
                .http
                .delete(&del_url)
                .header("Authorization", format!("Bearer {}", ctx.token))
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("DELETE: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            if !del.status().is_success() {
                return ScenarioResult::fail(
                    self.name(),
                    format!("http={}", del.status()),
                    started.elapsed().as_millis(),
                );
            }

            let check_url = format!("{}{}/{}", ctx.registry_url, FLAT_PKG, FIXTURE_VERSION);
            let check = match ctx.rget(&check_url).send().await {
                Ok(r) => r,
                Err(e) => {
                    return ScenarioResult::fail(
                        self.name(),
                        format!("verify GET: {e}"),
                        started.elapsed().as_millis(),
                    );
                }
            };
            let ms = started.elapsed().as_millis();
            if check.status().as_u16() != 404 {
                return ScenarioResult::fail(
                    self.name(),
                    format!("version still reachable: {}", check.status()),
                    ms,
                );
            }
            ScenarioResult::pass(self.name(), "", ms)
        })
    }
}
