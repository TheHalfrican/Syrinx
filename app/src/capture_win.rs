//! WASAPI loopback system-audio capture — the Windows twin of Linux's
//! `parecord <sink>.monitor`. The app owns system capture natively (mirroring
//! the Linux decision in main.rs's capture seam); the engine only ever does mic
//! capture, so nothing here touches the RPC protocol.
//!
//! A render endpoint is opened in shared mode with
//! `AUDCLNT_STREAMFLAGS_LOOPBACK`, and `IAudioCaptureClient` drains what the
//! speakers play on a dedicated thread, writing mono PCM16 to an app-chosen WAV.
//! All COM lives on that thread (the interfaces are `!Send`); `start` only hands
//! back a `Send` handle after a setup handshake, so activation errors surface as
//! an `io::Error` exactly like a failed `parecord` spawn.
//!
//! Loopback quirk: while the endpoint is idle WASAPI delivers no packets at all.
//! To keep the WAV duration honest, dry reads pad the timeline with silence up
//! to the elapsed wall-clock position (`silence_pad_frames`) rather than leaving
//! a gap — the file's length then matches how long the button was held.
#![cfg(target_os = "windows")]

use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK, DEVICE_STATE_ACTIVE,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
use windows::Win32::Media::Multimedia::{KSDATAFORMAT_SUBTYPE_IEEE_FLOAT, WAVE_FORMAT_IEEE_FLOAT};
use windows::Win32::System::Com::StructuredStorage::PropVariantClear;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED, STGM_READ,
};

// SILENT is a typed WASAPI flag; GetBuffer hands flags back as a raw u32.
const BUFFERFLAGS_SILENT: u32 = 2;
// Poll cadence for the drain loop. Small enough that stop() joins promptly, big
// enough not to spin a core while idle.
const POLL: Duration = Duration::from_millis(10);
// Shared-mode capture buffer. 200 ms is ample headroom between 10 ms polls.
const BUFFER_HNS: i64 = 2_000_000; // 100-ns units

/// A live loopback capture. The drain thread owns the COM stack and the WAV;
/// this handle just flips the stop flag and joins.
pub struct Loopback {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    wav: String,
}

impl Loopback {
    /// Begin capturing the given render endpoint (`None` = the default output).
    /// Blocks only until the drain thread reports its COM setup succeeded (a few
    /// ms), so a bad device id / activation failure returns here as an error
    /// rather than a silently dead capture — same contract as spawning
    /// `parecord`.
    pub fn start(device_id: Option<&str>, wav: &str) -> io::Result<Loopback> {
        let stop = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let dev = device_id.map(|s| s.to_string());
        let wav_path = wav.to_string();
        let (t_stop, t_err, t_wav) = (stop.clone(), errored.clone(), wav_path.clone());

        let thread = std::thread::Builder::new()
            .name("wasapi-loopback".into())
            .spawn(move || capture_thread(dev, t_wav, t_stop, t_err, tx))
            .map_err(io::Error::other)?;

        match rx.recv() {
            Ok(Ok(())) => Ok(Loopback { thread: Some(thread), stop, errored, wav: wav_path }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(io::Error::other(e))
            }
            // thread panicked before the handshake
            Err(_) => {
                let _ = thread.join();
                Err(io::Error::other("loopback thread exited during setup"))
            }
        }
    }

    /// Stop + finalize; returns the WAV path (empty on a mid-capture failure).
    pub fn stop(mut self) -> String {
        self.join();
        if self.errored.load(Ordering::SeqCst) {
            String::new()
        } else {
            self.wav.clone()
        }
    }

    /// Stop and throw the file away (modal cancel).
    pub fn discard(mut self) {
        self.join();
        let _ = std::fs::remove_file(&self.wav);
    }

    /// Did the capture die on its own (a mid-loop COM/WASAPI error)? Mirrors the
    /// Linux `parecord`-child poll.
    pub fn died(&self) -> bool {
        self.errored.load(Ordering::SeqCst)
    }

