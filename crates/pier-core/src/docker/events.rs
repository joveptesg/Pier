//! Docker events pub/sub bus.
//!
//! One subscription to `docker.events()` per process, fanned out to any number
//! of WebSocket subscribers via a Tokio broadcast channel. Reconnects with a
//! 2-second backoff whenever the Docker stream ends (restart, socket blip).

use std::sync::Arc;
use std::time::Duration;

use bollard::models::{EventMessage, EventMessageTypeEnum};
use bollard::Docker;
use futures_util::StreamExt;
use serde::Serialize;
use tokio::sync::broadcast;

const CHANNEL_CAPACITY: usize = 1024;
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Slim, UI-friendly projection of a `bollard::models::EventMessage`.
///
/// We drop types we don't need (image/volume/network/daemon) at the bus so
/// every subscriber sees only container lifecycle events.
#[derive(Debug, Clone, Serialize)]
pub struct DockerEventMsg {
    pub container_id: String,
    pub container_name: Option<String>,
    pub image: Option<String>,
    /// Docker action string: `start`, `stop`, `die`, `kill`, `oom`,
    /// `health_status`, `restart`, `rename`, `destroy`, `create`, ...
    pub action: String,
    /// `exitCode` attribute — present on `die`, always `Some("0")` for clean exits.
    pub exit_code: Option<String>,
    /// Engine timestamp (seconds since epoch).
    pub time: Option<i64>,
}

impl DockerEventMsg {
    fn from_bollard(ev: EventMessage) -> Option<Self> {
        // Only container events — ignore image/network/volume/etc noise.
        if ev.typ != Some(EventMessageTypeEnum::CONTAINER) {
            return None;
        }
        let action = ev.action?;
        let actor = ev.actor?;
        let attrs = actor.attributes.unwrap_or_default();
        Some(Self {
            container_id: actor.id.unwrap_or_default(),
            container_name: attrs.get("name").cloned(),
            image: attrs.get("image").cloned(),
            action,
            exit_code: attrs.get("exitCode").cloned(),
            time: ev.time,
        })
    }
}

/// Process-wide Docker events fan-out.
///
/// Background task holds the single Docker subscription; subscribers get a
/// `broadcast::Receiver` and never touch Docker directly. Receivers that fall
/// behind by more than [`CHANNEL_CAPACITY`] messages see a `RecvError::Lagged`
/// they must tolerate — the runtime drops the oldest events rather than
/// stalling the producer.
pub struct DockerEventBus {
    tx: broadcast::Sender<DockerEventMsg>,
}

impl DockerEventBus {
    /// Spawn the listener task and return a handle for subscription.
    pub fn spawn(docker: Docker) -> Arc<Self> {
        let (tx, _) = broadcast::channel(CHANNEL_CAPACITY);
        let bus = Arc::new(Self { tx: tx.clone() });
        tokio::spawn(run_listener(docker, tx));
        bus
    }

    /// Subscribe to the event stream. Each caller gets its own lagging buffer.
    pub fn subscribe(&self) -> broadcast::Receiver<DockerEventMsg> {
        self.tx.subscribe()
    }
}

async fn run_listener(docker: Docker, tx: broadcast::Sender<DockerEventMsg>) {
    loop {
        let mut stream = docker.events(None::<bollard::query_parameters::EventsOptions>);
        tracing::debug!("Docker events stream opened");

        while let Some(result) = stream.next().await {
            match result {
                Ok(ev) => {
                    if let Some(msg) = DockerEventMsg::from_bollard(ev) {
                        // `send` only fails when there are no active receivers,
                        // which is fine — we keep listening so newcomers can
                        // subscribe later without losing the fan-out.
                        let _ = tx.send(msg);
                    }
                }
                Err(e) => {
                    tracing::warn!("Docker events stream error: {e}");
                    break;
                }
            }
        }

        // Stream closed (Docker restart, socket error). Back off and retry.
        tokio::time::sleep(RECONNECT_DELAY).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bollard::models::EventActor;
    use std::collections::HashMap;

    fn make_event(action: &str, attrs: &[(&str, &str)]) -> EventMessage {
        let map: HashMap<String, String> = attrs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        EventMessage {
            typ: Some(EventMessageTypeEnum::CONTAINER),
            action: Some(action.to_string()),
            actor: Some(EventActor {
                id: Some("abcd1234".to_string()),
                attributes: Some(map),
            }),
            time: Some(1_700_000_000),
            ..Default::default()
        }
    }

    #[test]
    fn container_die_extracts_exit_code() {
        let ev = make_event(
            "die",
            &[("name", "web"), ("image", "nginx"), ("exitCode", "137")],
        );
        let msg = DockerEventMsg::from_bollard(ev).unwrap();
        assert_eq!(msg.action, "die");
        assert_eq!(msg.container_name.as_deref(), Some("web"));
        assert_eq!(msg.exit_code.as_deref(), Some("137"));
        assert_eq!(msg.image.as_deref(), Some("nginx"));
    }

    #[test]
    fn non_container_events_skipped() {
        let ev = EventMessage {
            typ: Some(EventMessageTypeEnum::IMAGE),
            action: Some("pull".into()),
            actor: Some(EventActor {
                id: Some("img".into()),
                attributes: Some(HashMap::new()),
            }),
            ..Default::default()
        };
        assert!(DockerEventMsg::from_bollard(ev).is_none());
    }

    #[test]
    fn missing_action_or_actor_returns_none() {
        assert!(DockerEventMsg::from_bollard(EventMessage {
            typ: Some(EventMessageTypeEnum::CONTAINER),
            action: None,
            ..Default::default()
        })
        .is_none());

        assert!(DockerEventMsg::from_bollard(EventMessage {
            typ: Some(EventMessageTypeEnum::CONTAINER),
            action: Some("start".into()),
            actor: None,
            ..Default::default()
        })
        .is_none());
    }
}
