# Syrinx — RPC Protocol (JSON-RPC 2.0 over localhost WebSocket)

> **This document is the contract for the Windows/macOS engine transport.**
> Linux keeps talking **D-Bus** (`sh.syrinx.Engine1`) exactly as before — this
> spec does not change the Linux build. It exists so that two engineers can
> implement, independently, a **Python WebSocket server** (a second thin wrapper
> over the same transport-agnostic engine core) and a **Rust WebSocket client**
> (the `EngineClient` RPC impl) that behave byte-for-byte like the D-Bus pair.
>
> Design brief: [`MULTIPLATPLAN.md`](../MULTIPLATPLAN.md) §1.1 "Transport".
> The authoritative surface being mirrored is the zbus proxy in
> [`shared/src/lib.rs`](../shared/src/lib.rs) and the dbus_next service in
> [`engine/syrinx_engine/service.py`](../engine/syrinx_engine/service.py).

The two transports are held identical by a **contract test suite** that runs the
same method/signal exercises over both wrappers in CI (MULTIPLATPLAN §1.1
"Drift protection"). Anything ambiguous below is a bug in this document — fix the
document, not one implementation.

---

## 0. Surface at a glance

| | Count | Source of truth |
|---|---|---|
| Methods | **65** | 65 `@method()` in `service.py`; 65 `fn` (of 77) in `lib.rs` |
| Read-only properties | **2** | `ModelLoaded`, `Backend` → become `GetModelLoaded` / `GetBackend` + `PropertiesChanged` |
| Signals | **10** | 10 `@signal()` → become server→client notifications |
| Transport-only RPC methods | **4** | `Authenticate`, `GetModelLoaded`, `GetBackend`, `GetProtocolVersion` (no D-Bus analog; properties/handshake are native there) |

`lib.rs` and `service.py` are **in sync** — every method name, arity, and
signature matches (verified for this spec; see §11). If a future change makes
them diverge, `service.py` is authoritative (it is the engine) and the
divergence must be called out here rather than silently resolved.

---

## 1. Transport

- **Server:** a WebSocket server bound to **`127.0.0.1`** (loopback only — never
  `0.0.0.0`), on an **ephemeral OS-assigned port** (`bind(port=0)`, read back the
  actual port). IPv4 loopback; a client MUST connect to `127.0.0.1`, not a
  hostname.
- **Framing:** JSON-RPC 2.0, one JSON value per **WebSocket text frame** (UTF-8).
  No batching. No binary frames. Each request, response, and notification is a
  single complete frame; implementations MUST NOT split a JSON value across
  frames or pack two into one.
- **Direction:** the client sends **requests**; the server sends **responses**
  (to requests) and **notifications** (signals + `PropertiesChanged`, id-less).