    fn join(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Clamp a float sample to i16 (mix formats are almost always 32-bit float).
pub(crate) fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Silence to pad on a dry read: how many frames the timeline is behind the
/// wall clock. Saturating, so a buffered burst that ran ahead pads nothing.
pub(crate) fn silence_pad_frames(rate: u32, elapsed_secs: f64, frames_written: u64) -> u64 {
    let expected = (elapsed_secs.max(0.0) * rate as f64) as u64;
    expected.saturating_sub(frames_written)
}

/// Enumerate active render endpoints as `(device id, friendly name)` for the ⚙
/// tap picker. Self-contained COM (the caller's worker thread is not COM-init'd).
pub fn enumerate_render_devices() -> Vec<(String, String)> {
    let mut out = Vec::new();
    unsafe {
        let guard = match ComGuard::new() {
            Ok(g) => g,
            Err(_) => return out,
        };
        let res = (|| -> windows::core::Result<()> {
            let enumr: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
            let coll = enumr.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)?;
            for i in 0..coll.GetCount()? {
                let dev = coll.Item(i)?;
                let id = pwstr_take(dev.GetId()?);
                let name = friendly_name(&dev).unwrap_or_else(|| "Output device".into());
                if !id.is_empty() {
                    out.push((id, name));
                }
            }
            Ok(())
        })();
        let _ = res;
        drop(guard);
    }
    out
}

/// Read a device's friendly name from its property store (VT_LPWSTR).
unsafe fn friendly_name(dev: &IMMDevice) -> Option<String> {
    let store = dev.OpenPropertyStore(STGM_READ).ok()?;
    let mut prop = store.GetValue(&PKEY_Device_FriendlyName).ok()?;
    let pw = prop.Anonymous.Anonymous.Anonymous.pwszVal;
    let s = if pw.is_null() { None } else { pw.to_string().ok() };
    let _ = PropVariantClear(&mut prop);
    s.filter(|s| !s.is_empty())
}

/// Consume a CoTaskMem-allocated PWSTR into an owned String and free it.
unsafe fn pwstr_take(p: PWSTR) -> String {
    if p.is_null() {
        return String::new();
    }
    let s = p.to_string().unwrap_or_default();
    CoTaskMemFree(Some(p.as_ptr() as *const _));
    s
}

/// Balanced CoInitialize/CoUninitialize for a scope. Only uninits if this scope
/// actually initialized (skips the RPC_E_CHANGED_MODE case).
struct ComGuard {
    uninit: bool,
}
impl ComGuard {
    unsafe fn new() -> Result<Self, String> {
        let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
        if hr.is_ok() {
            Ok(ComGuard { uninit: true })
        } else {
            // Already initialized on this thread with an incompatible mode: use
            // the existing apartment, but don't unbalance its ref count.
            Ok(ComGuard { uninit: false })
        }
    }
}
impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.uninit {
            unsafe { CoUninitialize() };
        }
    }
}

