//! Transport: RPC over HTTP, pushes over one WebSocket.
//!
//! The session cookie is HttpOnly, so it rides along with `credentials:
//! include` and this code never holds the token. That is deliberate — a token
//! in WASM memory is a token reachable from any script on the page.

use futures::StreamExt;
use gloo_net::http::Request;
use gloo_net::websocket::{futures::WebSocket, Message};
use leptos::prelude::*;
use serde_json::{json, Value};

/// Call a ported Electron channel. The body is the bare argument array,
/// mirroring `ipcRenderer.invoke(channel, ...args)`.
pub async fn rpc(channel: &str, args: Value) -> Result<Value, String> {
    let res = Request::post(&format!("/rpc/{channel}"))
        .credentials(web_sys::RequestCredentials::SameOrigin)
        .json(&args)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;

    if res.status() == 401 {
        return Err("unauthenticated".into());
    }
    let v: Value = res.json().await.map_err(|e| e.to_string())?;
    // The envelope is `{status:"ok",result}` or `{status:"err",error}`; unwrap
    // it here so no caller has to know the shape.
    match v.get("status").and_then(|s| s.as_str()) {
        Some("ok") => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
        _ => Err(v
            .pointer("/error/message")
            .and_then(|m| m.as_str())
            .unwrap_or("request failed")
            .to_string()),
    }
}

/// A web-native endpoint, not part of the Electron contract.
pub async fn get_json(path: &str) -> Result<Value, String> {
    Request::get(path)
        .credentials(web_sys::RequestCredentials::SameOrigin)
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

/// POST JSON to a web-native endpoint and read the JSON back.
pub async fn post_json(path: &str, body: &serde_json::Value) -> Result<Value, String> {
    Request::post(path)
        .credentials(web_sys::RequestCredentials::SameOrigin)
        .json(body)
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?
        .json()
        .await
        .map_err(|e| e.to_string())
}

pub async fn login(user: &str, password: &str) -> Result<(), String> {
    let res = Request::post("/api/login")
        .credentials(web_sys::RequestCredentials::SameOrigin)
        .json(&json!({ "user": user, "password": password }))
        .map_err(|e| e.to_string())?
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if res.ok() {
        Ok(())
    } else {
        Err("invalid credentials".into())
    }
}

/// Open the push socket and feed every event into `on_event`.
///
/// Reconnects with a fixed delay rather than giving up: the common cause is the
/// server restarting under a redeploy, and a client that stays dead until
/// someone reloads is worse than one that waits three seconds.
pub fn connect(on_event: impl Fn(Value) + 'static, on_state: impl Fn(bool) + 'static) {
    let url = {
        let loc = window().location();
        let proto = if loc.protocol().unwrap_or_default() == "https:" { "wss" } else { "ws" };
        format!("{proto}://{}/ws", loc.host().unwrap_or_default())
    };

    leptos::task::spawn_local(async move {
        loop {
            if let Ok(ws) = WebSocket::open(&url) {
                on_state(true);
                let (_, mut read) = ws.split();
                while let Some(Ok(Message::Text(t))) = read.next().await {
                    if let Ok(v) = serde_json::from_str::<Value>(&t) {
                        on_event(v);
                    }
                }
            }
            on_state(false);
            gloo_timers::future::TimeoutFuture::new(3_000).await;
        }
    });
}
