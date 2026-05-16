//! Ad-hoc task runner.
//!
//! Pier's answer to Semaphore UI's "saved task" + "ad-hoc command" workflow:
//! the operator picks a server and either runs a saved [`TaskTemplate`] or
//! a one-off command. The agent runs the command, core polls the agent for
//! status + output, and the UI shows a live-ish log (1s refresh).
//!
//! Wire protocol: HTTP polling against `/api/v1/agent/shell/**`. No
//! WebSocket — keeps the dependency surface small and the protocol
//! easy to debug with curl.

pub mod executor;
pub mod models;
pub mod recover;
