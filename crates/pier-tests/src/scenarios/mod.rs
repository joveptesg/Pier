//! Scenario registry.
//!
//! Order matters: publish_* must run before any packument/dist-tag/tarball
//! scenario, and `unpublish_single` runs last so the side effect doesn't
//! cascade.

use crate::scenario::Scenario;

mod mutations;
mod packument;
mod ping;
pub mod publish;
mod tarball;
mod whoami;

pub fn all() -> Vec<Box<dyn Scenario>> {
    vec![
        // Protocol smoke
        Box::new(ping::Ping),
        Box::new(whoami::WhoamiAuth),
        Box::new(whoami::WhoamiNoAuth),
        // Seed packages (everything below reads them)
        Box::new(publish::PublishFlat),
        Box::new(publish::PublishScoped),
        // Packument shape
        Box::new(packument::Abbreviated),
        Box::new(packument::DistTagsKebab),
        Box::new(packument::TimeCreatedModified),
        Box::new(packument::NoIsProxyLeak),
        Box::new(packument::EtagPackument),
        // Streaming
        Box::new(tarball::TarballStreaming),
        // Mutations (must end with unpublish — drops the fixture)
        Box::new(mutations::DistTagAdd),
        Box::new(mutations::DistTagRm),
        Box::new(mutations::Deprecate),
        Box::new(mutations::UnpublishSingle),
    ]
}