/// The drain thread: set up COM + WASAPI, report the result, then loop writing
/// PCM16 (real packets, or silence padding on dry reads) until stopped.
fn capture_thread(
    device_id: Option<String>,
    wav: String,
    stop: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    tx: mpsc::Sender<Result<(), String>>,
) {
    unsafe {
        let _guard = match ComGuard::new() {
            Ok(g) => g,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        let mut state = match setup(device_id.as_deref(), &wav) {
            Ok(s) => s,
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        };
        // Setup succeeded — the handle is now live.
        if tx.send(Ok(())).is_err() {
            return;
        }
        if let Err(e) = drain(&mut state, &stop) {
            tracing::error!("wasapi loopback drain failed: {e}");
            errored.store(true, Ordering::SeqCst);
        }
        // Finalize the WAV header regardless (partial capture is still useful).
        let _ = state.client.Stop();
        if let Err(e) = state.wav.finalize() {
            tracing::error!("wasapi loopback wav finalize failed: {e}");
            errored.store(true, Ordering::SeqCst);
        }
    }
}

/// Decoded mix-format shape + the live WASAPI objects and WAV sink.
struct CaptureState {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    fmt: MixFormat,
    wav: WavWriter,
    start: Instant,
    frames_written: u64,
}

#[derive(Clone, Copy)]
struct MixFormat {
    channels: u16,
    rate: u32,
    bits: u16,
    block_align: u16,
    is_float: bool,
}

unsafe fn setup(device_id: Option<&str>, wav: &str) -> Result<CaptureState, String> {
    let enumr: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(estr)?;
    let device: IMMDevice = match device_id {
        Some(id) if !id.is_empty() => {
            let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
            enumr.GetDevice(PCWSTR(wide.as_ptr())).map_err(estr)?
        }
        _ => enumr
            .GetDefaultAudioEndpoint(eRender, eConsole)
            .map_err(estr)?,
    };
    let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(estr)?;
    let pwfx = client.GetMixFormat().map_err(estr)?;
    if pwfx.is_null() {
        return Err("GetMixFormat returned null".into());
    }
    let fmt = parse_format(pwfx)?;
    client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            BUFFER_HNS,
            0,
            pwfx,
            None,
        )
        .map_err(estr)?;
    CoTaskMemFree(Some(pwfx as *const _));
    let capture: IAudioCaptureClient = client.GetService().map_err(estr)?;
    let wav = WavWriter::create(wav, fmt.rate).map_err(|e| e.to_string())?;
    client.Start().map_err(estr)?;
    Ok(CaptureState {
        client,
        capture,
        fmt,
        wav,
        start: Instant::now(),
        frames_written: 0,
    })
}

/// Interpret the WAVEFORMATEX(TENSIBLE) the endpoint hands us. We only need to
/// know channels/rate/width and whether samples are float or int PCM.
/// WAVEFORMATEX(TENSIBLE) are `repr(packed)`, so every field is read unaligned.
unsafe fn parse_format(pwfx: *const WAVEFORMATEX) -> Result<MixFormat, String> {
    use std::ptr::addr_of;
    let tag = addr_of!((*pwfx).wFormatTag).read_unaligned() as u32;
    let bits = addr_of!((*pwfx).wBitsPerSample).read_unaligned();
    let is_float = if tag == WAVE_FORMAT_IEEE_FLOAT {
        true
    } else if tag == WAVE_FORMAT_PCM {
        false
    } else if tag == WAVE_FORMAT_EXTENSIBLE {
        let ext = pwfx as *const WAVEFORMATEXTENSIBLE;
        addr_of!((*ext).SubFormat).read_unaligned() == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT
    } else {
        return Err(format!("unsupported mix format tag {tag}"));
    };
    if bits != 16 && bits != 32 {
        return Err(format!("unsupported bit depth {bits}"));
    }
    Ok(MixFormat {
        channels: addr_of!((*pwfx).nChannels).read_unaligned().max(1),
        rate: addr_of!((*pwfx).nSamplesPerSec).read_unaligned(),
        bits,
        block_align: addr_of!((*pwfx).nBlockAlign).read_unaligned(),
        is_float,
    })
}

unsafe fn drain(state: &mut CaptureState, stop: &AtomicBool) -> windows::core::Result<()> {
    while !stop.load(Ordering::SeqCst) {
        if state.capture.GetNextPacketSize()? == 0 {
            // Dry read: nothing is playing. Pad up to the wall clock so the WAV
            // duration tracks real time instead of collapsing to the audible
            // stretches only.
            let elapsed = state.start.elapsed().as_secs_f64();
            let pad = silence_pad_frames(state.fmt.rate, elapsed, state.frames_written);
            if pad > 0 {
                let _ = state.wav.write_silence(pad);
                state.frames_written += pad;
            }
            std::thread::sleep(POLL);
            continue;
        }
        loop {
            let mut pdata: *mut u8 = std::ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            state
                .capture
                .GetBuffer(&mut pdata, &mut frames, &mut flags, None, None)?;
            if frames > 0 {
                if flags & BUFFERFLAGS_SILENT != 0 || pdata.is_null() {
                    let _ = state.wav.write_silence(frames as u64);
                } else {
                    let bytes = frames as usize * state.fmt.block_align as usize;
                    let buf = std::slice::from_raw_parts(pdata, bytes);
                    write_frames(&mut state.wav, buf, &state.fmt);
                }
                state.frames_written += frames as u64;
            }
            state.capture.ReleaseBuffer(frames)?;
            if state.capture.GetNextPacketSize()? == 0 {
                break;
            }
        }
    }
    Ok(())
}

