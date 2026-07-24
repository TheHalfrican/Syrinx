//! The unified event enum both transports feed into the app's `select!` loop.
//!
//! Variant names and payloads are pinned by RPC-PROTOCOL.md Appendix A: one
//! variant per D-Bus signal (§6) plus `PropertiesChanged` (§5). The zbus impl
//! maps its signal streams onto these; the RPC impl decodes each notification
//! into one. Field names come from the `lib.rs` signal argument names; types
//! from the D-Bus signature (`u`→`u32`, `i`→`i32`, `d`→`f64`, `s`→`String`).

use serde_json::Value;
use std::collections::BTreeMap;

/// A server→client event: a signal or a property change, transport-agnostic.
#[derive(Debug, Clone)]
pub enum EngineEvent {
    GenerationProgress { gen_id: u32, state: String, pct: f64 },
    AudioLevel { gen_id: u32, rms: f64 },
    PlaybackInfo { gen_id: u32, clip_id: String, title: String, duration: f64, bars: String },
    PlaybackProgress { gen_id: u32, pct: f64 },
    LlmResult { req_id: u32, text: String },
    TranscribeProgress { req_id: u32, partial: String },
    TranscribeResult { req_id: u32, text: String },
    ModelProgress { model_id: String, pct: f64, status: String },
    SpeakStarted { gen_id: u32 },
    SpeakEnded { gen_id: u32 },

    /// Mirrors the D-Bus `PropertiesChanged` / the RPC `PropertiesChanged`
    /// notification. In practice only carries `{"ModelLoaded": true}` today.
    /// Keys are PascalCase property names; values are decoded as-is.
    PropertiesChanged { changed: BTreeMap<String, Value> },
}
