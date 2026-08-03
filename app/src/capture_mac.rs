//! Native Core Audio process-tap system capture — the macOS twin of Windows'
//! WASAPI loopback (`capture_win.rs`) and Linux's `parecord <sink>.monitor`.
//! The app owns system capture; the engine still only ever does mic capture, so
//! nothing here touches the RPC protocol.
//!
//! A `CATapDescription` global tap (stereo mixdown of every process, excluding
//! none) becomes an `AudioObjectID` via `AudioHardwareCreateProcessTap`; that
//! tap is then wrapped in a *private* aggregate device whose only member is the
//! tap itself, and an IOProc on that aggregate hands us the system mix. No sub-
//! device is listed: the tap is its own clock, and naming the current output as
//! a sub-device would drag the user's real (often Bluetooth) endpoint into an
//! aggregate for no gain. That is the whole point of this path over BlackHole —
//! it captures whatever is playing wherever it is playing, with no routing.
//!
//! macOS 14.2+ only. `native_tap_available()` is a runtime class lookup, and
//! the caller falls back to the loopback-driver path when it says no.
//!
//! TCC: the first `AudioDeviceStart` on a tap-bearing aggregate raises the
//! "System Audio Recording" prompt (kTCCServiceAudioCapture, distinct from the
//! microphone grant) and *blocks inside the call* until it is answered — so
//! `start()` can take minutes on a first run, and callers must not hold an
//! event loop while it does. There is no API to query or pre-request the grant;
//! a denial yields silence rather than an error, so the capture cannot detect
//! it here. Worse, when the *responsible* process has no
//! `NSAudioCaptureUsageDescription` (a terminal, say) tccd refuses without ever
//! prompting — hence the key in the bundle's Info.plist.
#![cfg(target_os = "macos")]

use std::ffi::c_void;
use std::fs::File;
use std::io::{self, BufWriter, Seek, SeekFrom, Write};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use objc2::rc::Retained;
use objc2::runtime::{AnyClass, AnyObject};
use objc2::AnyThread;
use objc2_core_audio::{
    kAudioAggregateDeviceIsPrivateKey, kAudioAggregateDeviceIsStackedKey,
    kAudioAggregateDeviceNameKey, kAudioAggregateDeviceTapAutoStartKey,
    kAudioAggregateDeviceTapListKey, kAudioAggregateDeviceUIDKey,
    kAudioObjectPropertyElementMain, kAudioObjectPropertyScopeGlobal,
    kAudioSubTapDriftCompensationKey, kAudioSubTapUIDKey, kAudioTapPropertyFormat,
    AudioDeviceCreateIOProcID, AudioDeviceDestroyIOProcID, AudioDeviceStart, AudioDeviceStop,
    AudioHardwareCreateAggregateDevice, AudioHardwareCreateProcessTap,
    AudioHardwareDestroyAggregateDevice, AudioHardwareDestroyProcessTap, AudioObjectGetPropertyData,
    AudioObjectPropertyAddress, CATapDescription,
};
use objc2_core_audio_types::{AudioBufferList, AudioStreamBasicDescription, AudioTimeStamp};
use objc2_core_foundation::CFDictionary;
use objc2_foundation::{NSArray, NSDictionary, NSNumber, NSString, NSUUID};

/// How often the writer thread moves the IOProc's samples into the WAV. Short
/// enough that `stop()` finalizes promptly, long enough not to spin.
const DRAIN: Duration = Duration::from_millis(20);
/// Cap on the handoff buffer, in mono samples. ~10 s at 48 kHz: if the writer
/// thread ever stalls that long the machine has bigger problems, and dropping
/// is the only option that keeps the realtime IOProc from growing without bound.
const MAX_QUEUED: usize = 480_000;
/// Every Core Audio tap hands back deinterleaved or interleaved **float32**;
/// anything else means the ASBD is not what this module was written against.
const FLOAT_BYTES: usize = 4;

/// What to tell the user when the tap produced nothing because the grant is
/// missing. Surfaced by the caller, not by this module — a denial is silent.
pub const TCC_HINT: &str =
    "System Settings > Privacy & Security > Screen & System Audio Recording";

