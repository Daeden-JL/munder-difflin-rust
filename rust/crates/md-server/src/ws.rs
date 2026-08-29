//! The push plane: one multiplexed socket per client.
//!
//! Electron gave every PTY its own IPC channel (`pty:data:<id>`). Over the network
//! that would mean a socket per terminal, so the streams are multiplexed instead
//! and the concrete id rides in the envelope's `stream` field.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use md_contract::{ClientMessage, ServerEvent};
use md_tenant::TenantId;
use tokio::sync::broadcast;

use crate::auth::Auth;
use crate::state::AppState;

/// Fan-out for server-initiated events, partitioned by tenant.
///
/// Partitioning is the whole design: a single global channel would deliver every
/// tenant's hive traffic to every socket and rely on client-side filtering, which
/// is not a boundary at all.
#[derive(Clone, Default)]
pub struct Hub {
    per_tenant: Arc<RwLock<HashMap<TenantId, broadcast::Sender<ServerEvent>>>>,
}

impl Hub {
    pub fn new() -> Self { Self::default() }

    fn channel(&self, tenant: &TenantId) -> broadcast::Sender<ServerEvent> {
        if let Some(tx) = self.per_tenant.read().unwrap().get(tenant) {
            return tx.clone();
        }
        let mut map = self.per_tenant.write().unwrap();
        map.entry(tenant.clone())
            .or_insert_with(|| broadcast::channel(512).0)
            .clone()
    }

    pub fn subscribe(&self, tenant: &TenantId) -> broadcast::Receiver<ServerEvent> {
        self.channel(tenant).subscribe()
    }

    /// Publish to one tenant. Errors are ignored: no subscribers simply means
    /// nobody has the app open, which is normal, not a failure.
    pub fn publish(&self, tenant: &TenantId, event: ServerEvent) {
        let _ = self.channel(tenant).send(event);
    }
}

pub async fn ws_handler(
    ws: WebSocketUpgrade,
    Auth(session): Auth,
    State(state): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| client_loop(socket, session.tenant, state))
}

async fn client_loop(socket: WebSocket, tenant: TenantId, state: AppState) {
    let (mut sink, mut stream) = socket.split();
    let mut events = state.hub.subscribe(&tenant);

    // Server -> client.
    let send_task = tokio::spawn(async move {
        loop {
            match events.recv().await {
                Ok(ev) => {
                    let Ok(text) = serde_json::to_string(&ev) else { continue };
                    if sink.send(Message::Text(text.into())).await.is_err() { break; }
                }
                // A slow client missed messages. Say so rather than pretending
                // the stream was continuous — the client resyncs by refetching.
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    let warn = serde_json::json!({ "channel": "transport:lagged", "payload": n });
                    if sink.send(Message::Text(warn.to_string().into())).await.is_err() { break; }
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    // Client -> server.
    let tenant_in = tenant.clone();
    let state_in = state.clone();
    let recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = stream.next().await {
            let Message::Text(text) = msg else { continue };
            let Ok(parsed) = serde_json::from_str::<ClientMessage>(&text) else { continue };
            match parsed {
                // Keystrokes take the socket, not the RPC path: a round trip per
                // keypress through HTTP adds latency the terminal cannot hide.
                ClientMessage::PtyInput { id, data } => {
                    if let Err(e) = state_in.pty.write(&id, &tenant_in, &data) {
                        tracing::debug!(%id, error = %e, "pty input rejected");
                    }
                }
                // Subscription is implicit in the tenant partition today; the
                // messages are accepted so the client protocol is stable when
                // per-channel filtering lands.
                ClientMessage::Subscribe { .. } | ClientMessage::Unsubscribe { .. } => {}
                ClientMessage::Ping => {}
            }
        }
    });

    // Either direction ending tears down the other; a half-open socket would
    // otherwise hold a broadcast receiver open and keep lagging the channel.
    tokio::select! {
        _ = send_task => {}
        _ = recv_task => {}
    }
}