- **Connections:** exactly **one** client connection is the normal case (the app,
  or in phase 3 the dictate binary). The spec nonetheless defines multi-client
  behavior:
  - **Requests** are accepted from **any** authenticated connection.
  - **Notifications** (all signals + `PropertiesChanged`) are **broadcast to
    every** authenticated connection.
  - JSON-RPC `id` values live in a **per-connection** namespace (the server
    tracks each socket's in-flight ids independently). Engine-global counters
    (`gen_id`, `req_id`, `model_id`) are shared across connections, so a second
    client will observe notifications for work it did not start; clients filter
    notifications by the ids they hold (the app already does this — see the
    `pending_vc` / `pending_tr` guards in `app/src/main.rs`).
- **Keepalive:** the server SHOULD honor WebSocket ping/pong; clients MAY send
  pings. There is no application-level heartbeat.

---

## 2. Discovery & authentication

### 2.1 Discovery file

On startup — **after** the WebSocket server is bound and listening, **before**
warmup — the engine writes a single JSON discovery file and keeps it until clean
shutdown.

**Contents** (all fields required):

```json
{
  "protocol": 1,
  "port": 53421,
  "token": "b9f1c0e2a7d34f5c8e1b6a09d2f47e3c5a8b1d0e6f92c4a7b3d8e0f1c2a4b6d8",
  "pid": 48213,
  "url": "ws://127.0.0.1:53421"
}
```

- `protocol` — integer protocol version, currently **1** (see §9). Mirrors
  `GetProtocolVersion`. A client whose supported version ≠ this MUST refuse to
  connect and surface an upgrade-needed message.
- `port` — the actual bound ephemeral port.
- `token` — a fresh **64-hex-character** (32-byte) secret, generated per engine
  start via a CSPRNG (`secrets.token_hex(32)` in Python). Never reused across
  runs.
- `pid` — the engine process id (lets a supervisor/client detect a stale file).
- `url` — convenience; `ws://127.0.0.1:<port>`. Clients MAY ignore it and
  rebuild from `port`.

**Write semantics:** write to a temp file in the same directory, then atomically
rename over the target. On POSIX, create with mode **`0600`**. The engine
**removes** the file on clean shutdown (normal exit, SIGTERM/SIGINT handler, or
`atexit`). A crash may leave a stale file; a client that fails to connect (or
connects but `Authenticate` is rejected) MUST treat the file as stale and, in the
supervised (Win/mac) model, wait for the app-spawned engine to rewrite it.

### 2.2 Discovery file path (pinned)

The directory is resolved with **`platformdirs`** (Python) and the **`dirs`**
crate (Rust). These MUST resolve to the same path per OS. Filename is
**`rpc.json`**. Values below are from platformdirs **4.x** for
`appname="syrinx"`, `appauthor="syrinx"`:

| OS | Directory | Full path (example) | platformdirs call | Rust `dirs` derivation |
|---|---|---|---|---|
| **Windows** | `%LOCALAPPDATA%\syrinx\syrinx` | `C:\Users\<user>\AppData\Local\syrinx\syrinx\rpc.json` | `user_data_dir("syrinx","syrinx")` | `dirs::data_local_dir()` + `syrinx\syrinx` |
| **macOS** | `~/Library/Application Support/syrinx` | `~/Library/Application Support/syrinx/rpc.json` | `user_data_dir("syrinx","syrinx")` (author ignored on mac) | `dirs::data_dir()` + `syrinx` |
| **Linux** (dev/test only) | `$XDG_RUNTIME_DIR/syrinx` | `/run/user/<uid>/syrinx/rpc.json` | `user_runtime_dir("syrinx","syrinx")` | `dirs::runtime_dir()` + `syrinx` |

Notes:
- **Windows/macOS** use the **data dir** (stable, per-user, ACL-protected).
- **Linux** uses **`XDG_RUNTIME_DIR`** (tmpfs, mode 0700, cleared on logout —
  the correct home for a session secret). On Linux the RPC transport is used
  **only** by the contract tests and dev tooling; production Linux uses D-Bus.
  If `XDG_RUNTIME_DIR` is unset, fall back to `user_data_dir` (`~/.local/share/syrinx`).
- Override for tests/CI: honor an explicit **`SYRINX_RPC_ENDPOINT`** env var
  (absolute path to the discovery file) on both server and client; it wins over
  the per-OS default. (This mirrors the existing `SYRINX_DATA_DIR` override
  convention.)

### 2.3 Authentication (first-message `Authenticate`)

Auth is a **first JSON-RPC message**, not an HTTP header.

*Rationale:* keeps the entire protocol inside one JSON-RPC framing — the Rust
client (`tokio-tungstenite`) and the contract tests never have to hand-craft or
inspect the HTTP upgrade request; the server needs no header plumbing; and the
mechanism is trivially exercisable by the drift tests. Loopback-binding plus the
per-session token is the actual access control; the handshake is about proving
possession of the token, which a normal request frame does cleanly.

**Sequence:**

1. Client reads `rpc.json`, gets `port` + `token`, opens `ws://127.0.0.1:<port>`.
2. Client's **first frame** MUST be:
   ```json
   {"jsonrpc":"2.0","method":"Authenticate","params":["<token>"],"id":0}
   ```
3. Server compares the token in **constant time** to the session token.
   - **Success** → `{"jsonrpc":"2.0","result":true,"id":0}`. The connection is
     now authenticated; the client may issue any method and will receive all
     broadcast notifications.
   - **Failure** (wrong token) → `{"jsonrpc":"2.0","error":{"code":-32001,"message":"invalid token"},"id":0}`,
     then the server closes the socket with WebSocket close code **1008**
     (policy violation).
4. **Any** method other than `Authenticate` sent before a successful
   `Authenticate` → `{"jsonrpc":"2.0","error":{"code":-32002,"message":"not authenticated"},...}`,
   then close **1008**. (Requests carry the caller's `id`; if unparseable, `id`
   is `null`.)
5. A second `Authenticate` on an already-authenticated connection is a no-op
   success (`result:true`).

There is no explicit subscribe step: authentication implicitly subscribes the
connection to every notification.

---

## 3. Type mapping (D-Bus signature → JSON)

| D-Bus | Meaning | JSON type | Notes |
|---|---|---|---|
| `s` | string | string | UTF-8 |
| `u` | uint32 | integer | `gen_id`, `req_id`, counts — always ≥ 0 |
| `i` | int32 | integer | signed (e.g. crop rects, semitones) |
| `d` | double | number | may be non-integer JSON number |
| `b` | boolean | boolean | |
| `a(ss)` | array of (string,string) | array of `[string,string]` | e.g. `ListVoices` |
| *(none)* | void reply | `null` | method still returns a response `{"result":null}` |

Params are passed as a **positional JSON array**, in the same order as the D-Bus
method arguments (and the `lib.rs` `fn` argument order — they match). Names in
the tables below are for readability; only position is significant.

Many methods take or return **`s` that carries JSON** (e.g. `spec_json`,
`ListProfiles`, `chain_json`). These stay **strings** on the wire — the engine
parses/serializes them internally exactly as on D-Bus. Do **not** "helpfully"
unwrap them into JSON objects; that would break the contract-test byte-equality
and the engine's `json.loads`.

---

## 4. Method table

Grouped and ordered as in `lib.rs`. RPC method name = the exact D-Bus PascalCase
name. `→ null` means a void reply (`{"result":null}`).

### 4.1 Synthesis, transcription, voices

| Method | Params `[name: type, …]` | Result | Semantics |
|---|---|---|---|
| `Speak` | `[text: string, voice_id: string]` | integer (gen_id) | Synthesize `text` in `voice_id`; returns a generation id. Progress/playback via signals. |
| `Transcribe` | `[audio_path: string]` | string | **Blocking** — recognized text returned in the response. May take a long time (see §7.2). |
| `ListVoices` | `[]` | array of `[id, display_name]` | Available voices (built-in + cloned). |
| `CloneVoice` | `[name: string, sample_path: string, ref_text: string]` | string (profile_id) | Clone a voice from a sample + its transcript (`ref_text` required by Qwen). |

### 4.2 Voice profiles

| Method | Params | Result | Semantics |
|---|---|---|---|
| `CreateProfile` | `[spec_json: string]` | string (profile_id) | Create a profile from a JSON spec. |
| `ListProfiles` | `[]` | string (JSON array) | Profile summaries. |
| `GetProfile` | `[profile_id: string]` | string (JSON, `""` if not found) | Full profile. |
| `UpdateProfile` | `[profile_id: string, patch_json: string]` | → null | Apply a JSON patch of editable fields. |
| `DeleteProfile` | `[profile_id: string]` | → null | Delete a profile + its samples. |
| `SetProfileAvatar` | `[profile_id: string, src: string, mode: string, sx: integer, sy: integer, sw: integer, sh: integer]` | → null | Attach avatar + crop rect. `mode` `"circle"`/`"panel"`; empty `src` re-crops. |
| `ExportProfile` | `[profile_id: string, dest: string]` | → null | Write a portable `.zip`. |
| `ImportProfile` | `[src: string]` | string (profile_id) | Import from an exported `.zip`. |
| `AddSample` | `[profile_id: string, audio_path: string, reference_text: string]` | string (JSON `{sample_id, reference_text}`) | Add a reference sample; empty `reference_text` auto-transcribes. |
| `DeleteSample` | `[sample_id: string]` | → null | Delete a reference sample. |
| `UpdateSampleText` | `[profile_id: string, sample_id: string, text: string]` | → null | Correct a sample's reference transcript. |

### 4.3 Personality LLM (async → `LlmResult`)

| Method | Params | Result | Semantics |
|---|---|---|---|
| `ComposeProfile` | `[voice_id: string, prompt: string]` | integer (req_id) | Compose in-character text; `0` if no personality. Result via `LlmResult`. |
| `RewriteProfile` | `[voice_id: string, text: string]` | integer (req_id) | Rewrite `text` in the voice's personality; `0` if none/empty. Result via `LlmResult`. |
| `RefineTranscript` | `[text: string]` | integer (req_id) | Clean a dictation transcript; `0` if empty. Result via `LlmResult`. |

### 4.4 Async transcription & voice conversion (→ signals)

| Method | Params | Result | Semantics |
|---|---|---|---|
| `TranscribeFile` | `[audio_path: string]` | integer (req_id) | Async transcription for long files. Partials via `TranscribeProgress`; final via `TranscribeResult`. |
| `ConvertVoice` | `[audio_path: string, profile_id: string, engine: string, label: string, transcript: string, mode: string, semitones: integer]` | integer (gen_id) | Style-preserved voice conversion (⇄). `engine` `""`=default; `mode` `"music"`=song pipeline. Progress/errors via `GenerationProgress`; auto-plays + lands in history like `Speak`. |

### 4.5 Voice-changer source clips

| Method | Params | Result | Semantics |
|---|---|---|---|
| `SaveSourceClip` | `[path: string, name: string, transcript: string, kind: string]` | string (clip_id, `""` on failure) | Copy an audio file into the clip store. Empty name → time-based default. `kind` `"speech"`\|`"music"` is the vc-mode active at save time (the rail filters on it, badges `"music"` with ♫); values other than `"music"` coerce to `"speech"`. |
| `SetSourceClipTranscript` | `[clip_id: string, transcript: string]` | → null | Backfill a clip's transcript cache. |
| `ListSourceClips` | `[]` | string (JSON array) | Saved source clips, newest first. |
| `DeleteSourceClip` | `[clip_id: string]` | → null | Delete a saved clip (row + file). |
| `PlayFile` | `[path: string, title: string]` | integer (gen_id) | Audition any local audio file (`0` if unreadable). Empty `title` → file stem. |

### 4.6 Generation history

| Method | Params | Result | Semantics |
|---|---|---|---|
| `ListHistory` | `[]` | string (JSON array) | Saved generations, newest first. |
| `PlayHistory` | `[hid: string]` | integer (gen_id) | Replay a stored clip (`0` if not found). |
| `PlayHistoryAt` | `[hid: string, pct: number]` | integer (gen_id) | Replay from fraction `pct` (0..1). |
| `PlaySample` | `[sample_id: string]` | integer (gen_id) | Audition a profile reference sample (`0` if not found). |
| `PausePlayback` | `[]` | → null | Pause current playback. |
| `ResumePlayback` | `[]` | → null | Resume current playback. |
| `SeekPlayback` | `[pct: number]` | → null | Seek to fraction `pct` (0..1). |
| `SetVolume` | `[volume: number]` | → null | Playback volume 0..1, applied live. |

### 4.7 Effects (presets & chain editor)

| Method | Params | Result | Semantics |
|---|---|---|---|
| `ListEffectPresets` | `[]` | string (JSON) | Built-in + user effect presets `[{id,name,description}]`. |
| `SetEffect` | `[preset_id: string]` | → null | Preset applied to subsequent generations (`""`=none). |
| `SetStyle` | `[instruct: string]` | → null | Delivery direction baked into generations (`""`=neutral). |
| `ApplyHistoryEffects` | `[hid: string, preset_id: string]` | string (new hid) | Re-process a history clip through a preset (new row). |
| `ListEffects` | `[]` | string (JSON) | Effect definitions for the chain editor. |
| `GetEffectPreset` | `[preset_id: string]` | string (JSON, `""` if unknown) | Full preset incl. chain. |
| `CreateEffectPreset` | `[name: string, description: string, chain_json: string]` | string (id, `""` on invalid/dup) | Create a user preset. |
| `UpdateEffectPreset` | `[preset_id: string, name: string, description: string, chain_json: string]` | boolean | Rewrite a user preset (builtins immutable). |
| `DeleteEffectPreset` | `[preset_id: string]` | boolean | Delete a user preset. |
| `PreviewEffects` | `[hid: string, chain_json: string]` | integer (gen_id) | Play a history clip through an ad-hoc chain (nothing saved); `0` on invalid. |
| `StarHistory` | `[hid: string, starred: boolean]` | → null | Star/unstar a history entry. |
| `SetHistoryTags` | `[hid: string, tags_json: string]` | → null | Replace a history entry's tags (JSON array of strings). |
| `DeleteHistory` | `[hid: string]` | → null | Delete a history entry (row + file). |
| `RegenerateHistory` | `[hid: string]` | integer (gen_id) | Re-run the generation behind a row (`0` if source gone). |
| `ExportPackage` | `[hid: string, dest: string]` | → null | Write a `.zip` package for a history entry. |
| `HistoryAudioPath` | `[hid: string]` | string | Absolute WAV path of a history entry. |

### 4.8 Trim

| Method | Params | Result | Semantics |
|---|---|---|---|
| `FileEnvelope` | `[path: string]` | string (JSON `{bars,duration}`) | Waveform bars + duration of any local audio file (`"{}"` if unreadable). |
| `TrimAudio` | `[path: string, start_s: number, end_s: number]` | string (path, `""` on failure) | Cut a recording to `[start_s, end_s)`. |
| `TrimHistoryClip` | `[hid: string, start_s: number, end_s: number]` | boolean | Cut a history clip in place. |
| `PlayFileAt` | `[path: string, title: string, pct: number]` | integer (gen_id) | `PlayFile` starting at fraction `pct` — trim-preview. |

### 4.9 Transcription captures (text only)

| Method | Params | Result | Semantics |
|---|---|---|---|
| `SaveCapture` | `[text: string]` | string (capture_id, `""` if empty) | Save a transcript as a capture. |
| `ListCaptures` | `[]` | string (JSON array) | Captures, newest first. |
| `UpdateCapture` | `[capture_id: string, text: string]` | → null | Replace a capture's text in place. |
| `DeleteCapture` | `[capture_id: string]` | → null | Delete a capture. |

### 4.10 Model management & settings

| Method | Params | Result | Semantics |
|---|---|---|---|
| `ListModels` | `[]` | string (JSON array) | Model catalog (id, display, category, size, status…). |
| `Hardware` | `[]` | string (JSON) | Detected hardware (cores, ram_gb, gpu, gpu_name). |
| `DownloadModel` | `[model_id: string]` | boolean | Start a download (progress via `ModelProgress`); `false` if unknown id. |
| `DeleteModel` | `[model_id: string]` | → null | Delete a downloaded model's files. |
| `SetActiveModel` | `[model_id: string]` | string (category) | Make a model active for its category; returns the category. |
| `GetSettings` | `[]` | string (JSON `{stored,effective}`) | Persisted engine settings + effective values. |
| `SetSetting` | `[key: string, value_json: string]` | → null | Set one engine setting (JSON-encoded value; `null` clears). |
| `Cancel` | `[gen_id: integer]` | → null | Cancel an in-flight generation. |

---

## 5. Properties → RPC methods + `PropertiesChanged`

The two D-Bus read-only properties become explicit getters, plus a notification
mirroring the D-Bus `org.freedesktop.DBus.Properties.PropertiesChanged` that the
engine emits after warmup (`emit_properties_changed({"ModelLoaded": True})` in
`service.py`).

| RPC method | Params | Result | D-Bus origin |
|---|---|---|---|
| `GetModelLoaded` | `[]` | boolean | `ModelLoaded` property |
| `GetBackend` | `[]` | string (`"cuda"`\|`"rocm"`\|`"cpu"`) | `Backend` property |

**`PropertiesChanged` notification** (server → client). Unlike the signals in §6,
its `params` is a **by-name object** of changed property → new value (mirroring
D-Bus semantics), not a positional array:

```json
{"jsonrpc":"2.0","method":"PropertiesChanged","params":{"ModelLoaded":true}}
```

Property names inside the object are the **PascalCase** D-Bus names
(`ModelLoaded`, `Backend`). Today the engine only ever emits
`{"ModelLoaded": true}` (once, at end of warmup), but the notification is defined
generally so a future `Backend` change would carry `{"Backend":"cuda"}`.

---

## 6. Signals → notifications

Each D-Bus signal becomes an id-less JSON-RPC **notification**, `method` = the
signal name, `params` = a **positional array** in the same order/types as the
D-Bus signature. Broadcast to all authenticated connections.

| Notification | Params `[name: type, …]` | D-Bus sig | Meaning |
|---|---|---|---|
| `GenerationProgress` | `[gen_id: integer, state: string, pct: number]` | `usd` | Generation state; `state` prefixed `"error: …"` on failure (see §7.3). |
| `AudioLevel` | `[gen_id: integer, rms: number]` | `ud` | Live output RMS during playback. |
| `PlaybackInfo` | `[gen_id: integer, clip_id: string, title: string, duration: number, bars: string]` | `ussds` | Playback of a clip started; `bars` is a JSON array string. |
| `PlaybackProgress` | `[gen_id: integer, pct: number]` | `ud` | Playback position 0..1, per audio block. |
| `LlmResult` | `[req_id: integer, text: string]` | `us` | Result of Compose/Rewrite/Refine (`""` = failed/none). |
| `TranscribeProgress` | `[req_id: integer, partial: string]` | `us` | Live partial transcript from `TranscribeFile`. |
| `TranscribeResult` | `[req_id: integer, text: string, error: boolean]` | `usb` | Final transcript from `TranscribeFile`. `error` `true` = the stt stack raised (with `text` `""`); this is **distinct** from a legitimately-empty transcript (`error` `false`, `text` `""`) so the app can show "transcription failed" vs. "no speech detected". |
| `ModelProgress` | `[model_id: string, pct: number, status: string]` | `sds` | Download progress; `status` `"downloading"`\|`"done"`\|`"error"`. |
| `SpeakStarted` | `[gen_id: integer]` | `u` | A generation's playback lifecycle began. |
| `SpeakEnded` | `[gen_id: integer]` | `u` | A generation's playback lifecycle ended. |

---

## 7. Error model, readiness, timeouts

### 7.1 Error codes

Errors use the standard JSON-RPC 2.0 `error` object
(`{"code":<int>,"message":<string>,"data"?:<any>}`).

| Code | Name | When |
|---|---|---|
| `-32700` | Parse error | Frame is not valid JSON. |
| `-32600` | Invalid request | Not a valid JSON-RPC 2.0 request object. |
| `-32601` | Method not found | Unknown method name. |
| `-32602` | Invalid params | Wrong arity/types for a known method. |
| `-32603` | Internal error | Transport/server bug not attributable to the engine call. |
| `-32000` | **Engine error** | An **exception raised inside a method handler** (the D-Bus analog: dbus_next turns it into an error reply; zbus surfaces it as `zbus::Error`). |
| `-32001` | Unauthorized | `Authenticate` with a wrong token. |
| `-32002` | Not authenticated | Any method issued before a successful `Authenticate`. |

### 7.2 Error message text is load-bearing — preserve it verbatim

For `-32000`, **`error.message` MUST be the exact human-readable string a D-Bus
caller would see** — i.e. `str(exception)` (the same text dbus_next puts in the
error body and zbus exposes via `zbus::Error`). The Rust app **string-matches**
this text and MUST behave identically on both transports. Known matched strings
today:

- **`profile_err_msg`** (`app/src/main.rs` ~L1129) checks
  `.contains("UNIQUE constraint failed: profiles.name")` — the raw
  `sqlite3.IntegrityError` text from a duplicate profile name (`CreateProfile` /
  `CloneVoice`) — and rewrites it to *"A voice with that name already exists."*
  If the RPC transport mangled or wrapped this text, the friendly message would
  break and the raw SQL error would leak to the user.

Therefore:
- The **server** puts `str(exc)` (untruncated) into `error.message` for any
  handler exception, and sets `code` = `-32000`. (Optionally include
  `data: {"type": "<ExceptionClassName>"}` — informational only; the app matches
  on `message`.)
- The **Rust client's** error type MUST render `error.message` **verbatim** in
  its `Display`/`to_string()`, so existing `.to_string().contains(...)` checks in
  the app match unchanged. In the unified `EngineClient`, the shared error type
  must expose this text the same way `zbus::Error::to_string()` does. Do **not**
  prefix the message with the code in a way that would break a `contains` check —
  a `contains` still passes with a prefix, but keep the raw message a contiguous
  substring.

Note on in-band vs. protocol errors: some engine failures are **not** exceptions.
A failed **generation** (`Speak`/`ConvertVoice`) returns a valid `gen_id` and
reports the failure through a `GenerationProgress` notification whose `state` is
`"error: <text>"` (see `_start_speak` / `_start_convert`). That path is unchanged
here — it travels as a normal notification, not a JSON-RPC error. Only synchronous
handler exceptions become `-32000`.

### 7.3 Readiness / lifecycle

There is no bus-name to claim, so readiness is defined by the handshake:

1. **Engine up** = the client can open the socket, `Authenticate` succeeds, and a
   `GetBackend` request returns a result. This is the RPC analog of the D-Bus
   name being claimed. (On Win/mac the app spawns + supervises the engine —
   MULTIPLATPLAN §1.2 — so "up" also means the child process is alive and the
   discovery file is current.)
2. **Models warmed** = `GetModelLoaded` returns `true`, or the client has
   received `PropertiesChanged {"ModelLoaded": true}`. As on D-Bus, the server
   binds/authenticates immediately and warms models in the background, so a
   client can connect and drive the UI (with empty/loading states) before
   warmup completes. A client SHOULD call `GetModelLoaded` once right after auth
   (to catch the case where warmup finished before it connected) and otherwise
   wait for the notification.

### 7.4 Timeouts — do not impose one

**The client MUST NOT apply a default request timeout.** Long-running work never
blocks: the async methods return an id immediately and results arrive as
notifications. The one method that *does* block its response — **`Transcribe`** —
can legitimately take a very long time on a long file (this is exactly why
`TranscribeFile` exists). Unlike D-Bus, which caps a method reply at ~25 s, this
transport imposes **no** reply deadline; the client waits indefinitely (or until
the socket closes / the user cancels). If a client wants cancellability it should
prefer the async variants and `Cancel`, not a timeout.

---

## 8. Worked examples

### 8.1 Handshake

```json
→ {"jsonrpc":"2.0","method":"Authenticate","params":["b9f1c0e2…6d8"],"id":0}
← {"jsonrpc":"2.0","result":true,"id":0}
→ {"jsonrpc":"2.0","method":"GetBackend","params":[],"id":1}
← {"jsonrpc":"2.0","result":"cuda","id":1}
→ {"jsonrpc":"2.0","method":"GetModelLoaded","params":[],"id":2}
← {"jsonrpc":"2.0","result":false,"id":2}
… later, warmup finishes …
← {"jsonrpc":"2.0","method":"PropertiesChanged","params":{"ModelLoaded":true}}
```

### 8.2 `Speak` round-trip (request → response → follow-up notifications)

```json
→ {"jsonrpc":"2.0","method":"Speak","params":["Hello there.","builtin:kokoro:af_heart"],"id":7}
← {"jsonrpc":"2.0","result":42,"id":7}

← {"jsonrpc":"2.0","method":"SpeakStarted","params":[42]}
← {"jsonrpc":"2.0","method":"GenerationProgress","params":[42,"synthesizing",0.0]}
← {"jsonrpc":"2.0","method":"GenerationProgress","params":[42,"playing",1.0]}
← {"jsonrpc":"2.0","method":"PlaybackInfo","params":[42,"a1b2c3d4","Heart",3.52,"[0.02,0.31,0.55,…]"]}
← {"jsonrpc":"2.0","method":"AudioLevel","params":[42,0.18]}
← {"jsonrpc":"2.0","method":"PlaybackProgress","params":[42,0.25]}
← {"jsonrpc":"2.0","method":"AudioLevel","params":[42,0.41]}
← {"jsonrpc":"2.0","method":"PlaybackProgress","params":[42,0.75]}
← {"jsonrpc":"2.0","method":"SpeakEnded","params":[42]}
```

(If synthesis fails, instead of the `playing`/`PlaybackInfo` frames the client
sees `{"jsonrpc":"2.0","method":"GenerationProgress","params":[42,"error: <text>",0.0]}`
followed by `SpeakEnded`.)

### 8.3 Engine error (duplicate profile name)

```json
→ {"jsonrpc":"2.0","method":"CreateProfile","params":["{\"name\":\"Nova\",\"voice_type\":\"cloned\"}"],"id":9}
← {"jsonrpc":"2.0","error":{"code":-32000,"message":"UNIQUE constraint failed: profiles.name","data":{"type":"IntegrityError"}},"id":9}
```

The Rust app's `profile_err_msg` sees `…contains("UNIQUE constraint failed: profiles.name")`
in the error's `to_string()` and shows *"A voice with that name already exists."* —
identical to the D-Bus path.

### 8.4 Async transcription

```json
→ {"jsonrpc":"2.0","method":"TranscribeFile","params":["/path/lecture.wav"],"id":12}
← {"jsonrpc":"2.0","result":3,"id":12}
← {"jsonrpc":"2.0","method":"TranscribeProgress","params":[3,"So today we"]}
← {"jsonrpc":"2.0","method":"TranscribeProgress","params":[3,"So today we will cover"]}
← {"jsonrpc":"2.0","method":"TranscribeResult","params":[3,"So today we will cover attention.",false]}
```

---

## 9. Versioning

- The discovery file carries **`protocol: 1`**.
- **`GetProtocolVersion`** (`params: []` → integer) returns the same value.
- Version **1** is this document.
- Compatibility rule: a client compares its supported version to the discovery
  file's `protocol` **before** connecting; on mismatch it refuses and surfaces an
  "update required" message rather than connecting and failing method-by-method.
  Any breaking change to a method signature, signal, error code, the handshake,
  or the discovery-file schema bumps `protocol`. Purely additive methods/signals
  MAY reuse the same version only if old clients keep working unchanged; when in
  doubt, bump.

---

## 10. Appendix A — Rust unified event enum

Both implementers agree on these variant names and payloads. The RPC client
decodes each notification into one of these; the zbus impl maps its signal
streams to the same enum. This is the type the app's `tokio::select!` loop
consumes regardless of transport (MULTIPLATPLAN §1.1). Field names are derived
from the `lib.rs` signal argument names; types from the D-Bus signature (`u`→
`u32`, `i`→`i32`, `d`→`f64`, `s`→`String`).

```rust
pub enum EngineEvent {
    GenerationProgress { gen_id: u32, state: String, pct: f64 },
    AudioLevel         { gen_id: u32, rms: f64 },
    PlaybackInfo       { gen_id: u32, clip_id: String, title: String, duration: f64, bars: String },
    PlaybackProgress   { gen_id: u32, pct: f64 },
    LlmResult          { req_id: u32, text: String },
    TranscribeProgress { req_id: u32, partial: String },
    TranscribeResult   { req_id: u32, text: String, error: bool },
    ModelProgress      { model_id: String, pct: f64, status: String },
    SpeakStarted       { gen_id: u32 },
    SpeakEnded         { gen_id: u32 },

    /// Mirrors the D-Bus PropertiesChanged / the RPC `PropertiesChanged`
    /// notification. In practice only carries `{"ModelLoaded": true}` today.
    PropertiesChanged  { changed: std::collections::BTreeMap<String, serde_json::Value> },
}
```

Notes for the Rust client:
- `bars` and the various `*_json` returns stay `String` (they carry JSON the app
  parses downstream), matching `lib.rs`.
- `PropertiesChanged.changed` keys are the PascalCase property names
  (`ModelLoaded`, `Backend`); values are decoded as `serde_json::Value` (a bool
  for `ModelLoaded`, a string for `Backend`).
- The unified `EngineClient` error type must expose the `-32000` `error.message`
  text verbatim through `Display` (see §7.2) so `app/src/main.rs`'s substring
  checks keep working.

---

## 11. Appendix B — Completeness check

- `service.py`: **65** `@method()`, **10** `@signal()`, **2** `@dbus_property`.
- `lib.rs`: **77** `fn` = **65** methods + **10** signals + **2** properties.
- §4's method table lists all **65** methods (4.1–4.10:
  4+11+3+2+5+8+16+4+4+8 = **65**); §6 lists all **10** signals; §5 covers both
  properties.
