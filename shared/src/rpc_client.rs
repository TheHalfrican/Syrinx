//! JSON-RPC 2.0 over a localhost WebSocket — the Windows/macOS transport.
//!
//! Implements RPC-PROTOCOL.md: discovery-file lookup (§2.2), the first-message
//! `Authenticate` handshake (§2.3), positional-param requests with an id→oneshot
//! correlator (no default timeout — §7.4), and a notification pump that decodes
//! each id-less frame into an [`EngineEvent`] (§6, §10). Errors preserve
//! `error.message` verbatim so `-32000` handler text matches the D-Bus path
//! (§7.2). Initial-connect retry lives in the app (the splash loop); this
//! module makes a single connect attempt.

use crate::{EngineError, EngineEvent};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

/// The protocol version this client speaks (RPC-PROTOCOL.md §9).
const PROTOCOL_VERSION: u32 = 1;

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type Pending = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, EngineError>>>>>;

/// The discovery file the engine writes once it is bound and listening.
#[derive(Deserialize)]
struct Discovery {
    protocol: u32,
    port: u16,
    token: String,
}

/// A connected, authenticated JSON-RPC client.
pub struct RpcClient {
    write: tokio::sync::Mutex<futures_util::stream::SplitSink<Ws, Message>>,
    pending: Pending,
    next_id: AtomicU64,
}

impl RpcClient {
    /// Read the discovery file, connect, authenticate, and start the read pump.
    /// One attempt only — the caller retries while the engine comes up.
    pub async fn connect() -> Result<(Self, mpsc::UnboundedReceiver<EngineEvent>), EngineError> {
        let disc = read_discovery()?;
        if disc.protocol != PROTOCOL_VERSION {
            return Err(EngineError::Transport(format!(
                "engine speaks protocol v{}; this app needs v{PROTOCOL_VERSION} — update required",
                disc.protocol
            )));
        }
        // Rebuild the URL from the port rather than trusting the file's `url`
        // (spec §2.1: clients MAY, and loopback IPv4 is mandatory).
        let url = format!("ws://127.0.0.1:{}", disc.port);
        let (stream, _) = connect_async(&url)
            .await
            .map_err(|e| EngineError::Transport(format!("connect {url}: {e}")))?;
        let (mut write, mut read) = stream.split();

        let (tx, rx) = mpsc::unbounded_channel::<EngineEvent>();

        // First frame MUST be Authenticate, id 0 (spec §2.3).
        let auth = json!({"jsonrpc": "2.0", "method": "Authenticate", "params": [disc.token], "id": 0});
        write
            .send(Message::Text(auth.to_string()))
            .await
            .map_err(|e| EngineError::Transport(format!("send auth: {e}")))?;
        // Await the id:0 response, forwarding any stray notification meanwhile.
        loop {
            match read.next().await {
                Some(Ok(Message::Text(txt))) => {
                    let v: Value = serde_json::from_str(&txt)
                        .map_err(|e| EngineError::Transport(format!("auth reply parse: {e}")))?;
                    if v.get("id") == Some(&json!(0)) {
                        if v.get("result") == Some(&json!(true)) {
                            break;
                        } else if let Some(err) = v.get("error") {
                            return Err(rpc_error(err));
                        }
                        return Err(EngineError::Transport("malformed Authenticate reply".into()));
                    } else if let Some(ev) = notification_to_event(&v) {
                        let _ = tx.send(ev);
                    }
                }
                Some(Ok(Message::Close(_))) | None => {
                    return Err(EngineError::Transport("socket closed during Authenticate".into()));
                }
                Some(Ok(_)) => continue, // ping/pong/binary — ignore
                Some(Err(e)) => return Err(EngineError::Transport(format!("auth read: {e}"))),
            }
        }

        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        tokio::spawn(read_pump(read, pending.clone(), tx));

        let client = RpcClient {
            write: tokio::sync::Mutex::new(write),
            pending,
            // ids start at 1 — 0 is reserved for the Authenticate handshake.
            next_id: AtomicU64::new(1),
        };
        Ok((client, rx))
    }

