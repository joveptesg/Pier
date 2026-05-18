//! Scenario trait + result types.
//!
//! A scenario is a self-contained async test. It receives a `ScenarioCtx`
//! (registry URL + token + shared HTTP client) and returns a
//! `ScenarioResult` with pass/fail status. Scenarios run sequentially
//! against a single Pier instance.

use std::future::Future;
use std::pin::Pin;

#[derive(Clone)]
pub struct ScenarioCtx {
    /// Used by panel-API scenarios (deprecate/unpublish via `/api/v1/registry/...`)
    /// which land in P2. Allow until then.
    #[allow(dead_code)]
    pub base_url: String,
    pub registry_url: String,
    pub token: String,
    pub http: reqwest::Client,
}

impl ScenarioCtx {
    /// GET with the harness's Bearer token.
    pub fn rget(&self, url: &str) -> reqwest::RequestBuilder {
        self.http
            .get(url)
            .header("Authorization", format!("Bearer {}", self.token))
    }
}

#[derive(Debug, Clone)]
pub struct ScenarioResult {
    pub name: String,
    pub status: Status,
    pub notes: String,
    pub duration_ms: u128,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skipped,
}

impl ScenarioResult {
    pub fn pass(name: impl Into<String>, notes: impl Into<String>, duration_ms: u128) -> Self {
        Self {
            name: name.into(),
            status: Status::Pass,
            notes: notes.into(),
            duration_ms,
        }
    }
    pub fn fail(name: impl Into<String>, notes: impl Into<String>, duration_ms: u128) -> Self {
        Self {
            name: name.into(),
            status: Status::Fail,
            notes: notes.into(),
            duration_ms,
        }
    }
}

pub trait Scenario: Send + Sync {
    fn name(&self) -> &'static str;
    fn run<'a>(
        &'a self,
        ctx: &'a ScenarioCtx,
    ) -> Pin<Box<dyn Future<Output = ScenarioResult> + Send + 'a>>;
}