- **No `lib.rs` ↔ `service.py` mismatch** was found: names (PascalCase in
  `service.py`, snake_case-of-the-same in `lib.rs`), arities, and signatures
  correspond one-to-one. (The design brief's "68 methods / ~50 methods" figures
  are approximate; the exact current count is **65**.)
- `Authenticate`, `GetModelLoaded`, `GetBackend`, `GetProtocolVersion` are
  RPC-transport-only additions with no D-Bus method analog (D-Bus uses native
  properties and has no app-level auth or version handshake).

---

## 12. Implementation notes (as-built, 2026-07-24)

Both sides shipped against this spec and passed a live cross-language smoke
test on Windows (`shared/examples/rpc_smoke.rs` against a real engine). Two
deliberately conservative readings in the server, recorded here so they're
contract, not accident:

- **§7.1 `-32602` covers arity only.** The server validates parameter *count*
  up front; a wrong-*typed* param that raises inside a handler surfaces as
  `-32000` with the real exception text. Full per-method type validation would
  need a schema this spec doesn't define, and the Rust client generates
  correctly-typed params from the same signature table.
- **§2.3 close-on-1008 applies only to auth failures.** Pre-auth frames that
  are malformed (`-32700`) or not valid JSON-RPC (`-32600`) get an error reply
  but the socket stays open; only `-32001`/`-32002` close with 1008.