    /// Issue a request and await its response. No timeout (spec §7.4): async
    /// work returns an id immediately and lands as a notification, and the one
    /// blocking method (`Transcribe`) may legitimately take a very long time.
    pub(crate) async fn call<T: serde::de::DeserializeOwned>(
        &self,
        method: &str,
        params: Vec<Value>,
    ) -> Result<T, EngineError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);

        let frame = json!({"jsonrpc": "2.0", "method": method, "params": params, "id": id});
        if let Err(e) = self.write.lock().await.send(Message::Text(frame.to_string())).await {
            self.pending.lock().unwrap().remove(&id);
            return Err(EngineError::Transport(format!("send {method}: {e}")));
        }

        match rx.await {
            Ok(Ok(v)) => serde_json::from_value(v)
                .map_err(|e| EngineError::Transport(format!("decode {method} result: {e}"))),
            Ok(Err(e)) => Err(e),
            // sender dropped: the read pump exited (socket closed).
            Err(_) => Err(EngineError::Transport(format!("{method}: connection closed"))),
        }
    }
}

/// Read frames forever, routing responses to their waiters and notifications to
/// the event channel. On socket close, fail every in-flight request.
async fn read_pump(
    mut read: futures_util::stream::SplitStream<Ws>,
    pending: Pending,
    tx: mpsc::UnboundedSender<EngineEvent>,
) {
    while let Some(msg) = read.next().await {
        match msg {
            Ok(Message::Text(txt)) => {
                let Ok(v) = serde_json::from_str::<Value>(&txt) else { continue };
                let is_response =
                    v.get("id").is_some() && (v.get("result").is_some() || v.get("error").is_some());
                if is_response {
                    if let Some(id) = v.get("id").and_then(Value::as_u64) {
                        if let Some(waiter) = pending.lock().unwrap().remove(&id) {
                            let res = match v.get("error") {
                                Some(err) => Err(rpc_error(err)),
                                None => Ok(v.get("result").cloned().unwrap_or(Value::Null)),
                            };
                            let _ = waiter.send(res);
                        }
                    }
                } else if let Some(ev) = notification_to_event(&v) {
                    let _ = tx.send(ev);
                }
            }
            Ok(Message::Close(_)) | Err(_) => break,
            _ => {} // ping/pong/binary
        }
    }
    // Drain waiters so pending `call`s wake with an error instead of hanging.
    for (_, waiter) in pending.lock().unwrap().drain() {
        let _ = waiter.send(Err(EngineError::Transport("connection closed".into())));
    }
}

/// Turn a JSON-RPC error object into an [`EngineError`]. `-32000` is an engine
/// handler failure whose `message` must survive verbatim (§7.2); every other
/// code is a transport/protocol error.
fn rpc_error(err: &Value) -> EngineError {
    let code = err.get("code").and_then(Value::as_i64).unwrap_or(0);
    let msg = err.get("message").and_then(Value::as_str).unwrap_or("").to_string();
    if code == -32000 {
        EngineError::Engine(msg)
    } else {
        EngineError::Transport(msg)
    }
}

/// Decode a server notification frame into an [`EngineEvent`] (spec §6, §10).
/// Signals carry a positional `params` array; `PropertiesChanged` carries a
/// by-name object (§5). Returns `None` for responses or unknown methods.
pub(crate) fn notification_to_event(v: &Value) -> Option<EngineEvent> {
    if v.get("id").is_some() {
        return None; // a response, not a notification
    }
    let method = v.get("method")?.as_str()?;
    let params = v.get("params");
    let arr = || params.and_then(Value::as_array);
    let u32_at = |a: &[Value], i: usize| a.get(i).and_then(Value::as_u64).map(|n| n as u32);
    let f64_at = |a: &[Value], i: usize| a.get(i).and_then(Value::as_f64);
    let str_at = |a: &[Value], i: usize| a.get(i).and_then(Value::as_str).map(str::to_string);

    match method {
        "GenerationProgress" => {
            let a = arr()?;
            Some(EngineEvent::GenerationProgress {
                gen_id: u32_at(a, 0)?,
                state: str_at(a, 1)?,
                pct: f64_at(a, 2)?,
            })
        }
        "AudioLevel" => {
            let a = arr()?;
            Some(EngineEvent::AudioLevel { gen_id: u32_at(a, 0)?, rms: f64_at(a, 1)? })
        }
        "PlaybackInfo" => {
            let a = arr()?;
            Some(EngineEvent::PlaybackInfo {
                gen_id: u32_at(a, 0)?,
                clip_id: str_at(a, 1)?,
                title: str_at(a, 2)?,
                duration: f64_at(a, 3)?,
                bars: str_at(a, 4)?,
            })
        }
        "PlaybackProgress" => {
            let a = arr()?;
            Some(EngineEvent::PlaybackProgress { gen_id: u32_at(a, 0)?, pct: f64_at(a, 1)? })
        }
        "LlmResult" => {
            let a = arr()?;
            Some(EngineEvent::LlmResult { req_id: u32_at(a, 0)?, text: str_at(a, 1)? })
        }
        "TranscribeProgress" => {
            let a = arr()?;
            Some(EngineEvent::TranscribeProgress { req_id: u32_at(a, 0)?, partial: str_at(a, 1)? })
        }
        "TranscribeResult" => {
            let a = arr()?;
            Some(EngineEvent::TranscribeResult { req_id: u32_at(a, 0)?, text: str_at(a, 1)? })
        }
        "ModelProgress" => {
            let a = arr()?;
            Some(EngineEvent::ModelProgress {
                model_id: str_at(a, 0)?,
                pct: f64_at(a, 1)?,
                status: str_at(a, 2)?,
            })
        }
        "SpeakStarted" => Some(EngineEvent::SpeakStarted { gen_id: u32_at(arr()?, 0)? }),
        "SpeakEnded" => Some(EngineEvent::SpeakEnded { gen_id: u32_at(arr()?, 0)? }),
        "PropertiesChanged" => {
            let obj = params?.as_object()?;
            let changed = obj.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            Some(EngineEvent::PropertiesChanged { changed })
        }
        _ => None,
    }
}

