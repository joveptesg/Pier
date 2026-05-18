//! Scenario registry.
//!
//! P1 ships protocol smoke scenarios that exercise the registry HTTP surface
//! without shelling out to npm/yarn/pnpm/bun. The client matrix lives in P2.

use crate::scenario::Scenario;

mod ping;
mod whoami;

pub fn all() -> Vec<Box<dyn Scenario>> {
    vec![
        Box::new(ping::Ping),
        Box::new(whoami::WhoamiAuth),
        Box::new(whoami::WhoamiNoAuth),
    ]
}