Client-side notes: the Rust client rebuilds the URL from the discovery `port`
(ignoring `url` and `pid`), reserves request id 0 for `Authenticate`, and
starts app requests at 1.

---

## 13. Supervised lifecycle (seam 1.2 — Win/mac engine spawning)

On Linux the engine's lifecycle belongs to systemd + D-Bus activation and
none of this section applies. On Windows/macOS the app owns the engine as a
supervised child process (MULTIPLATPLAN §1.2).

### 13.1 Supervised mode (engine side)

- The parent sets `SYRINX_SUPERVISED=1` in the child's environment and keeps
  the child's **stdin an open pipe it never writes to**.
- Under `SYRINX_SUPERVISED=1` the engine runs a stdin watchdog (daemon
  thread blocking on a read): stdin EOF/error means the parent is gone —
  the engine **removes the discovery file explicitly, then exits
  immediately** (`os._exit(0)`; the explicit cleanup is required because
  `_exit` skips `finally`). This covers app crashes as well as graceful
  quits — the pipe closes either way.
- Without the env var, behavior is exactly as before (manual/dev engines are
  never tied to any app).

### 13.2 Adopt-or-spawn (app side)

1. Try the discovery file + connect + `Authenticate` first. Success ⇒ an
   engine is already running (dev flow): **adopt it — do not supervise it,
   do not kill it on quit** (the Windows analog of Linux's "manual dev
   engines survive app quits").
2. Any failure (no file, stale file from a hard kill, refused connect, bad
   token) ⇒ treat as absent and **spawn**, resolving the engine executable
   in this order:
   1. `SYRINX_ENGINE_CMD` — absolute path to the engine executable, used
      verbatim (dev/CI override);
   2. `engine/.venv/Scripts/syrinx-engine.exe` (Windows) /
      `engine/.venv/bin/syrinx-engine` (mac) relative to the current
      working directory;
   3. the same, relative to each ancestor of the app executable's directory
      (covers `target/debug/syrinx-app.exe` in a checkout);
   4. `syrinx-engine` on `PATH`.
3. After spawning: poll connect-with-handshake with backoff; the first
   successful `Authenticate` + `GetBackend` round-trip is readiness (§7.3).
   The pre-existing stale discovery file, if any, is irrelevant — the child
   rewrites it on boot.

### 13.3 Supervision (app side)

- Spawned child exits unexpectedly ⇒ respawn with exponential backoff
  (1 s doubling to a 30 s cap, reset after 60 s of stable uptime), then
  reconnect and re-run the initial data loads. No tight crash loops.
- App quits ⇒ close the child's stdin (the watchdog exits the engine); if
  the process is still alive after a short grace (~3 s), kill it.
- An **adopted** (externally started) engine whose socket drops is treated
  like case 2 above: fall through to spawn.

---

## 14. Recording methods (seam 1.3 — Win/mac capture)

Four transport-agnostic engine methods (present on BOTH transports and in
the D-Bus interface — the drift guards require it; the Linux app simply
keeps its native `parecord`/`pactl` path and never calls them). This
extends the §4 method table: the surface is now **69** methods. sounddevice
(PortAudio) is the backend; its import is lazy per the engine-wide rule.

| Method | Params | Returns | Semantics |
|---|---|---|---|
| `ListRecordingDevices` | `[]` | string | JSON array `[{"id": str, "name": str, "default": bool}]` of input devices; `"[]"` when enumeration fails. `id` is stable enough to persist in settings (prefer name-based ids over bare PortAudio indices, which reshuffle on hotplug). |
| `StartRecording` | `[device_id: str]` | string | Start capturing mic input to a WAV (PCM16 mono, 48 kHz or device-native — implementation reports which). `""` device = system default input. Returns a recording id, `""` on failure (device missing/busy). A second `StartRecording` while one is live **cancels the previous** (latest-wins, mirroring playback epoch semantics). |
| `StopRecording` | `[rec_id: str]` | string | Stop + finalize the WAV; returns its absolute path (`""` for an unknown/already-stopped id). The file lives in engine-owned scratch space and stays until consumed (AddSample/ConvertVoice/TranscribeFile all take paths). |
| `CancelRecording` | `[rec_id: str]` | null | Stop and delete the file. Unknown id is a no-op. |

No new signals (the recording UI has no live meter today; a
`RecordingLevel` signal can be added later if the meter gets wired).