/// Resolve the discovery file path: the `SYRINX_RPC_ENDPOINT` override wins,
/// else the pinned per-OS data/runtime dir + `rpc.json` (spec §2.2).
fn discovery_path() -> Result<PathBuf, EngineError> {
    if let Ok(p) = std::env::var("SYRINX_RPC_ENDPOINT") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    default_discovery_dir()
        .map(|d| d.join("rpc.json"))
        .ok_or_else(|| EngineError::Transport("cannot resolve the discovery directory".into()))
}

/// The per-OS discovery directory (spec §2.2), matching platformdirs 4.x.
fn default_discovery_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        // %LOCALAPPDATA%\syrinx\syrinx
        dirs::data_local_dir().map(|d| d.join("syrinx").join("syrinx"))
    }
    #[cfg(target_os = "macos")]
    {
        // ~/Library/Application Support/syrinx  (author ignored on mac)
        dirs::data_dir().map(|d| d.join("syrinx"))
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        // $XDG_RUNTIME_DIR/syrinx, falling back to ~/.local/share/syrinx.
        dirs::runtime_dir()
            .or_else(dirs::data_dir)
            .map(|d| d.join("syrinx"))
    }
}

fn read_discovery() -> Result<Discovery, EngineError> {
    let path = discovery_path()?;
    let raw = std::fs::read_to_string(&path).map_err(|e| {
        EngineError::Transport(format!("discovery file {}: {e}", path.display()))
    })?;
    serde_json::from_str(&raw)
        .map_err(|e| EngineError::Transport(format!("discovery file {}: {e}", path.display())))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_a_positional_signal() {
        let v = json!({"jsonrpc": "2.0", "method": "GenerationProgress", "params": [42, "synthesizing", 0.5]});
        match notification_to_event(&v) {
            Some(EngineEvent::GenerationProgress { gen_id, state, pct }) => {
                assert_eq!(gen_id, 42);
                assert_eq!(state, "synthesizing");
                assert_eq!(pct, 0.5);
            }
            other => panic!("wrong decode: {other:?}"),
        }
    }

    #[test]
    fn decodes_properties_changed_by_name() {
        let v = json!({"jsonrpc": "2.0", "method": "PropertiesChanged", "params": {"ModelLoaded": true}});
        match notification_to_event(&v) {
            Some(EngineEvent::PropertiesChanged { changed }) => {
                assert_eq!(changed.get("ModelLoaded"), Some(&json!(true)));
            }
            other => panic!("wrong decode: {other:?}"),
        }
    }

    #[test]
    fn ignores_responses_and_unknown_methods() {
        // a response frame carries an id — never an event
        assert!(notification_to_event(&json!({"jsonrpc": "2.0", "result": 3, "id": 7})).is_none());
        // unknown notification method
        assert!(notification_to_event(&json!({"jsonrpc": "2.0", "method": "Nope", "params": []})).is_none());
    }

    #[test]
    fn engine_error_preserves_message_verbatim() {
        let err = json!({"code": -32000, "message": "UNIQUE constraint failed: profiles.name"});
        let e = rpc_error(&err);
        assert!(matches!(e, EngineError::Engine(_)));
        assert!(e.to_string().contains("UNIQUE constraint failed: profiles.name"));
    }
}