/// Downmix one interleaved WASAPI buffer to mono PCM16.
fn write_frames(wav: &mut WavWriter, buf: &[u8], fmt: &MixFormat) {
    let bps = (fmt.bits / 8) as usize;
    let stride = fmt.block_align as usize;
    let ch = fmt.channels as usize;
    let mut off = 0;
    while off + stride <= buf.len() {
        let mut acc = 0f32;
        for c in 0..ch {
            let o = off + c * bps;
            let v = if fmt.is_float {
                f32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]])
            } else if fmt.bits == 16 {
                i16::from_le_bytes([buf[o], buf[o + 1]]) as f32 / 32768.0
            } else {
                // 32-bit int PCM
                i32::from_le_bytes([buf[o], buf[o + 1], buf[o + 2], buf[o + 3]]) as f32
                    / 2_147_483_648.0
            };
            acc += v;
        }
        let _ = wav.write_sample(f32_to_i16(acc / ch as f32));
        off += stride;
    }
}

/// Minimal streaming mono PCM16 WAV writer with a patch-on-finalize header.
struct WavWriter {
    file: BufWriter<File>,
    data_bytes: u32,
}

impl WavWriter {
    fn create(path: &str, rate: u32) -> io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let mut file = BufWriter::new(File::create(path)?);
        let byte_rate = rate * 2; // mono, 16-bit
        file.write_all(b"RIFF")?;
        file.write_all(&0u32.to_le_bytes())?; // riff size (patched)
        file.write_all(b"WAVE")?;
        file.write_all(b"fmt ")?;
        file.write_all(&16u32.to_le_bytes())?;
        file.write_all(&1u16.to_le_bytes())?; // PCM
        file.write_all(&1u16.to_le_bytes())?; // mono
        file.write_all(&rate.to_le_bytes())?;
        file.write_all(&byte_rate.to_le_bytes())?;
        file.write_all(&2u16.to_le_bytes())?; // block align
        file.write_all(&16u16.to_le_bytes())?; // bits
        file.write_all(b"data")?;
        file.write_all(&0u32.to_le_bytes())?; // data size (patched)
        Ok(WavWriter { file, data_bytes: 0 })
    }

    fn write_sample(&mut self, s: i16) -> io::Result<()> {
        self.file.write_all(&s.to_le_bytes())?;
        self.data_bytes = self.data_bytes.saturating_add(2);
        Ok(())
    }

    fn write_silence(&mut self, frames: u64) -> io::Result<()> {
        const CHUNK: usize = 4096;
        let zeros = [0u8; CHUNK];
        let mut remaining = frames.saturating_mul(2); // bytes, mono 16-bit
        while remaining > 0 {
            let n = remaining.min(CHUNK as u64) as usize;
            self.file.write_all(&zeros[..n])?;
            remaining -= n as u64;
        }
        self.data_bytes = self
            .data_bytes
            .saturating_add((frames.saturating_mul(2)).min(u32::MAX as u64) as u32);
        Ok(())
    }

    fn finalize(mut self) -> io::Result<()> {
        self.file.flush()?;
        let riff = 36u32.saturating_add(self.data_bytes);
        let f = self.file.get_mut();
        f.seek(SeekFrom::Start(4))?;
        f.write_all(&riff.to_le_bytes())?;
        f.seek(SeekFrom::Start(40))?;
        f.write_all(&self.data_bytes.to_le_bytes())?;
        f.flush()?;
        Ok(())
    }
}

