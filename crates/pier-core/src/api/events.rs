//! WebSocket endpoint that fans out Docker container events to the UI.
//!
//! A single process-wide [`DockerEventBus`](crate::docker::events::DockerEventBus)
//! supplies the stream; each WebSocket subscription gets its own lagging
//! buffer and is closed on client disconnect.

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{Query, State, WebSocketUpgrade};
use axum::response::IntoResponse;
use serde::Deserialize;
use tokio::sync::broadcast::error::RecvError;

use crate::docker::events::DockerEventMsg;
use crate::state::SharedState;

#[derive(Deserialize)]
pub struct EventsParams {
    /// Filter to a single container by id prefix or full name.
    pub container: Option<String>,
}

/// GET /api/v1/events/ws
pub async fn events_ws(
    State(state): State<SharedState>,
    Query(params): Query<EventsParams>,
    ws: WebSocketUpgrade,
) -> impl IntoResponse {
    let rx = state.event_bus.subscribe();
    ws.on_upgrade(move |socket| handle(socket, rx, params.container))
}

async fn handle(
    mut socket: WebSocket,
    mut rx: tokio::sync::broadcast::Receiver<DockerEventMsg>,
    filter: Option<String>,
) {
    let mut ping = tokio::time::interval(Duration::from_secs(30));
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Ok(event) => {
                        if !matches_filter(&event, filter.as_deref()) {
                            continue;
                        }
                        let payload = match serde_json::to_string(&event) {
                            Ok(s) => s,
                            Err(e) => {
                                tracing::warn!("events ws: serialize failed: {e}");
                                continue;
                            }
                        };
                        if socket.send(Message::Text(payload.into())).await.is_err() {
                            return; // client disconnected
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        // Subscriber fell behind; emit a marker frame so the UI
                        // can trigger a full refresh if it cares about gaps.
                        let marker = serde_json::json!({
                            "lagged": n,
                        });
                        if socket.send(Message::Text(marker.to_string().into())).await.is_err() {
                            return;
                        }
                    }
                    Err(RecvError::Closed) => return,
                }
            }
            _ = ping.tick() => {
                if socket.send(Message::Ping(vec![].into())).await.is_err() {
                    return;
                }
            }
            frame = socket.recv() => {
                match frame {
                    Some(Ok(Message::Close(_))) | None => return,
                    Some(Err(_)) => return,
                    _ => continue, // ignore pings/pongs/text from client
                }
            }
        }
    }
}

fn matches_filter(event: &DockerEventMsg, filter: Option<&str>) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    if event.container_id.starts_with(filter) {
        return true;
    }
    if let Some(name) = &event.container_name {
        if name == filter {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(id: &str, name: &str) -> DockerEventMsg {
        DockerEventMsg {
            container_id: id.to_string(),
            container_name: Some(name.to_string()),
            image: None,
            action: "start".to_string(),
            exit_code: None,
            time: None,
        }
    }

    #[test]
    fn no_filter_matches_everything() {
        assert!(matches_filter(&sample("abc123", "web"), None));
    }

    #[test]
    fn id_prefix_matches() {
        assert!(matches_filter(&sample("abc123def", "web"), Some("abc123")));
        assert!(!matches_filter(&sample("abc123def", "web"), Some("xyz")));
    }

    #[test]
    fn exact_name_matches() {
        assert!(matches_filter(&sample("abc", "pier-api"), Some("pier-api")));
        assert!(!matches_filter(
            &sample("abc", "pier-api"),
            Some("pier-worker")
        ));
    }
}
