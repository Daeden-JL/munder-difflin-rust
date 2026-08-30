//! The wire contract shared by the server and the WASM client.
//!
//! Generated from `contract/manifest.json`, which is itself extracted from the
//! Electron bridge. While the port is in flight the TypeScript stays
//! authoritative; after cutover this crate becomes the source of truth.

mod channels {
    include!(concat!(env!("OUT_DIR"), "/channels.rs"));
}
pub use channels::{Push, Rpc};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A call against one RPC channel. Arguments are positional because the bridge
/// they came from is positional (`invoke('pty:write', id, data)`); naming them
/// would mean inventing names for 158 signatures and keeping the invention in
/// sync on both sides.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcRequest {
    pub channel: String,
    #[serde(default)]
    pub args: Vec<Value>,
}

/// Mirrors how the bridge already reports failure. Much of the existing surface
/// returns `{ ok: false, error }` in-band rather than rejecting, so the transport
/// keeps a separate envelope-level error for transport/authorization failures and
/// leaves in-band shapes untouched in `result`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum RpcResponse {
    Ok { result: Value },
    Err { error: RpcError },
}

impl RpcResponse {
    pub fn ok(result: impl Serialize) -> Self {
        RpcResponse::Ok { result: serde_json::to_value(result).unwrap_or(Value::Null) }
    }
    pub fn err(code: ErrorCode, message: impl Into<String>) -> Self {
        RpcResponse::Err { error: RpcError { code, message: message.into() } }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    /// No such channel. During the port this also means "not ported yet".
    UnknownChannel,
    /// Channel exists but this build has no handler for it. Distinct from
    /// `UnknownChannel` so parity tooling can tell "missing" from "misspelled".
    NotImplemented,
    /// The channel exists in the Electron contract but has no server-side
    /// meaning here, and never will: it is either the browser's job (clipboard,
    /// opening a link) or an Electron-only capability (the app's own window, the
    /// desktop auto-updater). Distinct from `NotImplemented` so "still to do" and
    /// "never" do not sit in the same bucket — otherwise port progress can never
    /// reach 100%% and nobody can tell why.
    NotApplicable,
    Unauthenticated,
    /// Authenticated, but not for this tenant's resource.
    Forbidden,
    BadArguments,
    Internal,
}

/// Server-to-client push. `stream` carries the id for per-instance channels
/// (`pty:data:{id}`), replacing Electron's one-channel-per-PTY scheme with a
/// single multiplexed socket.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerEvent {
    pub channel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<String>,
    pub payload: Value,
}

impl ServerEvent {
    pub fn new(channel: Push, payload: impl Serialize) -> Self {
        Self {
            channel: channel.as_str().to_string(),
            stream: None,
            payload: serde_json::to_value(payload).unwrap_or(Value::Null),
        }
    }
    /// For dynamic channels the stored name keeps its `{id}` hole; the concrete
    /// id travels in `stream` so the client can route without string parsing.
    pub fn stream(channel: Push, id: impl Into<String>, payload: impl Serialize) -> Self {
        Self {
            channel: channel.as_str().to_string(),
            stream: Some(id.into()),
            payload: serde_json::to_value(payload).unwrap_or(Value::Null),
        }
    }
}

/// Client-to-server socket traffic. PTY input is deliberately here rather than on
/// the RPC path: keystrokes are hot and ordered, and a round trip per keypress
/// through HTTP would add latency the terminal cannot hide.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ClientMessage {
    Subscribe { channel: String, #[serde(default)] stream: Option<String> },
    Unsubscribe { channel: String, #[serde(default)] stream: Option<String> },
    PtyInput { id: String, data: String },
    Ping,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The generated tables must agree with the manifest they came from. If the
    /// extractor changes shape, this fails rather than silently shrinking the API.
    #[test]
    fn generated_channels_match_manifest_counts() {
        assert_eq!(Rpc::ALL.len(), 161, "expected 158 rpc + 3 rpc-sync channels");
        assert_eq!(Push::ALL.len(), 28, "expected 28 push channels");
    }

    #[test]
    fn every_channel_round_trips_through_parse() {
        for &c in Rpc::ALL {
            assert_eq!(Rpc::parse(c.as_str()), Some(c), "rpc {:?}", c);
        }
        for &c in Push::ALL {
            assert_eq!(Push::parse(c.as_str()), Some(c), "push {:?}", c);
        }
    }

    /// The three per-PTY streams are the only dynamic ones; anything else gaining
    /// a `{hole}` is a design change that should be noticed here first.
    #[test]
    fn only_pty_streams_are_dynamic() {
        let dynamic: Vec<_> = Push::ALL.iter().filter(|c| c.is_dynamic())
            .map(|c| c.as_str()).collect();
        assert_eq!(dynamic, vec!["pty:data:{id}", "pty:exit:{id}", "pty:relaunch:{id}"]);
    }
}