fn estr(e: windows::core::Error) -> String {
    e.message()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn f32_to_i16_clamps_and_scales() {
        assert_eq!(f32_to_i16(0.0), 0);
        assert_eq!(f32_to_i16(1.0), 32767);
        assert_eq!(f32_to_i16(-1.0), -32767);
        assert_eq!(f32_to_i16(2.0), 32767); // clamped
        assert_eq!(f32_to_i16(-9.0), -32767); // clamped
        assert_eq!(f32_to_i16(0.5), 16383);
    }

    #[test]
    fn silence_pad_tracks_wall_clock() {
        // 1 s in at 48 kHz with nothing written yet → a full second of pad.
        assert_eq!(silence_pad_frames(48_000, 1.0, 0), 48_000);
        // already caught up → no pad.
        assert_eq!(silence_pad_frames(48_000, 1.0, 48_000), 0);
        // ran ahead of the clock (buffered burst) → saturating, never negative.
        assert_eq!(silence_pad_frames(48_000, 1.0, 60_000), 0);
        // partial second.
        assert_eq!(silence_pad_frames(24_000, 0.5, 0), 12_000);
        // negative elapsed is treated as zero.
        assert_eq!(silence_pad_frames(48_000, -3.0, 0), 0);
    }

    #[test]
    fn wav_header_is_well_formed_after_finalize() {
        let dir = std::env::temp_dir();
        let path = dir.join("syrinx-capwin-headertest.wav");
        let p = path.to_string_lossy().to_string();
        let mut w = WavWriter::create(&p, 48_000).unwrap();
        // 100 frames of audible-ish + 50 frames silence = 150 mono samples.
        for i in 0..100 {
            w.write_sample((i as i16) * 100).unwrap();
        }
        w.write_silence(50).unwrap();
        w.finalize().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        let data_size = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
        assert_eq!(data_size, 150 * 2); // 150 mono i16 samples
        let riff = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        assert_eq!(riff, 36 + 150 * 2);
        assert_eq!(bytes.len() as u32, 44 + 150 * 2);
        let _ = std::fs::remove_file(&path);
    }

    /// Live smoke — NOT run in CI (needs audio actually playing on the default
    /// render endpoint). Drive it with:
    ///   cargo test -p syrinx-app --lib wasapi_loopback_smoke -- --ignored --nocapture
    /// while a sound loops (the test itself loops a Windows chime via SoundPlayer).
    #[test]
    #[ignore]
    fn wasapi_loopback_smoke() {
        // Loop a bundled Windows chime on the default output for the capture window.
        let sound = r"C:\Windows\Media\Alarm01.wav";
        let mut player = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                &format!(
                    "$p = New-Object System.Media.SoundPlayer '{sound}'; $p.PlayLooping(); Start-Sleep -Seconds 6"
                ),
            ])
            .spawn()
            .expect("spawn player");
        std::thread::sleep(Duration::from_millis(400)); // let audio start

        let path = std::env::temp_dir()
            .join("syrinx-capwin-smoke.wav")
            .to_string_lossy()
            .to_string();
        let cap = Loopback::start(None, &path).expect("start loopback");
        std::thread::sleep(Duration::from_secs(3));
        let out = cap.stop();
        let _ = player.kill();
        let _ = player.wait();

        assert!(!out.is_empty(), "stop returned empty path");
        let bytes = std::fs::read(&out).unwrap();
        let data_size =
            u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]) as f64;
        let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]) as f64;
        let secs = data_size / 2.0 / rate;
        // RMS over the PCM16 payload.
        let pcm = &bytes[44..];
        let mut sumsq = 0f64;
        let mut n = 0u64;
        let mut i = 0;
        let mut nonzero = 0u64;
        while i + 1 < pcm.len() {
            let s = i16::from_le_bytes([pcm[i], pcm[i + 1]]) as f64 / 32768.0;
            sumsq += s * s;
            if s != 0.0 {
                nonzero += 1;
            }
            n += 1;
            i += 2;
        }
        let rms = (sumsq / n as f64).sqrt();
        eprintln!(
            "SMOKE: path={out} rate={rate} dur={secs:.3}s samples={n} nonzero={nonzero} rms={rms:.5}"
        );
        assert!((2.5..4.0).contains(&secs), "duration {secs:.3}s not ~3s");
        assert!(nonzero > 0, "capture was all zeros — audio did not route");
    }
}