/// Is the native process-tap API present? macOS 14.2 introduced
/// `CATapDescription`; on anything older the class is simply not registered,
/// which is a cheaper and more honest probe than parsing the OS version.
pub fn native_tap_available() -> bool {
    AnyClass::get(c"CATapDescription").is_some()
}

/// A live system-audio capture. The writer thread owns the Core Audio stack and
/// the WAV; this handle only flips the stop flag and joins — same contract as
/// `capture_win::Loopback`.
pub struct SystemTap {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    wav: String,
}

impl SystemTap {
    /// Begin capturing the whole system mix into `wav`. Blocks only until the
    /// worker reports that the tap, the aggregate device and the IOProc all came
    /// up (a few ms), so a creation failure surfaces here as an `io::Error`
    /// rather than a silently dead capture.
    pub fn start(wav: &str) -> io::Result<SystemTap> {
        if !native_tap_available() {
            return Err(io::Error::other("Core Audio process taps need macOS 14.2 or newer"));
        }
        let stop = Arc::new(AtomicBool::new(false));
        let errored = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel::<Result<(), String>>();
        let wav_path = wav.to_string();
        let (t_stop, t_err, t_wav) = (stop.clone(), errored.clone(), wav_path.clone());

        let thread = std::thread::Builder::new()
            .name("catap-capture".into())
            .spawn(move || capture_thread(t_wav, t_stop, t_err, tx))
            .map_err(io::Error::other)?;

        match rx.recv() {
            Ok(Ok(())) => Ok(SystemTap { thread: Some(thread), stop, errored, wav: wav_path }),
            Ok(Err(e)) => {
                let _ = thread.join();
                Err(io::Error::other(e))
            }
            Err(_) => {
                let _ = thread.join();
                Err(io::Error::other("tap thread exited during setup"))
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

    /// Did the capture die on its own? Mirrors the Linux `parecord`-child poll.
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

/// Handoff between the realtime IOProc and the writer thread. The IOProc only
/// ever locks to append already-downmixed PCM16, so the critical section is a
/// memcpy — no file I/O and no allocation beyond an amortized `Vec` growth.
struct Shared {
    pcm: Mutex<Vec<i16>>,
    dropped: AtomicBool,
}

/// The worker: build the tap, run it, drain into the WAV, tear everything down.
fn capture_thread(
    wav: String,
    stop: Arc<AtomicBool>,
    errored: Arc<AtomicBool>,
    tx: mpsc::Sender<Result<(), String>>,
) {
    let shared = Arc::new(Shared { pcm: Mutex::new(Vec::new()), dropped: AtomicBool::new(false) });
    let mut tap = match Tap::start(shared.clone()) {
        Ok(t) => t,
        Err(e) => {
            let _ = tx.send(Err(e));
            return;
        }
    };
    let mut writer = match WavWriter::create(&wav, tap.rate) {
        Ok(w) => w,
        Err(e) => {
            tap.teardown();
            let _ = tx.send(Err(e.to_string()));
            return;
        }
    };
    if tx.send(Ok(())).is_err() {
        tap.teardown();
        return;
    }

    let mut failed = false;
    while !stop.load(Ordering::SeqCst) {
        std::thread::sleep(DRAIN);
        if !drain_into(&shared, &mut writer) {
            failed = true;
            break;
        }
    }
    // Stop the IOProc before the final drain so no sample lands after it.
    tap.teardown();
    if !failed {
        failed = !drain_into(&shared, &mut writer);
    }
    if shared.dropped.load(Ordering::SeqCst) {
        tracing::warn!("catap: writer fell behind, samples dropped");
    }
    if let Err(e) = writer.finalize() {
        tracing::error!("catap wav finalize failed: {e}");
        failed = true;
    }
    if failed {
        errored.store(true, Ordering::SeqCst);
    }
}

/// Move everything queued by the IOProc into the WAV. Returns false on a write
/// error (the capture is then reported as died, like a `parecord` that exited).
fn drain_into(shared: &Shared, writer: &mut WavWriter) -> bool {
    let batch = {
        let mut q = shared.pcm.lock().unwrap_or_else(|e| e.into_inner());
        if q.is_empty() {
            return true;
        }
        std::mem::take(&mut *q)
    };
    if let Err(e) = writer.write_samples(&batch) {
        tracing::error!("catap wav write failed: {e}");
        return false;
    }
    true
}

// --- Core Audio plumbing -------------------------------------------------

/// The live Core Audio objects, owned start-to-finish by the worker thread.
struct Tap {
    tap_id: u32,
    agg_id: u32,
    proc_id: IOProcId,
    client: *mut c_void, // the leaked Arc<Shared> the IOProc reads through
    running: bool,
    rate: u32,
}

type IOProcId = Option<
    unsafe extern "C-unwind" fn(
        u32,
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
        NonNull<AudioBufferList>,
        NonNull<AudioTimeStamp>,
        *mut c_void,
    ) -> i32,
>;

impl Tap {
    fn start(shared: Arc<Shared>) -> Result<Tap, String> {
        unsafe {
            // Empty exclude list = the entire system mix, stereo.
            let empty: Retained<NSArray<NSNumber>> = NSArray::new();
            let desc = CATapDescription::initStereoGlobalTapButExcludeProcesses(
                CATapDescription::alloc(),
                &empty,
            );
            // The aggregate references the tap by *this* UUID string, so it has
            // to be pinned before the tap object is created.
            let tap_uuid = NSUUID::new();
            desc.setUUID(&tap_uuid);

            let mut tap_id: u32 = 0;
            let st = AudioHardwareCreateProcessTap(Some(&desc), &mut tap_id);
            if st != 0 || tap_id == 0 {
                return Err(format!("AudioHardwareCreateProcessTap failed ({st})"));
            }
            let mut tap = Tap {
                tap_id,
                agg_id: 0,
                proc_id: None,
                client: std::ptr::null_mut(),
                running: false,
                rate: 48_000,
            };

            let asbd = match tap_format(tap_id) {
                Ok(a) => a,
                Err(e) => {
                    tap.teardown();
                    return Err(e);
                }
            };
            if asbd.mBitsPerChannel != 32 || asbd.mSampleRate <= 0.0 {
                tap.teardown();
                return Err(format!(
                    "unexpected tap format: {} Hz, {} bits",
                    asbd.mSampleRate, asbd.mBitsPerChannel
                ));
            }
            tap.rate = asbd.mSampleRate as u32;

            let dict = aggregate_dict(&tap_uuid.UUIDString());
            let cf: &CFDictionary = &*(Retained::as_ptr(&dict) as *const CFDictionary);
            let mut agg_id: u32 = 0;
            let st = AudioHardwareCreateAggregateDevice(cf, NonNull::from(&mut agg_id));
            if st != 0 || agg_id == 0 {
                tap.teardown();
                return Err(format!("AudioHardwareCreateAggregateDevice failed ({st})"));
            }
            tap.agg_id = agg_id;

            // The IOProc reads the queue through a leaked Arc; it is reclaimed
            // in teardown(), strictly after AudioDeviceDestroyIOProcID, so the
            // realtime thread can never see a freed pointer.
            tap.client = Arc::into_raw(shared) as *mut c_void;
            let mut proc_id: IOProcId = None;
            let st = AudioDeviceCreateIOProcID(
                agg_id,
                Some(io_proc),
                tap.client,
                NonNull::from(&mut proc_id),
            );
            if st != 0 || proc_id.is_none() {
                tap.teardown();
                return Err(format!("AudioDeviceCreateIOProcID failed ({st})"));
            }
            tap.proc_id = proc_id;

            // First start on a tap-bearing aggregate is what raises the TCC
            // prompt; it returns noErr either way, and a denial shows up as
            // silence in the WAV.
            let st = AudioDeviceStart(agg_id, proc_id);
            if st != 0 {
                tap.teardown();
                return Err(format!("AudioDeviceStart failed ({st}) — check {TCC_HINT}"));
            }
            tap.running = true;
            Ok(tap)
        }
    }

    /// Idempotent: `start` uses it as its own error unwind, and the worker calls
    /// it once more at the end.
    fn teardown(&mut self) {
        unsafe {
            if self.running {
                let _ = AudioDeviceStop(self.agg_id, self.proc_id);
                self.running = false;
            }
            if let Some(p) = self.proc_id.take() {
                let _ = AudioDeviceDestroyIOProcID(self.agg_id, Some(p));
            }
            if !self.client.is_null() {
                drop(Arc::from_raw(self.client as *const Shared));
                self.client = std::ptr::null_mut();
            }
            if self.agg_id != 0 {
                let _ = AudioHardwareDestroyAggregateDevice(self.agg_id);
                self.agg_id = 0;
            }
            if self.tap_id != 0 {
                let _ = AudioHardwareDestroyProcessTap(self.tap_id);
                self.tap_id = 0;
            }
        }
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        self.teardown();
    }
}

/// Read the tap's stream format (`kAudioTapPropertyFormat`) — the sample rate
/// the WAV header has to carry, since nothing here resamples.
unsafe fn tap_format(tap_id: u32) -> Result<AudioStreamBasicDescription, String> {
    let addr = AudioObjectPropertyAddress {
        mSelector: kAudioTapPropertyFormat,
        mScope: kAudioObjectPropertyScopeGlobal,
        mElement: kAudioObjectPropertyElementMain,
    };
    let mut asbd: AudioStreamBasicDescription = std::mem::zeroed();
    let mut size = std::mem::size_of::<AudioStreamBasicDescription>() as u32;
    let st = AudioObjectGetPropertyData(
        tap_id,
        NonNull::from(&addr),
        0,
        std::ptr::null(),
        NonNull::from(&mut size),
        NonNull::new_unchecked(&mut asbd as *mut _ as *mut c_void),
    );
    if st != 0 {
        return Err(format!("reading the tap format failed ({st})"));
    }
    Ok(asbd)
}

/// The aggregate-device description. Private (never shown in Audio MIDI Setup
/// or the output menu), unstacked, auto-starting, and containing nothing but
/// the tap — see the module header on why no sub-device is listed.
fn aggregate_dict(tap_uid: &NSString) -> Retained<NSDictionary<NSString, AnyObject>> {
    let yes = NSNumber::new_bool(true);
    let no = NSNumber::new_bool(false);
    let sub_tap: Retained<NSDictionary<NSString, AnyObject>> = NSDictionary::from_slices(
        &[&*key(kAudioSubTapUIDKey), &*key(kAudioSubTapDriftCompensationKey)],
        &[&**tap_uid as &AnyObject, &**yes],
    );
    let tap_list: Retained<NSArray<NSDictionary<NSString, AnyObject>>> =
        NSArray::from_slice(&[&*sub_tap]);
    let name = NSString::from_str("Syrinx System Tap");
    let uid = NSUUID::new().UUIDString();
    NSDictionary::from_slices(
        &[
            &*key(kAudioAggregateDeviceNameKey),
            &*key(kAudioAggregateDeviceUIDKey),
            &*key(kAudioAggregateDeviceIsPrivateKey),
            &*key(kAudioAggregateDeviceIsStackedKey),
            &*key(kAudioAggregateDeviceTapAutoStartKey),
            &*key(kAudioAggregateDeviceTapListKey),
        ],
        &[
            &*name as &AnyObject,
            &*uid,
            &**yes,
            &**no,
            &**yes,
            &**tap_list,
        ],
    )
}

/// The framework's dictionary keys are C strings; the dictionary wants NSString.
fn key(k: &std::ffi::CStr) -> Retained<NSString> {
    NSString::from_str(k.to_str().unwrap_or_default())
}

/// The realtime callback. Downmixes the tap's float32 input to mono PCM16 and
/// appends it to the handoff queue — no allocation beyond the queue's amortized
/// growth, no file I/O, no blocking call other than the uncontended lock.
unsafe extern "C-unwind" fn io_proc(
    _device: u32,
    _now: NonNull<AudioTimeStamp>,
    input: NonNull<AudioBufferList>,
    _in_time: NonNull<AudioTimeStamp>,
    _output: NonNull<AudioBufferList>,
    _out_time: NonNull<AudioTimeStamp>,
    client: *mut c_void,
) -> i32 {
    if client.is_null() {
        return 0;
    }
    let shared = &*(client as *const Shared);
    let list = input.as_ref();
    let n = list.mNumberBuffers as usize;
    if n == 0 {
        return 0;
    }
    let bufs = std::slice::from_raw_parts(list.mBuffers.as_ptr(), n);

    // Frames per cycle: identical across buffers, so the first non-empty one
    // decides. Interleaved taps ship one buffer of N channels, deinterleaved
    // ones ship N buffers of 1 channel; both shapes are handled below.
    let mut frames = usize::MAX;
    let mut channels = 0usize;
    for b in bufs {
        let ch = b.mNumberChannels.max(1) as usize;
        let f = b.mDataByteSize as usize / (FLOAT_BYTES * ch);
        if b.mData.is_null() || f == 0 {
            continue;
        }
        frames = frames.min(f);
        channels += ch;
    }
    if channels == 0 || frames == 0 || frames == usize::MAX {
        return 0;
    }

    let mut q = match shared.pcm.try_lock() {
        Ok(q) => q,
        // The writer thread holds it for a memcpy only; missing one cycle is
        // still better than blocking the HAL's realtime thread.
        Err(_) => {
            shared.dropped.store(true, Ordering::Relaxed);
            return 0;
        }
    };
    if q.len() + frames > MAX_QUEUED {
        shared.dropped.store(true, Ordering::Relaxed);
        return 0;
    }
    let base = q.len();
    q.resize(base + frames, 0);
    let scale = 1.0 / channels as f32;
    for b in bufs {
        let ch = b.mNumberChannels.max(1) as usize;
        if b.mData.is_null() || b.mDataByteSize as usize / (FLOAT_BYTES * ch) < frames {
            continue;
        }
        let samples = std::slice::from_raw_parts(b.mData as *const f32, frames * ch);
        for f in 0..frames {
            let mut acc = 0f32;
            for c in 0..ch {
                acc += samples[f * ch + c];
            }
            let slot = &mut q[base + f];
            *slot = slot.saturating_add(f32_to_i16(acc * scale));
        }
    }
    0
}

/// Clamp a float sample to i16.
pub(crate) fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Minimal streaming mono PCM16 WAV writer with a patch-on-finalize header —
/// byte-for-byte the same container `capture_win` produces.
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

    fn write_samples(&mut self, s: &[i16]) -> io::Result<()> {
        let mut bytes = Vec::with_capacity(s.len() * 2);
        for v in s {
            bytes.extend_from_slice(&v.to_le_bytes());
        }
        self.file.write_all(&bytes)?;
        self.data_bytes = self.data_bytes.saturating_add(bytes.len() as u32);
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
    fn wav_header_is_well_formed_after_finalize() {
        let path = std::env::temp_dir().join("syrinx-capmac-headertest.wav");
        let p = path.to_string_lossy().to_string();
        let mut w = WavWriter::create(&p, 44_100).unwrap();
        let samples: Vec<i16> = (0..150).map(|i| (i as i16) * 100).collect();
        w.write_samples(&samples).unwrap();
        w.finalize().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        assert_eq!(&bytes[36..40], b"data");
        let rate = u32::from_le_bytes([bytes[24], bytes[25], bytes[26], bytes[27]]);
        assert_eq!(rate, 44_100, "the header carries the tap's own rate");
        let data_size = u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]);
        assert_eq!(data_size, 150 * 2);
        let riff = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        assert_eq!(riff, 36 + 150 * 2);
        assert_eq!(bytes.len() as u32, 44 + 150 * 2);
        let _ = std::fs::remove_file(&path);
    }

    /// The API is a runtime lookup, so on this repo's supported macOS floor it
    /// must resolve; a `false` here on 14.2+ means the crate stopped linking
    /// CoreAudio and every mac would silently drop to the BlackHole path.
    #[test]
    fn native_tap_is_available_on_a_modern_mac() {
        let major: u32 = std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| String::from_utf8(o.stdout).ok())
            .and_then(|s| s.trim().split('.').next()?.parse().ok())
            .unwrap_or(0);
        if major >= 15 {
            assert!(native_tap_available());
        }
    }
}
