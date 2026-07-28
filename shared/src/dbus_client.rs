//! The Linux transport: a thin wrapper over the existing `#[zbus::proxy]`.
//!
//! Behavior mirrors the app's previous inline D-Bus code — same
//! `Connection::session()` + `EngineProxy::new()` sequence, and the *same nine*
//! signal subscriptions the app consumed before (audio level, generation
//! progress, transcribe progress/result, speak ended, playback info/progress,
//! llm result, model progress) plus `RecordingLevel` — ten — which carries the
//! ⚙ test-mic meter (idle on Linux, where the mic path is `parecord` and the
//! §14 recorder is never started, but the transports stay identical), plus
//! `VcSetupProgress` — eleven — which streams the one-click VC engine install,
//! plus one property stream: `ModelLoadError`,
//! whose RPC counterpart reaches the app as a `PropertiesChanged` notification
//! — without it a failed warmup would be invisible on Linux only. `SpeakStarted`
//! and the `ModelLoaded` property stay unsubscribed: the app consumes neither.
//! A forwarder task pumps each stream into the unified [`EngineEvent`] channel.

use crate::{EngineError, EngineEvent, EngineProxy};
use futures_util::StreamExt;
use tokio::sync::mpsc;

pub struct DbusClient {
    pub(crate) proxy: EngineProxy<'static>,
}

impl DbusClient {
    pub async fn connect() -> Result<(Self, mpsc::UnboundedReceiver<EngineEvent>), EngineError> {
        let conn = zbus::Connection::session().await?;
        let proxy = EngineProxy::new(&conn).await?;

        // Exactly the streams the app subscribed to before, in the same order.
        let mut levels = proxy.receive_audio_level().await?;
        let mut rlevels = proxy.receive_recording_level().await?;
        let mut gprog = proxy.receive_generation_progress().await?;
        let mut tprog = proxy.receive_transcribe_progress().await?;
        let mut tres = proxy.receive_transcribe_result().await?;
        let mut ended = proxy.receive_speak_ended().await?;
        let mut pinfo = proxy.receive_playback_info().await?;
        let mut pprog = proxy.receive_playback_progress().await?;
        let mut llm = proxy.receive_llm_result().await?;
        let mut mprog = proxy.receive_model_progress().await?;
        let mut vcprog = proxy.receive_vc_setup_progress().await?;
        // Property change (not a signal): the D-Bus analog of the RPC
        // `PropertiesChanged {"ModelLoadError": "…"}` notification.
        let mut mlerr = proxy.receive_model_load_error_changed().await;

        let (tx, rx) = mpsc::unbounded_channel::<EngineEvent>();

        // Fan the eleven signal-args streams into the one unified channel. Each
        // `.args()` yields owned fields, mapped 1:1 onto an EngineEvent variant.
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    Some(sig) = levels.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::AudioLevel { gen_id: a.gen_id, rms: a.rms });
                        }
                    }
                    Some(sig) = rlevels.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::RecordingLevel { rec_id: a.rec_id, rms: a.rms });
                        }
                    }
                    Some(sig) = gprog.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::GenerationProgress { gen_id: a.gen_id, state: a.state, pct: a.pct });
                        }
                    }
                    Some(sig) = tprog.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::TranscribeProgress { req_id: a.req_id, partial: a.partial });
                        }
                    }
                    Some(sig) = tres.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::TranscribeResult { req_id: a.req_id, text: a.text, error: a.error });
                        }
                    }
                    Some(sig) = ended.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::SpeakEnded { gen_id: a.gen_id });
                        }
                    }
                    Some(sig) = pinfo.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::PlaybackInfo {
                                gen_id: a.gen_id, clip_id: a.clip_id, title: a.title,
                                duration: a.duration, bars: a.bars,
                            });
                        }
                    }
                    Some(sig) = pprog.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::PlaybackProgress { gen_id: a.gen_id, pct: a.pct });
                        }
                    }
                    Some(sig) = llm.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::LlmResult { req_id: a.req_id, text: a.text, error: a.error });
                        }
                    }
                    Some(sig) = mprog.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::ModelProgress { model_id: a.model_id, pct: a.pct, status: a.status });
                        }
                    }
                    Some(sig) = vcprog.next() => {
                        if let Ok(a) = sig.args() {
                            let _ = tx.send(EngineEvent::VcSetupProgress {
                                setup_id: a.setup_id, stage: a.stage,
                                status: a.status, detail: a.detail,
                            });
                        }
                    }
                    Some(change) = mlerr.next() => {
                        // "" is the healthy boot value (and the stream's first
                        // item) — only a real failure becomes an event.
                        if let Ok(msg) = change.get().await {
                            if !msg.is_empty() {
                                let _ = tx.send(EngineEvent::PropertiesChanged {
                                    changed: [("ModelLoadError".to_string(), serde_json::Value::String(msg))]
                                        .into_iter()
                                        .collect(),
                                });
                            }
                        }
                    }
                    else => break,
                }
            }
        });

        Ok((Self { proxy }, rx))
    }
}
