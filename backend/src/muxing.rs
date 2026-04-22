use std::{
    collections::HashMap,
    env,
    path::{Path, PathBuf},
    sync::{
        Arc, Weak,
        atomic::{AtomicU32, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use tokio::{
    fs,
    process::{Child, Command},
    sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore},
    time::sleep,
};
use tracing::{info, warn};
use uuid::Uuid;

use crate::persistence::Persistence;
use crate::protocol::{AudioStream, MediaItem, PlaybackMode};
use crate::scan_gate::{ScanGate, ScanGateGuard};

const DEFAULT_HLS_SUBDIR: &str = "syncplay-hls";
const DEFAULT_MAX_CONCURRENT: usize = 2;
const DEFAULT_IDLE_SECS: u64 = 60;
/// How often readiness / segment-wait loops re-check the filesystem. Keep this
/// tight — the dominant tail latency at startup is "ffmpeg wrote the file a
/// few ms ago but we're still sleeping before our next `exists` check".
const READINESS_POLL_MS: u64 = 20;
const READINESS_DEADLINE_SECS: u64 = 15;
/// Cadence for the background idle-eviction task. Eviction used to run on
/// every `serve` call; moving it off the hot path keeps segment requests from
/// contending on the sessions map just to confirm nothing is stale.
const EVICTION_INTERVAL_SECS: u64 = 5;
const MASTER_FILENAME: &str = "master.m3u8";
/// Target segment duration in seconds. Smaller = lower time-to-first-frame at
/// the cost of more files / more HTTP requests. Two seconds is the sweet spot
/// for a single-viewer-on-a-LAN use case: the readiness gate fires after
/// roughly one segment of source has been encoded, so this directly bounds
/// startup latency for the transcode tier.
const HLS_TARGET_SEGMENT_SECONDS: u32 = 2;
/// Segments shorter than this are folded into the next boundary when building
/// a copy-mode plan from a dense keyframe list. Some sources (scene changes,
/// fades) pack keyframes tightly enough that a 1:1 segment-per-keyframe
/// mapping would produce sub-second EXTINFs, which waste manifest bytes and
/// cause hls.js to churn through a segment per frame during playback.
const MIN_SEGMENT_SECONDS: f64 = 1.0;
/// How far ahead of the current encoder frontier we're willing to wait a
/// segment request out before deciding to respawn ffmpeg at the seek target.
/// Expressed in seconds of video, not seconds of wall-clock: at 2 s segments
/// this is about 30 segments of tolerance, which covers a hls.js tab
/// buffering five or six segments ahead of playback without respawning for
/// each one.
const CATCHUP_WINDOW_SECS: f64 = 60.0;
/// Filename of the frozen init segment referenced by the synthesized variant
/// playlist. `init_0.mp4` is what ffmpeg writes; we rename it to this stable
/// name after the first session readiness so subsequent encoder respawns (on
/// seek) don't overwrite it while a client still has the MAP request open.
const INIT_SEGMENT_FROZEN: &str = "init_0.lock.mp4";
/// Raw ffmpeg init segment filename before we freeze it.
const INIT_SEGMENT_RAW: &str = "init_0.mp4";

/// Per-session layout of segment boundaries.
///
/// The video is carved into N segments; boundary `i` is the start PTS
/// (seconds) of segment `i`, and the segment's duration is
/// `boundaries[i+1] - boundaries[i]` (or `total_duration - boundaries[i]`
/// for the final segment). Segment count is always `boundaries.len()`.
///
/// Construction rules are mode-specific:
///
/// * **Transcode (`HlsFullTranscode`):** a fixed 2 s grid `[0.0, 2.0, 4.0,
///   ...]`. Forced keyframes on the encoder (`-force_key_frames
///   expr:gte(t,n_forced*2)`) guarantee real segment starts land on these
///   boundaries, so we can advertise them up-front as a VOD playlist with
///   `#EXT-X-ENDLIST` and know ffmpeg will honor them on respawn.
///
/// * **Copy (`HlsRemux`, `HlsAudioTranscode`):** boundaries derived from
///   the source keyframe index. `-c:v copy` cannot emit a keyframe that
///   isn't already present in the source, so segment starts MUST be a
///   subset of the source keyframe positions. We coalesce keyframes that
///   are closer than `MIN_SEGMENT_SECONDS` apart, accepting the longer
///   segment rather than shipping a spray of tiny ones.
///
/// Whenever the keyframe list is missing or empty (pre-reindex, probe
/// failure, audio-only), callers fall back to a single-segment plan
/// `[0.0]`, which degrades gracefully to the pre-deep-seek linear-only
/// playback behavior.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct SegmentPlan {
    boundaries: Vec<f64>,
    total_duration: f64,
    target_duration_secs: u32,
}

impl SegmentPlan {
    /// Single-segment plan covering `[0, total_duration)`. The fallback for
    /// titles where we don't know any better — keyframe index missing,
    /// audio-only, probe failure — and for sessions whose duration ffprobe
    /// couldn't extract at all (we pass 0.0 in that case and the HLS
    /// session behaves as today's linear encoder).
    fn single(total_duration: f64) -> Self {
        let total_duration = total_duration.max(0.0);
        Self {
            boundaries: vec![0.0],
            total_duration,
            target_duration_secs: total_duration.ceil().max(1.0) as u32,
        }
    }

    /// Fixed-grid plan at `HLS_TARGET_SEGMENT_SECONDS` granularity. Used for
    /// transcode sessions where forced keyframes pin real segments to this
    /// exact cadence.
    fn fixed_grid(total_duration: f64) -> Self {
        let total_duration = total_duration.max(0.0);
        if total_duration <= f64::from(HLS_TARGET_SEGMENT_SECONDS) {
            return Self::single(total_duration);
        }
        let step = f64::from(HLS_TARGET_SEGMENT_SECONDS);
        let mut boundaries = Vec::new();
        let mut t = 0.0;
        while t + step <= total_duration {
            boundaries.push(t);
            t += step;
        }
        // Absorb the trailing sub-segment into the last full one if it
        // would be shorter than MIN_SEGMENT_SECONDS — otherwise ship the
        // short final segment (hls.js handles that fine at end-of-playlist).
        if boundaries.is_empty() {
            boundaries.push(0.0);
        }
        if total_duration - boundaries.last().copied().unwrap_or(0.0) > step
            && total_duration - t >= MIN_SEGMENT_SECONDS
        {
            boundaries.push(t);
        }
        Self {
            boundaries,
            total_duration,
            target_duration_secs: HLS_TARGET_SEGMENT_SECONDS,
        }
    }

    /// Build a plan from a sorted keyframe PTS list. Coalesces adjacent
    /// keyframes closer than `MIN_SEGMENT_SECONDS` by dropping the later
    /// one; falls back to `single(total_duration)` when the list is empty
    /// or the first keyframe starts after 0 (the source doesn't have a
    /// keyframe at t=0, which is unusual — most containers do — and
    /// segment 0 must start at 0 or hls.js won't play it).
    fn from_keyframes(total_duration: f64, keyframes: &[f64]) -> Self {
        let total_duration = total_duration.max(0.0);
        if total_duration <= MIN_SEGMENT_SECONDS || keyframes.is_empty() {
            return Self::single(total_duration);
        }
        // Every keyframe-indexed title we've seen has a keyframe at the
        // stream start; if the first one is noticeably inside the stream
        // we'd be advertising a segment range that can't cover
        // `[0, keyframes[0])` — fall back rather than ship an invalid
        // playlist. The 0.1 s slop absorbs MPEG-TS containers whose PTS
        // origin drifts a few tens of ms.
        if keyframes[0] > 0.1 {
            return Self::single(total_duration);
        }

        let mut boundaries: Vec<f64> = Vec::with_capacity(keyframes.len());
        boundaries.push(0.0);
        for &kf in keyframes.iter().skip(1) {
            if kf <= 0.0 || kf >= total_duration {
                continue;
            }
            let prev = *boundaries.last().expect("seeded with 0.0");
            if kf - prev >= MIN_SEGMENT_SECONDS {
                boundaries.push(kf);
            }
        }

        let target = boundaries
            .windows(2)
            .map(|w| w[1] - w[0])
            .chain(std::iter::once(
                total_duration - boundaries.last().copied().unwrap_or(0.0),
            ))
            .fold(0.0_f64, f64::max)
            .ceil()
            .max(1.0) as u32;

        Self {
            boundaries,
            total_duration,
            target_duration_secs: target,
        }
    }

    /// Highest-level entry point: pick the right constructor for a given
    /// playback mode. `keyframes` is only consulted for copy modes; pass
    /// an empty slice when it's not available yet.
    pub(crate) fn for_session(mode: PlaybackMode, total_duration: f64, keyframes: &[f64]) -> Self {
        match mode {
            PlaybackMode::HlsFullTranscode => Self::fixed_grid(total_duration),
            PlaybackMode::HlsRemux | PlaybackMode::HlsAudioTranscode => {
                Self::from_keyframes(total_duration, keyframes)
            }
            // Direct / Unsupported never reach the HLS session; a trivial
            // plan keeps callers simple.
            _ => Self::single(total_duration),
        }
    }

    fn segment_count(&self) -> u32 {
        self.boundaries.len() as u32
    }

    fn start_time_of(&self, index: u32) -> f64 {
        self.boundaries
            .get(index as usize)
            .copied()
            .unwrap_or(self.total_duration)
    }

    fn duration_of(&self, index: u32) -> f64 {
        let start = self.start_time_of(index);
        let next = self
            .boundaries
            .get(index as usize + 1)
            .copied()
            .unwrap_or(self.total_duration);
        (next - start).max(0.0)
    }

    /// Inverse of `start_time_of`: which segment covers the given PTS?
    /// Kept for symmetry with `start_time_of` — the respawn path
    /// currently receives a pre-computed segment index from the request
    /// handler (so this isn't on its hot path), but the inverse lookup
    /// is needed the moment we add a "seek to timestamp T" API.
    #[allow(dead_code)]
    fn segment_index_for_time(&self, t: f64) -> u32 {
        if self.boundaries.is_empty() {
            return 0;
        }
        // boundaries are ascending; partition_point gives first index whose
        // start > t. The segment containing t is the one before that.
        let upper = self.boundaries.partition_point(|b| *b <= t);
        upper.saturating_sub(1) as u32
    }
}

/// ffmpeg segment-filename pattern. `%v` is the variant id (v:0 for video,
/// a:0+ for audio renditions); `%05d` is the zero-padded sequence number.
/// Kept as a single source of truth so the synthesized playlist and the
/// `-hls_segment_filename` arg can't drift apart.
const SEGMENT_FILENAME_PATTERN: &str = "seg_%v_%05d.m4s";

/// Builds the concrete on-disk filename for segment `index` of the video
/// variant (v:0). This is both what ffmpeg writes and what the synthesized
/// variant playlist advertises to the client.
pub(crate) fn video_segment_filename(index: u32) -> String {
    format!("seg_0_{index:05}.m4s")
}

/// Parses a video segment filename back into a segment index. Returns
/// `None` for audio-variant segments (`seg_1_…`, `seg_eng_…`, etc.) — those
/// follow the encoder without independent scheduling, so out-of-range
/// request handling is video-only. Returns `None` for any other pattern.
pub(crate) fn parse_video_segment_filename(filename: &str) -> Option<u32> {
    let rest = filename.strip_prefix("seg_0_")?;
    let number = rest.strip_suffix(".m4s")?;
    // Accept any number of digits (the pattern emits 5, but a renamed
    // legacy file with fewer / more is still parseable).
    if number.is_empty() || !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    number.parse().ok()
}

/// Synthesize a VOD variant playlist from a `SegmentPlan`.
///
/// This is the playlist hls.js actually sees on the wire — we never serve
/// ffmpeg's own `stream_*.m3u8` for the video variant. Synthesizing it
/// ourselves has three properties that matter:
///
/// 1. **Fully VOD from session start.** `#EXT-X-ENDLIST` and
///    `#EXT-X-PLAYLIST-TYPE:VOD` are present immediately, so hls.js
///    commits to the final duration and offers seekability anywhere in
///    the range before the encoder has even started. Forward-seeking to
///    a segment that doesn't yet exist is what the kill-restart respawn
///    path handles.
///
/// 2. **Stable URIs across respawns.** Because the playlist is a function
///    of the plan (not of ffmpeg's current state), killing ffmpeg and
///    respawning it at a different offset doesn't change what the client
///    thinks the playlist looks like. The player keeps reading the same
///    list of URIs and the backend transparently reroutes segment
///    requests to whichever encoder is active.
///
/// 3. **Frozen init segment.** The `#EXT-X-MAP` points at
///    `INIT_SEGMENT_FROZEN`, which we atomically rename from ffmpeg's raw
///    `init_0.mp4` after the first readiness. Subsequent respawns overwrite
///    a fresh `init_0.mp4` that nobody reads, avoiding any partial-write
///    race with the client's MAP fetch.
pub(crate) fn build_variant_playlist(plan: &SegmentPlan) -> String {
    let mut out = String::new();
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:7\n");
    out.push_str(&format!(
        "#EXT-X-TARGETDURATION:{}\n",
        plan.target_duration_secs
    ));
    out.push_str("#EXT-X-MEDIA-SEQUENCE:0\n");
    out.push_str("#EXT-X-PLAYLIST-TYPE:VOD\n");
    out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");
    out.push_str(&format!("#EXT-X-MAP:URI=\"{INIT_SEGMENT_FROZEN}\"\n"));
    for i in 0..plan.segment_count() {
        // EXTINF requires 3-decimal precision for reliable playback in
        // hls.js — integer EXTINFs are accepted but confuse seek math
        // when the last segment is shorter than target.
        out.push_str(&format!("#EXTINF:{:.3},\n", plan.duration_of(i)));
        out.push_str(&video_segment_filename(i));
        out.push('\n');
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

#[derive(Debug)]
pub enum HlsError {
    UnsupportedMode,
    SpawnFailed(String),
    Io(std::io::Error),
    NotReady,
    NotFound,
    InvalidFilename,
}

impl std::fmt::Display for HlsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedMode => f.write_str("media is not eligible for HLS streaming"),
            Self::SpawnFailed(message) => write!(f, "ffmpeg failed: {message}"),
            Self::Io(error) => write!(f, "{error}"),
            Self::NotReady => f.write_str("HLS session did not become ready in time"),
            Self::NotFound => f.write_str("HLS asset not found"),
            Self::InvalidFilename => f.write_str("invalid HLS filename"),
        }
    }
}

impl std::error::Error for HlsError {}

impl From<std::io::Error> for HlsError {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

#[derive(Clone, Debug)]
pub struct HlsConfig {
    pub cache_dir: PathBuf,
    pub max_concurrent: usize,
    pub idle_secs: u64,
    pub ffmpeg_command: Option<PathBuf>,
    pub hw_accel: HwAccel,
    /// Optional override of the DRM render node used by VAAPI. `None` falls back
    /// to `/dev/dri/renderD128` at use site, which is the conventional default
    /// on single-GPU Linux boxes.
    pub vaapi_device: Option<PathBuf>,
}

impl Default for HlsConfig {
    fn default() -> Self {
        Self {
            cache_dir: default_cache_dir(),
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            idle_secs: DEFAULT_IDLE_SECS,
            ffmpeg_command: Some(PathBuf::from("ffmpeg")),
            hw_accel: HwAccel::None,
            vaapi_device: None,
        }
    }
}

impl HlsConfig {
    pub fn from_env(ffmpeg_command: Option<PathBuf>) -> Self {
        let cache_dir = env::var_os("SYNCPLAY_HLS_DIR")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty())
            .map(absolutize_path)
            .unwrap_or_else(default_cache_dir);
        let max_concurrent = env::var("SYNCPLAY_HLS_MAX_CONCURRENT")
            .ok()
            .and_then(|value| value.parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_MAX_CONCURRENT);
        let idle_secs = env::var("SYNCPLAY_HLS_IDLE_SECS")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(DEFAULT_IDLE_SECS);
        let hw_accel = match env::var("SYNCPLAY_HLS_HW_ACCEL") {
            Ok(raw) => match HwAccel::from_env_value(&raw) {
                Some(value) => value,
                None => {
                    warn!(
                        value = raw.as_str(),
                        "unknown SYNCPLAY_HLS_HW_ACCEL value; falling back to software encoding"
                    );
                    HwAccel::None
                }
            },
            Err(_) => HwAccel::None,
        };
        let vaapi_device = env::var_os("SYNCPLAY_HLS_VAAPI_DEVICE")
            .map(PathBuf::from)
            .filter(|path| !path.as_os_str().is_empty());

        Self {
            cache_dir,
            max_concurrent,
            idle_secs,
            ffmpeg_command,
            hw_accel,
            vaapi_device,
        }
    }
}

/// Selects which hardware encoder/decoder pair the full-transcode tier should
/// use. `None` (the default) keeps the existing software `libx264` path, which
/// runs anywhere ffmpeg does. Every other variant requires the matching driver
/// stack on the host (NVIDIA driver + CUDA runtime, Mesa/Intel media driver,
/// Intel VPL runtime, or macOS).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum HwAccel {
    #[default]
    None,
    /// NVIDIA: CUDA decode + `h264_nvenc` encode.
    Nvenc,
    /// Intel/AMD on Linux: VAAPI decode + `h264_vaapi` encode.
    Vaapi,
    /// Intel Quick Sync via libvpl: `qsv` decode + `h264_qsv` encode.
    Qsv,
    /// Apple silicon / Intel Macs: VideoToolbox decode + `h264_videotoolbox` encode.
    VideoToolbox,
}

impl HwAccel {
    /// Parse an env-var value. Returns `None` for unrecognised values so callers
    /// can warn and fall back rather than panic — operators may template the
    /// same env across machines that don't all have the requested accelerator.
    pub(crate) fn from_env_value(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "none" | "off" | "0" | "false" => Some(Self::None),
            "nvenc" | "cuda" | "nvidia" => Some(Self::Nvenc),
            "vaapi" => Some(Self::Vaapi),
            "qsv" | "quicksync" => Some(Self::Qsv),
            "videotoolbox" | "vt" => Some(Self::VideoToolbox),
            _ => None,
        }
    }
}

/// Default DRM render node used when `SYNCPLAY_HLS_VAAPI_DEVICE` is unset. Most
/// single-GPU Linux boxes expose exactly one render node here.
const DEFAULT_VAAPI_DEVICE: &str = "/dev/dri/renderD128";

/// Default HLS session cache lives in the system temp dir so it survives nothing
/// across reboots, doesn't pollute the project directory, and — critically — is
/// always an absolute path. ffmpeg interprets relative output paths against its
/// own CWD, so a relative cache_dir caused init segments to be written to nested,
/// non-existent paths.
fn default_cache_dir() -> PathBuf {
    env::temp_dir().join(DEFAULT_HLS_SUBDIR)
}

/// Resolve any relative path to an absolute one against the process CWD. We do
/// not canonicalize because the directory may not exist yet; we only need the
/// path to be absolute so ffmpeg's CWD doesn't affect resolution.
fn absolutize_path(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    match env::current_dir() {
        Ok(cwd) => cwd.join(path),
        Err(_) => path,
    }
}

#[derive(Clone, Copy, Debug)]
pub enum BrowserHint {
    Generic,
    Safari,
}

impl BrowserHint {
    pub fn from_user_agent(user_agent: Option<&str>) -> Self {
        match user_agent {
            Some(ua) if is_safari_user_agent(ua) => Self::Safari,
            _ => Self::Generic,
        }
    }
}

fn is_safari_user_agent(ua: &str) -> bool {
    // Safari UA contains "Safari" but not "Chrome" / "Chromium" / "Edg".
    let lower = ua.to_ascii_lowercase();
    lower.contains("safari")
        && !lower.contains("chrome")
        && !lower.contains("chromium")
        && !lower.contains("edg/")
        && !lower.contains("firefox")
}

/// HLS session compatibility bucket.
///
/// Most playback modes are browser-agnostic at the transcoder level, but
/// `HlsFullTranscode` has a Safari-specific fast path (`-c:v copy`) while
/// non-Safari clients need full video transcode. If both buckets shared one
/// session keyed only by media id, the first requester would pin behavior for
/// everyone else.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum SessionFlavor {
    Default,
    SafariPassthrough,
}

impl SessionFlavor {
    fn for_request(item: &MediaItem, browser: BrowserHint) -> Self {
        if matches!(item.playback_mode, PlaybackMode::HlsFullTranscode)
            && matches!(browser, BrowserHint::Safari)
        {
            Self::SafariPassthrough
        } else {
            Self::Default
        }
    }

    fn as_dir_token(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::SafariPassthrough => "safari-copy",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct SessionKey {
    media_id: Uuid,
    flavor: SessionFlavor,
}

impl SessionKey {
    fn for_request(item: &MediaItem, browser: BrowserHint) -> Self {
        Self {
            media_id: item.id,
            flavor: SessionFlavor::for_request(item, browser),
        }
    }

    fn dir_name(self) -> String {
        format!("{}-{}", self.media_id, self.flavor.as_dir_token())
    }
}

#[derive(Debug)]
struct StartupState {
    /// `true` once at least one ffmpeg has been spawned for this session.
    /// A respawn (deep-seek) happens while this is `true`, so the flag
    /// isn't a "don't spawn" gate by itself — callers serialize through
    /// the mutex and inspect `encoder` for the source of truth.
    started: bool,
}

/// Snapshot of the currently-running encoder. Updated on every spawn and
/// every respawn; `None` once the encoder exits or hasn't been spawned
/// yet.
#[derive(Debug)]
struct EncoderState {
    /// The `-start_number` value we passed ffmpeg. Every segment index in
    /// `[start_segment, highest_produced]` is either on disk or about to
    /// be, depending on whether the watcher has observed ffmpeg advertise
    /// it in the variant playlist.
    start_segment: u32,
    /// The `-ss` / `-output_ts_offset` value we passed ffmpeg, seconds.
    /// Currently unread — kept for debug visibility (the Debug impl
    /// prints it) and so future log lines don't have to re-derive it.
    #[allow(dead_code)]
    start_offset_secs: f64,
    /// Highest segment index ffmpeg has published in its variant playlist
    /// so far. Updated by the readiness watcher after each poll.
    highest_produced: AtomicU32,
}

#[derive(Debug)]
struct HlsSession {
    media_id: Uuid,
    dir: PathBuf,
    child: Mutex<Option<Child>>,
    /// Number of ffmpeg spawns attempted for this session (initial spawn +
    /// any deep-seek respawns). Primarily a test/observability hook.
    spawn_count: AtomicU32,
    last_access_ms: AtomicU64,
    in_flight: AtomicU32,
    ready: Notify,
    is_ready: AtomicU32,
    startup: Mutex<StartupState>,
    spawn_error: Mutex<Option<String>>,
    /// Wall-clock `Instant` of the ffmpeg `spawn()` call. Used solely for the
    /// "session became ready in N.Ns" log line — that number is the single
    /// best signal we have for whether the encoder is pacing realtime, since
    /// it's compared against `HLS_TARGET_SEGMENT_SECONDS`. Populated by
    /// `spawn_if_needed` immediately before returning, left as `None` only for
    /// sessions that never started (error path).
    spawned_at: Mutex<Option<Instant>>,
    /// Segment plan is computed once at session creation from probe data
    /// (duration + keyframes) and never changes for the session's lifetime.
    /// Both the synthesized variant playlist and the respawn logic consult
    /// this to map segment indices to start times and vice versa.
    plan: SegmentPlan,
    /// The currently-running encoder's range and frontier. Rewritten on
    /// each respawn. Wrapped in a sync `Mutex` because request handling
    /// crosses the `serve` path — keeping it `std::sync` avoids polluting
    /// that path with extra `await`s; all mutations are millisecond-scale.
    encoder: std::sync::Mutex<Option<EncoderState>>,
    /// Cached copy of the input path — the `MediaItem` isn't available on
    /// the respawn path (`serve` has a reference but the internal
    /// `respawn_at_segment` doesn't), so we stash the resolved path at
    /// session creation time.
    input_path: PathBuf,
    /// Same for browser hint — decides whether HlsFullTranscode should
    /// passthrough (Safari) or transcode (everyone else). Captured once at
    /// session spawn because the first request dictates the encoder plan.
    browser: BrowserHint,
    /// Whole `MediaItem` snapshot captured at session start; used by
    /// `build_ffmpeg_args` on both the initial spawn and every respawn to
    /// pick codec / stream map args. Note: this is intentionally a clone,
    /// not a reference — sessions outlive the `MediaItem` reference the
    /// caller passed into `serve`, since idle eviction can run well after
    /// the request that created the session returned.
    item: MediaItem,
    _permit: OwnedSemaphorePermit,
    /// Held for the entire session lifetime so that background probe /
    /// thumbnail work pauses while we're serving playback. Dropped with the
    /// session (via the Arc); see `crate::scan_gate`.
    _scan_gate_guard: ScanGateGuard,
}

impl HlsSession {
    fn touch(&self) {
        self.last_access_ms.store(now_ms(), Ordering::Relaxed);
    }

    fn last_access(&self) -> Instant {
        // Convert monotonic-ish u64 ms to relative Instant via current clock delta.
        let stored_ms = self.last_access_ms.load(Ordering::Relaxed);
        let now_ms_value = now_ms();
        let elapsed_ms = now_ms_value.saturating_sub(stored_ms);
        Instant::now()
            .checked_sub(Duration::from_millis(elapsed_ms))
            .unwrap_or_else(Instant::now)
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[derive(Debug)]
pub struct InFlightGuard {
    session: Arc<HlsSession>,
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.session.in_flight.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Payload returned to the HTTP handler. Either a file on disk (the
/// common case — segments, init, the master) or an inline body we
/// synthesized in memory (the video variant playlist `stream_0.m3u8`).
///
/// Keeping both paths in one type lets `serve` decide which one applies
/// without forcing the caller to branch on filename.
#[derive(Debug)]
pub enum HlsServeBody {
    File(PathBuf),
    Inline(Vec<u8>),
}

#[derive(Debug)]
pub struct HlsServeResult {
    pub body: HlsServeBody,
    pub content_type: &'static str,
    pub _guard: InFlightGuard,
}

#[derive(Clone, Debug)]
pub struct HlsSessionManager {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    sessions: Mutex<HashMap<SessionKey, Arc<HlsSession>>>,
    semaphore: Arc<Semaphore>,
    config: HlsConfig,
    scan_gate: ScanGate,
    /// Used to load the keyframe index when spawning a session. Optional
    /// for the test path (`HlsSessionManager::new`) — callers that skip
    /// persistence get the single-segment fallback plan, which is fine
    /// for all existing tests since they don't exercise deep-seek.
    persistence: Option<Persistence>,
}

impl HlsSessionManager {
    pub fn new(config: HlsConfig) -> Self {
        Self::with_scan_gate_and_persistence(config, ScanGate::new(), None)
    }

    /// Shared-ScanGate constructor preserved for legacy call sites (tests
    /// wiring a ScanGate without the persistence layer).
    pub fn with_scan_gate(config: HlsConfig, scan_gate: ScanGate) -> Self {
        Self::with_scan_gate_and_persistence(config, scan_gate, None)
    }

    /// Full constructor used in production. `persistence` lets the session
    /// layer pull the keyframe index for copy-mode titles; without it,
    /// every session falls back to a single-segment plan (the pre-deep-
    /// seek behavior).
    pub fn with_scan_gate_and_persistence(
        mut config: HlsConfig,
        scan_gate: ScanGate,
        persistence: Option<Persistence>,
    ) -> Self {
        // Force the cache dir to be absolute. ffmpeg uses its own CWD when it
        // resolves output paths; if a relative cache_dir leaks through, the
        // session directory ends up nested under the spawn CWD.
        config.cache_dir = absolutize_path(config.cache_dir);
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent));

        let inner = Arc::new(Inner {
            sessions: Mutex::new(HashMap::new()),
            semaphore,
            config,
            scan_gate,
            persistence,
        });

        // Background idle-eviction: a weak handle so the task exits once the
        // manager is dropped. Previously this ran inline on every `serve`
        // call, which took the sessions lock on every HTTP segment fetch.
        let weak = Arc::downgrade(&inner);
        tokio::spawn(async move {
            run_eviction_loop(weak).await;
        });

        Self { inner }
    }

    pub fn config(&self) -> &HlsConfig {
        &self.inner.config
    }

    /// Number of currently active HLS sessions.
    pub async fn session_count(&self) -> usize {
        self.inner.sessions.lock().await.len()
    }

    /// Total ffmpeg spawn count across all currently-active sessions.
    ///
    /// This includes respawns triggered by deep seeks and is mainly used by
    /// integration tests to verify respawn behavior.
    pub async fn total_spawn_count(&self) -> u32 {
        let sessions = self.inner.sessions.lock().await;
        sessions
            .values()
            .map(|session| session.spawn_count.load(Ordering::Acquire))
            .sum()
    }

    pub async fn shutdown(&self) {
        let mut sessions = self.inner.sessions.lock().await;
        let drained: Vec<_> = sessions.drain().collect();
        drop(sessions);

        for (_, session) in drained {
            kill_session(&session).await;
        }
    }

    /// Returns a ready HLS asset (master.m3u8 / variant playlist / segment).
    /// Spawns ffmpeg on demand, and — if a requested video segment falls
    /// outside the active encoder's advertised range — kills and respawns
    /// ffmpeg aligned to the requested offset so deep seeks land in a few
    /// seconds instead of the full transcode wait.
    ///
    /// Caller MUST hold the returned `HlsServeResult` while the response is
    /// being sent (its drop guard tracks in-flight requests for eviction).
    pub async fn serve(
        &self,
        item: &MediaItem,
        filename: &str,
        browser: BrowserHint,
    ) -> Result<HlsServeResult, HlsError> {
        if !item.playback_mode.is_hls() {
            return Err(HlsError::UnsupportedMode);
        }
        validate_filename(filename)?;

        let session = self.get_or_start(item, browser).await?;
        session.touch();

        // The video variant playlist is synthesized from `SegmentPlan`
        // (complete VOD with ENDLIST, stable URIs across respawns). We
        // return it directly from memory — ffmpeg's own `stream_0.m3u8`
        // on disk is kept purely for the readiness watcher to scrape the
        // frontier from. Note: `master.m3u8` is also synthesized, but we
        // already wrote it to disk in `spawn_if_needed`, so the normal
        // file path works for it.
        if filename == VIDEO_VARIANT_FILENAME {
            self.wait_for_ready(&session).await?;
            session.in_flight.fetch_add(1, Ordering::Relaxed);
            let body = build_variant_playlist(&session.plan).into_bytes();
            return Ok(HlsServeResult {
                body: HlsServeBody::Inline(body),
                content_type: content_type_for_filename(filename),
                _guard: InFlightGuard {
                    session: session.clone(),
                },
            });
        }

        // Deep-seek fast path: if the request names a specific video
        // segment and that segment is outside the current encoder's
        // catch-up window, respawn ffmpeg at that offset instead of
        // waiting. Without this, a seek to minute 45 would block until
        // ffmpeg linearly transcoded all 45 minutes first.
        if let Some(segment_index) = parse_video_segment_filename(filename) {
            // Reject impossible segment indices up-front. Without this check a
            // request like seg_0_99999 can trigger a useless respawn at EOF
            // and then time out waiting for a segment that can never exist.
            if segment_index >= session.plan.segment_count() {
                return Err(HlsError::NotFound);
            }
            let path = session.dir.join(filename);
            // Already on disk from an earlier spawn → serve directly.
            if fs::try_exists(&path).await.unwrap_or(false) {
                session.in_flight.fetch_add(1, Ordering::Relaxed);
                return Ok(HlsServeResult {
                    body: HlsServeBody::File(path),
                    content_type: content_type_for_filename(filename),
                    _guard: InFlightGuard {
                        session: session.clone(),
                    },
                });
            }
            // Not on disk yet — decide whether the current encoder will
            // ever produce this segment within the catch-up window, or we
            // need to respawn at the seek target.
            if self.should_respawn_for_segment(&session, segment_index) {
                info!(
                    session = %session.media_id,
                    segment = segment_index,
                    "respawning ffmpeg at seek target"
                );
                self.respawn_at_segment(&session, segment_index).await?;
            }
            // Wait (short deadline on a fresh spawn, longer when close to
            // the existing frontier) for the segment to land on disk.
            self.wait_for_ready(&session).await?;
            let path = session.dir.join(filename);
            self.wait_for_segment(&session, segment_index, &path)
                .await?;
            session.in_flight.fetch_add(1, Ordering::Relaxed);
            return Ok(HlsServeResult {
                body: HlsServeBody::File(path),
                content_type: content_type_for_filename(filename),
                _guard: InFlightGuard {
                    session: session.clone(),
                },
            });
        }

        // Everything else (master, audio variants, audio segments, init)
        // — handled by the original polling path.
        self.wait_for_ready(&session).await?;
        let path = session.dir.join(filename);
        if !fs::try_exists(&path).await.unwrap_or(false) {
            self.wait_for_file(&path).await?;
        }

        session.in_flight.fetch_add(1, Ordering::Relaxed);
        Ok(HlsServeResult {
            body: HlsServeBody::File(path),
            content_type: content_type_for_filename(filename),
            _guard: InFlightGuard {
                session: session.clone(),
            },
        })
    }

    /// Decide whether a pending segment request warrants killing ffmpeg
    /// and respawning at the requested offset. The thresholds are all
    /// about "will the current encoder reach this segment faster than a
    /// cold restart would?" — at 2 s segments a new spawn is ~2 s to
    /// first segment (see `READINESS_DEADLINE_SECS`), so any wait longer
    /// than that is pure waste.
    ///
    /// Returns `true` when EITHER:
    /// * No encoder has started yet (shouldn't happen after
    ///   `get_or_start`, but defensive), OR
    /// * The requested segment is before the current encoder's start —
    ///   backwards seek past the active range, nothing to wait for, OR
    /// * The requested segment is further ahead of the frontier than
    ///   `CATCHUP_WINDOW_SECS` worth of video.
    fn should_respawn_for_segment(&self, session: &Arc<HlsSession>, segment_index: u32) -> bool {
        let guard = session.encoder.lock().expect("encoder mutex poisoned");
        let Some(state) = guard.as_ref() else {
            return true;
        };
        if segment_index < state.start_segment {
            return true;
        }
        let frontier = state.highest_produced.load(Ordering::Acquire);
        let ahead_segments = segment_index.saturating_sub(frontier);
        let ahead_secs = f64::from(ahead_segments) * f64::from(HLS_TARGET_SEGMENT_SECONDS);
        ahead_secs > CATCHUP_WINDOW_SECS
    }

    /// Wait for a specific segment file to land on disk, with a deadline
    /// scaled by how far ahead of the current frontier the caller's
    /// request is. The base `READINESS_DEADLINE_SECS` value is right for
    /// the initial spawn, but a request for segment frontier+20 at 2 s
    /// segments is naturally 40 s of real-time transcoder output away —
    /// fixed deadlines would 503 that request. Scale up to keep the
    /// happy-path request alive without drifting into "how long until
    /// respawn would be cheaper" territory (the respawn threshold already
    /// covers that case).
    async fn wait_for_segment(
        &self,
        session: &Arc<HlsSession>,
        segment_index: u32,
        path: &Path,
    ) -> Result<(), HlsError> {
        let frontier = session
            .encoder
            .lock()
            .expect("encoder mutex poisoned")
            .as_ref()
            .map(|s| s.highest_produced.load(Ordering::Acquire))
            .unwrap_or(0);
        let ahead_segments = segment_index.saturating_sub(frontier);
        let ahead_secs =
            (f64::from(ahead_segments) * f64::from(HLS_TARGET_SEGMENT_SECONDS)).max(0.0) * 2.0;
        let deadline_secs = (READINESS_DEADLINE_SECS as f64).max(ahead_secs) as u64;
        let deadline = Instant::now() + Duration::from_secs(deadline_secs);
        loop {
            if fs::try_exists(path).await.unwrap_or(false) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(HlsError::NotFound);
            }
            sleep(Duration::from_millis(READINESS_POLL_MS)).await;
        }
    }

    async fn get_or_start(
        &self,
        item: &MediaItem,
        browser: BrowserHint,
    ) -> Result<Arc<HlsSession>, HlsError> {
        let session_key = SessionKey::for_request(item, browser);
        // Fast path: existing session.
        {
            let sessions = self.inner.sessions.lock().await;
            if let Some(existing) = sessions.get(&session_key) {
                return Ok(existing.clone());
            }
        }

        // Need to create. Acquire semaphore (may block if at capacity).
        let permit = self
            .inner
            .semaphore
            .clone()
            .acquire_owned()
            .await
            .map_err(|error| HlsError::SpawnFailed(format!("semaphore closed: {error}")))?;

        // Re-check under lock (another caller may have raced).
        let mut sessions = self.inner.sessions.lock().await;
        if let Some(existing) = sessions.get(&session_key) {
            return Ok(existing.clone());
        }

        let session_dir = self.inner.config.cache_dir.join(session_key.dir_name());
        fs::create_dir_all(&session_dir).await?;
        // Clean any stale files from a prior run.
        clean_directory(&session_dir).await;

        // Load the keyframe index from persistence so the SegmentPlan can
        // carve the video on real keyframe boundaries in copy mode. If
        // persistence isn't wired (test path) or the index hasn't been
        // produced yet (scan in progress, older library, probe failure),
        // `for_session` falls back to a single-segment plan — the pre-deep-
        // seek linear-only behavior.
        let keyframes = match self.inner.persistence.as_ref() {
            Some(p) => p.load_keyframes(item.id).await.unwrap_or_else(|error| {
                warn!(
                    %error,
                    media_id = %item.id,
                    "failed to load keyframes; falling back to single-segment plan"
                );
                Vec::new()
            }),
            None => Vec::new(),
        };
        let plan = SegmentPlan::for_session(
            item.playback_mode,
            item.duration_seconds.unwrap_or(0.0),
            &keyframes,
        );
        let input_path = crate::streaming::resolve_media_path(item);

        let session = Arc::new(HlsSession {
            media_id: item.id,
            dir: session_dir.clone(),
            child: Mutex::new(None),
            spawn_count: AtomicU32::new(0),
            last_access_ms: AtomicU64::new(now_ms()),
            in_flight: AtomicU32::new(0),
            ready: Notify::new(),
            is_ready: AtomicU32::new(0),
            startup: Mutex::new(StartupState { started: false }),
            spawn_error: Mutex::new(None),
            spawned_at: Mutex::new(None),
            plan,
            encoder: std::sync::Mutex::new(None),
            input_path,
            browser,
            item: item.clone(),
            _permit: permit,
            _scan_gate_guard: self.inner.scan_gate.hold(),
        });
        sessions.insert(session_key, session.clone());
        drop(sessions);

        // Per-session startup: spawn ffmpeg under the session's startup lock so concurrent
        // callers wait on the same spawn.
        self.spawn_if_needed(&session, 0).await?;
        Ok(session)
    }

    /// Spawn (or respawn) ffmpeg for this session. Called once during
    /// `get_or_start` at `start_segment = 0`, then again by
    /// `respawn_at_segment` when the player seeks outside the current
    /// encoder's advertised range.
    ///
    /// For `start_segment == 0` this runs the original first-spawn path
    /// end-to-end: write the synthesized master and variant playlists,
    /// launch ffmpeg from the beginning of the file. For any non-zero
    /// value the caller is responsible for having killed the previous
    /// ffmpeg and cleaned any stale ffmpeg-written variant state from the
    /// session dir (see `respawn_at_segment`); this function then spawns
    /// a new encoder with `-ss` / `-start_number` pointing at the
    /// requested segment.
    async fn spawn_if_needed(
        &self,
        session: &Arc<HlsSession>,
        start_segment: u32,
    ) -> Result<(), HlsError> {
        let mut startup = session.startup.lock().await;
        if startup.started && start_segment == 0 {
            return Ok(());
        }

        let ffmpeg_command = self
            .inner
            .config
            .ffmpeg_command
            .as_deref()
            .ok_or_else(|| HlsError::SpawnFailed("ffmpeg binary is not configured".into()))?;

        let item = &session.item;
        let browser = session.browser;
        let input_path = session.input_path.as_path();

        // First-spawn bookkeeping: write both playlists (master + our
        // synthesized VOD variant) before ffmpeg comes up. On respawn the
        // playlists are unchanged — they're a function of the plan, not
        // of the current encoder — so we only rewrite them the first time.
        if !startup.started {
            // Synthesize `master.m3u8` ourselves, *before* spawning ffmpeg.
            // ffmpeg's own master writer drops the video `EXT-X-STREAM-INF` when
            // it can't compute bandwidth (routine for `-c:v copy` VBR sources),
            // and no playlist-publish-rate setting coaxes it back in. We always
            // have enough probe data to emit a valid master ourselves — see
            // `build_master_playlist`. Writing it synchronously also means the
            // master is on disk the instant `spawn_if_needed` returns, so a
            // client fetching it hits an ordinary file read rather than racing
            // ffmpeg's header emission.
            let audio_streams_for_master = effective_audio_streams(&item.audio_streams);
            let master_body = build_master_playlist(item, browser, &audio_streams_for_master);
            let master_path = session.dir.join(MASTER_FILENAME);
            fs::write(&master_path, master_body)
                .await
                .map_err(|error| {
                    HlsError::SpawnFailed(format!("could not write master playlist: {error}"))
                })?;

            // Note: we intentionally do NOT write the synthesized variant
            // playlist (`stream_0.m3u8`) to disk here. ffmpeg's HLS muxer
            // would overwrite it seconds after spawn — its own variant
            // writer lives at the same filename and there's no supported
            // knob to rename it. Instead, `serve` intercepts client
            // requests for `stream_0.m3u8` and returns the synthesized body
            // from memory; ffmpeg's on-disk copy is scraped by
            // `watch_readiness` for the frontier (see `EncoderState`).
        }

        let start_offset_secs = session.plan.start_time_of(start_segment);
        let spawn_offset = if start_segment == 0 {
            SpawnOffset::ZERO
        } else {
            SpawnOffset {
                segment: start_segment,
                start_secs: start_offset_secs,
            }
        };

        let mut command = Command::new(ffmpeg_command);
        command.kill_on_drop(true);
        let args = build_ffmpeg_args(
            item,
            browser,
            input_path,
            &session.dir,
            self.inner.config.hw_accel,
            self.inner.config.vaapi_device.as_deref(),
            spawn_offset,
        );
        for arg in &args {
            command.arg(arg);
        }
        command.current_dir(&session.dir);
        // Discard stdio to prevent it from filling pipes; ffmpeg logs to stderr.
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::piped());

        // Log the full arg list at debug so operators can reproduce the
        // session outside the server when investigating encoder issues (VAAPI
        // fallbacks in particular are hardware-specific and need the exact
        // command to debug).
        info!(
            session = %session.media_id,
            mode = ?item.playback_mode,
            hw_accel = ?self.inner.config.hw_accel,
            args = %args.join(" "),
            "spawning ffmpeg for HLS session"
        );

        let spawn_instant = Instant::now();
        let mut child = command
            .spawn()
            .map_err(|error| HlsError::SpawnFailed(format!("could not start ffmpeg: {error}")))?;
        session.spawn_count.fetch_add(1, Ordering::Release);
        *session.spawned_at.lock().await = Some(spawn_instant);

        // Spawn a background reader for stderr so it doesn't block the child.
        //
        // ffmpeg uses two line terminators on stderr:
        //   * `\n` for ordinary log lines (warnings, errors, one-shot info)
        //   * `\r` for its running progress line (`frame=… fps=… speed=Nx`)
        //     — the carriage return rewinds to column 0 so a terminal keeps
        //     overwriting one line.
        // `BufReader::lines()` only splits on `\n`, so a long-running transcode
        // buffers every progress update into one enormous "line" that only
        // flushes when a warning (with `\n`) shows up. That is why previous
        // logs showed stats garbled into the session-ID field. Split on BOTH
        // terminators manually so operators actually see `speed=…` ticking in
        // the log.
        if let Some(stderr) = child.stderr.take() {
            let media_id = session.media_id;
            let spawn_error = Arc::new(Mutex::new(String::new()));
            let spawn_error_clone = spawn_error.clone();
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut reader = stderr;
                let mut buf = [0u8; 4096];
                let mut line = Vec::<u8>::with_capacity(256);
                loop {
                    match reader.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => {
                            for &byte in &buf[..n] {
                                if byte == b'\n' || byte == b'\r' {
                                    if !line.is_empty() {
                                        let text = String::from_utf8_lossy(&line).into_owned();
                                        warn!(session = %media_id, "ffmpeg: {text}");
                                        let mut buffer = spawn_error_clone.lock().await;
                                        if buffer.len() < 4096 {
                                            buffer.push_str(&text);
                                            buffer.push('\n');
                                        }
                                        line.clear();
                                    }
                                } else {
                                    line.push(byte);
                                }
                            }
                        }
                        Err(_) => break,
                    }
                }
                // Flush any trailing bytes (no terminator).
                if !line.is_empty() {
                    let text = String::from_utf8_lossy(&line).into_owned();
                    warn!(session = %media_id, "ffmpeg: {text}");
                }
            });
            // Persist the latest stderr buffer in the session so failed startup can surface it.
            // We do this by polling the buffer when readiness fails (not exposed yet — kept simple).
            let _ = spawn_error;
        }

        *session.child.lock().await = Some(child);
        startup.started = true;

        // Publish the new encoder range. Previous respawns may have left an
        // older `EncoderState` pointing at a lower `start_segment`; the
        // readiness watcher uses this to decide what segment numbers are
        // fresh, and the `serve` path uses it to decide whether the
        // currently-running ffmpeg covers a given request (and, if not,
        // whether to respawn again).
        {
            let mut encoder = session.encoder.lock().expect("encoder mutex poisoned");
            *encoder = Some(EncoderState {
                start_segment,
                start_offset_secs,
                // Segments `< start_segment` may already be on disk from a
                // prior spawn — but the "highest produced by THIS encoder"
                // is still zero until the watcher sees ffmpeg advertise one.
                highest_produced: AtomicU32::new(start_segment),
            });
        }

        // On a respawn we need to clear the ready flag so waiters re-arm on
        // the new encoder's first segment. The first spawn starts with
        // `is_ready = 0` already, so this is a no-op there.
        if start_segment != 0 {
            session.is_ready.store(0, Ordering::Release);
        }

        drop(startup);

        // Schedule readiness watcher.
        let session_for_watch = session.clone();
        tokio::spawn(async move {
            watch_readiness(session_for_watch).await;
        });

        Ok(())
    }

    /// Tear down the active ffmpeg and spawn a fresh one aligned to
    /// `target_segment`. Used when a segment request falls outside the
    /// current encoder's covered range — the client seeked further than
    /// `CATCHUP_WINDOW_SECS` past the frontier, and waiting would take
    /// longer than respawning.
    ///
    /// Preserves:
    /// * Already-produced `.m4s` files on disk. The synthesized playlist's
    ///   URIs are stable across respawns, so anything the prior encoder
    ///   wrote is still valid content — just served directly from disk on
    ///   future fetches.
    /// * The synthesized `master.m3u8` and `stream_0.m3u8` we authored
    ///   (they're a function of the plan, not of ffmpeg state).
    /// * The frozen init segment (`init_0.lock.mp4`) — the respawn will
    ///   rewrite `init_0.mp4` identically (same source codec params), but
    ///   any client reading MAP is still hitting the locked copy.
    ///
    /// Removes:
    /// * Any ffmpeg-written scratch variant playlists. ffmpeg appends to
    ///   `stream_*.m3u8` rather than truncating on startup, so a stale
    ///   file would make `watch_readiness` think the new encoder has
    ///   already produced segments it hasn't.
    async fn respawn_at_segment(
        &self,
        session: &Arc<HlsSession>,
        target_segment: u32,
    ) -> Result<(), HlsError> {
        // Kill the old encoder before touching anything on disk. A running
        // ffmpeg that's still writing segments would race the clean-up.
        if let Some(mut child) = session.child.lock().await.take() {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }

        // Reset the startup flag so `spawn_if_needed` doesn't short-circuit.
        // We're already past the "spawn ffmpeg" decision — the request
        // handler decided this respawn is needed — so treat it as a fresh
        // spawn at the new offset.
        session.startup.lock().await.started = false;

        // Scrub ffmpeg's own scratch playlists. ffmpeg writes
        // `stream_*.m3u8` in append-on-restart mode — a leftover from the
        // prior encoder would poison the frontier-scrape in
        // `watch_readiness`, which parses the tail of ffmpeg's playlist
        // to decide how far the new encoder has gotten. Deleting them
        // forces ffmpeg to write fresh files that start at the new
        // `-start_number`.
        clean_ffmpeg_scratch_playlists(&session.dir).await;

        self.spawn_if_needed(session, target_segment).await
    }

    async fn wait_for_ready(&self, session: &Arc<HlsSession>) -> Result<(), HlsError> {
        if session.is_ready.load(Ordering::Acquire) != 0 {
            return Ok(());
        }

        let deadline = Instant::now() + Duration::from_secs(READINESS_DEADLINE_SECS);
        loop {
            if session.is_ready.load(Ordering::Acquire) != 0 {
                return Ok(());
            }
            if Instant::now() >= deadline {
                let err = session.spawn_error.lock().await.clone();
                if let Some(message) = err {
                    return Err(HlsError::SpawnFailed(message));
                }
                return Err(HlsError::NotReady);
            }
            // Either the watcher signals via Notify or we tick periodically.
            let remaining = deadline.saturating_duration_since(Instant::now());
            let wait = remaining.min(Duration::from_millis(READINESS_POLL_MS));
            tokio::select! {
                _ = session.ready.notified() => {}
                _ = sleep(wait) => {}
            }
        }
    }

    async fn wait_for_file(&self, path: &Path) -> Result<(), HlsError> {
        let deadline = Instant::now() + Duration::from_secs(READINESS_DEADLINE_SECS);
        loop {
            if fs::try_exists(path).await.unwrap_or(false) {
                return Ok(());
            }
            if Instant::now() >= deadline {
                return Err(HlsError::NotFound);
            }
            sleep(Duration::from_millis(READINESS_POLL_MS)).await;
        }
    }
}

/// Background ticker that evicts idle sessions. Exits once the manager's
/// `Inner` is dropped (weak reference upgrade fails), so the task lifetime is
/// naturally bound to the manager's.
async fn run_eviction_loop(weak: Weak<Inner>) {
    let mut ticker = tokio::time::interval(Duration::from_secs(EVICTION_INTERVAL_SECS));
    // Skip the immediately-firing first tick; there's nothing to evict at
    // startup and it avoids hammering the lock right after boot.
    ticker.tick().await;
    loop {
        ticker.tick().await;
        let Some(inner) = weak.upgrade() else {
            return;
        };
        evict_idle(&inner).await;
    }
}

async fn evict_idle(inner: &Inner) {
    let idle_threshold = Duration::from_secs(inner.config.idle_secs);
    let mut sessions = inner.sessions.lock().await;
    let now = Instant::now();
    let to_evict: Vec<SessionKey> = sessions
        .iter()
        .filter(|(_, session)| {
            session.in_flight.load(Ordering::Relaxed) == 0
                && now.duration_since(session.last_access()) > idle_threshold
        })
        .map(|(id, _)| *id)
        .collect();

    let mut drained = Vec::new();
    for id in to_evict {
        if let Some(session) = sessions.remove(&id) {
            drained.push(session);
        }
    }
    drop(sessions);

    for session in drained {
        info!(session = %session.media_id, "evicting idle HLS session");
        kill_session(&session).await;
    }
}

async fn watch_readiness(session: Arc<HlsSession>) {
    // Two-phase watcher, re-entered per ffmpeg spawn:
    //
    // **Phase 1 — readiness.** Wait for ffmpeg to close its first segment
    // and list it in the variant playlist. That's the ffmpeg-side signal
    // that playback can start: the `.m4s` file is written before the
    // playlist line lands, so a file-existence check races the muxer
    // and can flip `is_ready` while the variant is still empty.
    //
    // When readiness hits on the first spawn, atomically rename
    // `init_0.mp4` to `init_0.lock.mp4` — the synthesized variant
    // playlist's `#EXT-X-MAP` points at the locked name, so by the time
    // hls.js fetches MAP it sees a stable file even if ffmpeg later
    // respawns and rewrites its own `init_0.mp4` underneath.
    //
    // **Phase 2 — frontier tracking.** Continuously parse the tail of
    // ffmpeg's own `stream_0.m3u8` (the scratch file the HLS muxer
    // writes; we don't serve it — hls.js sees our synthesized variant
    // instead) to keep `EncoderState::highest_produced` current. The
    // `serve` path reads this frontier to decide whether a segment
    // request should wait or trigger a respawn.
    //
    // The watcher has no deadline of its own. Request-scoped waiters
    // (`wait_for_ready`, `wait_for_segment`) enforce their own. Bailing
    // out early would leave a running ffmpeg invisible to future waiters
    // — and slow-start encoders (cold VAAPI, sub-realtime software on
    // 10-bit HEVC) are exactly the case where that would manifest as
    // "minutes to first frame" before idle eviction recycled the
    // session.
    let spawn_instant = session.spawned_at.lock().await.unwrap_or_else(Instant::now);
    let mut warned_slow = false;
    let mut readiness_reached = session.is_ready.load(Ordering::Acquire) != 0;

    loop {
        // Pull the frontier from ffmpeg's variant playlist every tick.
        // In Phase 1 this also doubles as the readiness check — the
        // first time we see a segment numbered ≥ start_segment we flip
        // the ready flag and lock the init segment.
        let scraped = scrape_variant_playlist_frontier(&session.dir).await;

        if let Some(scraped_frontier) = scraped {
            // Update the encoder's highest-produced cursor. Monotonic
            // per-spawn (each respawn resets it to `start_segment`), so
            // a strict max keeps the counter honest even if the playlist
            // is briefly truncated during a write.
            if let Ok(guard) = session.encoder.lock()
                && let Some(state) = guard.as_ref()
            {
                let prev = state.highest_produced.load(Ordering::Acquire);
                if scraped_frontier > prev {
                    state
                        .highest_produced
                        .store(scraped_frontier, Ordering::Release);
                }
            }

            if !readiness_reached {
                readiness_reached = true;
                // Freeze the init segment on first readiness. Subsequent
                // respawns will overwrite `init_0.mp4`, but the synthesized
                // playlist's MAP points at the locked copy, so in-flight
                // clients never see a partial write.
                freeze_init_segment(&session.dir).await;
                let elapsed = spawn_instant.elapsed();
                info!(
                    session = %session.media_id,
                    elapsed_ms = elapsed.as_millis() as u64,
                    target_segment_secs = HLS_TARGET_SEGMENT_SECONDS,
                    "HLS session became ready"
                );
                session.is_ready.store(1, Ordering::Release);
                session.ready.notify_waiters();
            }
        }

        // One-shot slow-spawn warning pre-readiness. Keeps the
        // operational signal ("this transcode is slow") without spamming
        // the log every poll interval.
        if !readiness_reached
            && !warned_slow
            && spawn_instant.elapsed() >= Duration::from_secs(READINESS_DEADLINE_SECS)
        {
            warned_slow = true;
            warn!(
                session = %session.media_id,
                deadline_ms = (READINESS_DEADLINE_SECS * 1000) as u64,
                "HLS session still not ready after deadline; ffmpeg is running sub-realtime — continuing to poll"
            );
        }

        // ffmpeg exit: nothing left to track for this spawn. Surface a
        // spawn-error message if we never reached readiness (pre-first-
        // segment crashes typically mean bad args or a codec mismatch);
        // otherwise this is a normal end-of-stream — the encoder finished
        // the file and exited cleanly.
        if child_has_exited(&session).await {
            if !readiness_reached {
                let mut buffer = session.spawn_error.lock().await;
                if buffer.is_none() {
                    *buffer = Some("ffmpeg exited before producing a manifest".into());
                }
                session.ready.notify_waiters();
            }
            return;
        }

        sleep(Duration::from_millis(READINESS_POLL_MS)).await;
    }
}

/// Atomically rename ffmpeg's raw `init_0.mp4` to the locked name the
/// synthesized variant playlist references. The synthesized playlist's
/// `#EXT-X-MAP:URI` points at `INIT_SEGMENT_FROZEN`; this rename makes
/// that file exist on disk before any client can follow the MAP.
///
/// Non-fatal on failure: a missing init segment post-readiness is
/// unusual but not catastrophic — hls.js will 404 once and retry, and
/// the next poll cycle of the watcher will re-attempt.
async fn freeze_init_segment(dir: &Path) {
    let raw = dir.join(INIT_SEGMENT_RAW);
    let frozen = dir.join(INIT_SEGMENT_FROZEN);
    // If the lock already exists we're re-entering after a respawn; the
    // frozen file is already the authoritative copy, so skip. ffmpeg will
    // have written a fresh `init_0.mp4` alongside which nobody reads.
    if fs::try_exists(&frozen).await.unwrap_or(false) {
        return;
    }
    if fs::try_exists(&raw).await.unwrap_or(false) {
        if let Err(error) = fs::rename(&raw, &frozen).await {
            warn!(%error, dir = %dir.display(), "failed to freeze init segment");
        }
    }
}

/// Scrape the tail of ffmpeg's own `stream_0.m3u8` for the highest
/// segment index it has advertised so far. Returns `None` when the
/// playlist doesn't exist yet (pre-readiness) or has no `#EXTINF:` line.
///
/// Parses the `seg_0_NNNNN.m4s` filenames produced by the muxer —
/// simpler than trying to follow `EXTINF:` durations, and matches the
/// filename format asserted by `video_segment_filename`.
async fn scrape_variant_playlist_frontier(dir: &Path) -> Option<u32> {
    // ffmpeg writes its variant playlist at the same name we serve from
    // `build_variant_playlist`. Both live in the session dir — we don't
    // write ours to disk (see `serve`), so the on-disk copy is ffmpeg's.
    let path = dir.join(VIDEO_VARIANT_FILENAME);
    let bytes = fs::read(&path).await.ok()?;
    let text = std::str::from_utf8(&bytes).ok()?;
    // Walk lines in reverse, looking for the last segment URI. Skip
    // empty lines and tag lines; the first thing we find that matches
    // the segment-filename pattern is the frontier.
    text.lines()
        .rev()
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .find_map(parse_video_segment_filename)
}

/// Non-blocking check for whether the ffmpeg child has exited. Returns `true`
/// once the process has been reaped (or was never there). `try_wait` is
/// cheap — it's a non-blocking `waitpid` — so it's safe to call from the
/// readiness poll loop.
async fn child_has_exited(session: &Arc<HlsSession>) -> bool {
    let mut guard = session.child.lock().await;
    match guard.as_mut() {
        Some(child) => match child.try_wait() {
            Ok(Some(_status)) => {
                // Reap: drop the handle so subsequent calls short-circuit.
                *guard = None;
                true
            }
            Ok(None) => false,
            // Treat a `waitpid` error as "gone" — there's nothing useful we
            // can do with it and we don't want to loop forever on a broken
            // handle.
            Err(_) => {
                *guard = None;
                true
            }
        },
        None => true,
    }
}

async fn kill_session(session: &Arc<HlsSession>) {
    if let Some(mut child) = session.child.lock().await.take() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let _ = fs::remove_dir_all(&session.dir).await;
}

/// On respawn, delete ffmpeg's own `stream_*.m3u8` scratch files so its
/// next write starts from a clean slate. ffmpeg appends to an existing
/// playlist on startup (it doesn't truncate), so a stale file would look
/// to the readiness watcher like the new encoder has already produced
/// segments it hasn't — and then the frontier estimate would run ahead of
/// the real encoder's output.
///
/// We explicitly do NOT delete `.m4s` files or init segments: already-
/// produced segments are legitimate content the next request could serve
/// directly from disk, and the init segment is identical across respawns
/// (same source codec params → same extradata).
async fn clean_ffmpeg_scratch_playlists(dir: &Path) {
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let lossy = name.to_string_lossy();
        if lossy.starts_with("stream_") && lossy.ends_with(".m3u8") {
            let _ = fs::remove_file(entry.path()).await;
        }
    }
}

/// Format a seconds value as an ffmpeg-accepted decimal string.
///
/// ffmpeg accepts both `HH:MM:SS[.ms]` and raw seconds for `-ss`. Raw
/// seconds with 3 decimals is enough precision for our segment boundaries
/// (we never subsample below 1 ms in the keyframe index) and survives the
/// round-trip through ffmpeg's float parsing — `format!("{:.6}")` would
/// add trailing zero noise, and `format!("{}")` loses precision on values
/// like `198.23199999999997`.
fn format_seconds(value: f64) -> String {
    format!("{:.3}", value)
}

async fn clean_directory(dir: &Path) {
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(_) => return,
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let _ = fs::remove_file(entry.path()).await;
    }
}

fn validate_filename(filename: &str) -> Result<(), HlsError> {
    if filename.is_empty() || filename.len() > 128 {
        return Err(HlsError::InvalidFilename);
    }
    let valid = filename
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'));
    if !valid {
        return Err(HlsError::InvalidFilename);
    }
    let lower = filename.to_ascii_lowercase();
    if !(lower.ends_with(".m3u8") || lower.ends_with(".m4s") || lower.ends_with(".mp4")) {
        return Err(HlsError::InvalidFilename);
    }
    Ok(())
}

fn content_type_for_filename(filename: &str) -> &'static str {
    let lower = filename.to_ascii_lowercase();
    if lower.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if lower.ends_with("init.mp4") || lower.ends_with(".mp4") {
        "video/mp4"
    } else {
        "video/iso.segment"
    }
}

/// Seek offset applied to an ffmpeg spawn. Zeroed for the initial session
/// spawn; carries the segment index + source PTS for every subsequent
/// respawn triggered by an out-of-range seek.
///
/// `segment` is the HLS segment number (0-based) that the new encoder
/// should produce first — written into `-start_number` so the on-disk
/// filenames align with the synthesized playlist.
///
/// `start_secs` is the same value translated to the source time axis
/// (via `SegmentPlan::start_time_of`). It's used for BOTH `-ss` (input
/// seek — fast, keyframe-aligned with `-c copy`) and `-output_ts_offset`
/// (keeps fMP4 PTS continuous with what hls.js expects for segment N at
/// offset N × duration).
#[derive(Copy, Clone, Debug)]
pub(crate) struct SpawnOffset {
    pub segment: u32,
    pub start_secs: f64,
}

impl SpawnOffset {
    /// Canonical zero — passed on first spawn. Kept as a constant so call
    /// sites visibly distinguish "first spawn, no seek" from "respawn at
    /// segment 0" (the latter is pathological but shouldn't get special
    /// handling beyond the explicit-zero check).
    pub const ZERO: Self = Self {
        segment: 0,
        start_secs: 0.0,
    };
}

pub(crate) fn build_ffmpeg_args(
    item: &MediaItem,
    browser: BrowserHint,
    input_path: &Path,
    session_dir: &Path,
    hw_accel: HwAccel,
    vaapi_device: Option<&Path>,
    offset: SpawnOffset,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    args.push("-y".into());
    // `warning` (not `error`) is the right noise floor for a streaming
    // transcoder: it surfaces the messages that actually matter for
    // diagnosing startup pathologies (VAAPI profile mismatches, hw-decode
    // fallbacks to software, unsupported pixel formats) without the
    // per-frame chatter of `info`. Pair it with `-stats` so we also get
    // ffmpeg's periodic `fps=… speed=…` progress line — the single most
    // useful number for deciding whether the pipeline is realtime.
    args.push("-loglevel".into());
    args.push("warning".into());
    args.push("-stats".into());
    // `-nostdin` prevents ffmpeg from attaching its stdin to the inherited
    // terminal (we set Stdio::null elsewhere, but this is belt-and-braces —
    // especially under orchestration where stdin might otherwise be a tty).
    args.push("-nostdin".into());
    // Bound the demuxer's probe phase. ffmpeg's defaults are generous
    // (`probesize=5000000`, `analyzeduration=5000000` µs = 5s) and on big MKV
    // files the demuxer can burn multiple real-time seconds before the first
    // packet reaches the encoder, which goes straight into time-to-first-
    // segment. 1s / 5MB is ample for well-formed consumer containers.
    args.push("-analyzeduration".into());
    args.push("1000000".into());
    args.push("-probesize".into());
    args.push("5000000".into());
    // Deliberately NOT setting `-fflags +nobuffer`: it's tempting for
    // low-latency setups, but on file input it disables the demuxer's AV
    // timing buffer, producing segments with zero-duration `#EXTINF` and
    // suppressing the master's `EXT-X-STREAM-INF` line. Stick to the
    // bounded probe settings above.

    // Decide up-front whether we'll actually transcode video. Copy-only sessions
    // (remux, audio-transcode, Safari HEVC passthrough) skip both the encoder
    // and the hardware decode path — there's nothing for the GPU to do, and
    // adding `-hwaccel` flags would force ffmpeg to load the driver for no
    // benefit (or fail outright on hosts that don't have it).
    let video_copy = match item.playback_mode {
        PlaybackMode::HlsRemux | PlaybackMode::HlsAudioTranscode => true,
        PlaybackMode::HlsFullTranscode => matches!(browser, BrowserHint::Safari),
        _ => true,
    };

    // Hardware decode flags MUST precede `-i`. They only apply to the transcode
    // tier; copy-only sessions get the software path regardless of `hw_accel`.
    if !video_copy {
        push_hw_decode_args(&mut args, hw_accel, vaapi_device);
    }

    // Input-side seek. Placed BEFORE `-i` so ffmpeg uses the fast demuxer-
    // level seek path: for `-c copy` this snaps to the nearest preceding
    // keyframe (which is exactly what `SpawnOffset` computes — segment
    // boundaries come from the keyframe index) and avoids decoding +
    // discarding any frames up to the target. Without `-ss` here a respawn
    // would burn real time decoding the whole prefix before emitting
    // anything useful; for a 2-hour title seeked to near-end that's minutes
    // of wall-clock, which defeats the entire point of the respawn.
    //
    // `-ss 0.0` is skipped entirely — it's the default behavior and adding
    // the flag only risks confusing the demuxer into an unnecessary seek.
    if offset.start_secs > 0.0 {
        args.push("-ss".into());
        args.push(format_seconds(offset.start_secs));
        // `-copyts` is what keeps the output PTS equal to the input PTS
        // (instead of being reset to zero after the seek). Combined with
        // `-output_ts_offset` below, the fMP4 segments we produce carry
        // the same PTS hls.js expects for segment N — i.e., the client's
        // current-time seek lands on the correct source timeline, not on
        // a rebased zero-origin timeline that would appear to restart.
        args.push("-copyts".into());
    }

    args.push("-i".into());
    args.push(input_path.to_string_lossy().into_owned());

    // Map video stream (optional, in case audio-only).
    args.push("-map".into());
    args.push("0:v:0?".into());

    // Map audio streams.
    let audio_streams = effective_audio_streams(&item.audio_streams);
    for stream in &audio_streams {
        args.push("-map".into());
        args.push(format!("0:a:{}", stream.local_index));
    }

    // No subtitles (sidecar route handles those).
    args.push("-sn".into());

    if video_copy {
        args.push("-c:v".into());
        args.push("copy".into());
    } else {
        push_hw_encode_args(&mut args, hw_accel);
    }

    let audio_copy = matches!(item.playback_mode, PlaybackMode::HlsRemux);
    if audio_copy {
        args.push("-c:a".into());
        args.push("copy".into());
    } else {
        args.push("-c:a".into());
        args.push("aac".into());
        args.push("-b:a".into());
        args.push("192k".into());
        args.push("-ac".into());
        args.push("2".into());
    }

    // Muxer-side low-latency tuning. `-flush_packets 1` forces every packet
    // to be written immediately rather than accumulated in an internal
    // buffer; `-muxdelay 0` / `-muxpreload 0` strip the default ~0.7s of
    // mux-time padding the HLS muxer inherits from the MP4 family. Net
    // effect: the first fmp4 segment closes roughly a GOP earlier than it
    // did before.
    args.push("-flush_packets".into());
    args.push("1".into());
    args.push("-muxdelay".into());
    args.push("0".into());
    args.push("-muxpreload".into());
    args.push("0".into());
    // Normalize any negative input PTS to zero so the muxer never has to
    // wait on a timestamp fixup before closing the first segment.
    args.push("-avoid_negative_ts".into());
    args.push("make_zero".into());

    args.push("-f".into());
    args.push("hls".into());
    args.push("-hls_time".into());
    args.push(HLS_TARGET_SEGMENT_SECONDS.to_string());
    args.push("-hls_list_size".into());
    args.push("0".into());
    // `vod`, not `event`. Both values make ffmpeg rewrite the variant
    // playlist after every closed segment (the relevant code path in
    // `hls_window()` is identical); the only difference is the
    // `#EXT-X-PLAYLIST-TYPE:` line it emits. That line is what determines
    // how hls.js behaves *during* the transcode, before `#EXT-X-ENDLIST`
    // lands:
    //
    //   * `EVENT` → hls.js's parser sets `level.live = true` until ENDLIST
    //     arrives. Live mode means unknown total duration (the media
    //     element's `duration` is just the sum of EXTINFs listed so far,
    //     which keeps growing as segments appear), no seek past the live
    //     edge, and start-position heuristics that can land the player
    //     mid-stream on the "live" segment instead of at t=0. That's the
    //     exact set of symptoms reported: unordered playback, wrong total
    //     time, no forward seek.
    //   * `VOD` → hls.js sets `level.live = false` regardless of whether
    //     ENDLIST is present yet (`live = !endList && type !== 'VOD'`). The
    //     player treats each playlist reload as a growing-but-seekable VOD
    //     snapshot, starts from t=0, and handles short-duration prefixes
    //     cleanly while ffmpeg catches up. `EXT-X-ENDLIST` still lands on
    //     clean shutdown, finalizing the "real" duration for anything the
    //     player does after ffmpeg exits.
    //
    // There is still an inherent limitation: the player only knows about
    // the segments ffmpeg has published, so forward-seeking past the
    // current transcode frontier will hit the end of the playlist until
    // the next reload. For `-c:v copy` remuxes (the common case — anime
    // remuxes run at 4–7x realtime) the frontier lands on the ENDLIST
    // within seconds, so in practice seeking "just works" the moment
    // ffmpeg finishes.
    args.push("-hls_playlist_type".into());
    args.push("vod".into());
    args.push("-hls_segment_type".into());
    args.push("fmp4".into());
    args.push("-hls_flags".into());
    args.push("independent_segments+temp_file".into());
    // Numbering for the first segment this encoder produces. Zero on initial
    // spawn (the default), non-zero on a deep-seek respawn so on-disk
    // filenames agree with what the synthesized playlist already advertises.
    // Without this, a respawn at segment 99 would write `seg_0_00000.m4s`
    // and the player would 404 looking for `seg_0_00099.m4s`.
    if offset.segment != 0 {
        args.push("-start_number".into());
        args.push(offset.segment.to_string());
    }
    args.push("-hls_segment_filename".into());
    // %05d supports up to 99999 segments. At a 2 s target that's ~55 hours
    // of content; in copy mode (keyframe-driven boundaries) the practical
    // cap is weeks. The previous %03d rolled over at 999 segments (≈33
    // min), which broke playback of feature-length films mid-playback
    // because ffmpeg happily emits `seg_0_1000.m4s` but hls.js (with a
    // synthesized playlist that expected `seg_0_999.m4s` and below) 404s.
    args.push(
        session_dir
            .join(SEGMENT_FILENAME_PATTERN)
            .to_string_lossy()
            .into_owned(),
    );
    // Intentionally NOT setting -hls_fmp4_init_filename: ffmpeg 8.x prepends the
    // variant playlist's directory to whatever value is supplied, so an absolute
    // path becomes a doubled path (e.g. "/cache/<id>//cache/<id>/init_0.mp4")
    // and a relative path with %v isn't substituted in single-variant runs. The
    // default ("init.mp4" → auto-suffixed to "init_N.mp4" per variant) writes
    // beside the variant playlists, which is exactly what we want.
    // Intentionally no `-master_pl_name` / `-master_pl_publish_rate`: we
    // write `master.m3u8` ourselves before spawning ffmpeg. ffmpeg's HLS
    // muxer decides whether to emit each variant's `EXT-X-STREAM-INF` based
    // on whether it has a bandwidth number at write time, and for `-c:v
    // copy` VBR sources with no explicit `-b:v` it prints "Bandwidth info
    // not available, set audio and video bitrates" and silently drops the
    // video STREAM-INF line — permanently, even under periodic republish.
    // The resulting master has only audio `EXT-X-MEDIA` rows; hls.js sees
    // no video variant and cannot start playback. Synthesizing the master
    // ourselves from probe data (see `build_master_playlist`) sidesteps
    // that entirely and, as a bonus, makes the master available to clients
    // the instant the session exists.

    let var_stream_map = build_var_stream_map(&audio_streams);
    args.push("-var_stream_map".into());
    args.push(var_stream_map);

    args.push(
        session_dir
            .join("stream_%v.m3u8")
            .to_string_lossy()
            .into_owned(),
    );

    args
}

/// Emits the `-hwaccel ...` flags that must appear before `-i`. These set up
/// hardware decode and (for vendors that support it) keep frames on-GPU so the
/// encoder can consume them without a copy back through system memory.
fn push_hw_decode_args(args: &mut Vec<String>, hw_accel: HwAccel, vaapi_device: Option<&Path>) {
    match hw_accel {
        HwAccel::None => {}
        HwAccel::Nvenc => {
            args.push("-hwaccel".into());
            args.push("cuda".into());
            args.push("-hwaccel_output_format".into());
            args.push("cuda".into());
        }
        HwAccel::Vaapi => {
            let device = vaapi_device
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| DEFAULT_VAAPI_DEVICE.to_string());
            // `-vaapi_device` initialises a VAAPI device and binds it as the
            // default hardware device for both filters and the encoder. This is
            // what `hwupload` and `h264_vaapi` reference. Without it (e.g. only
            // setting `-hwaccel_device`, which scopes the device to the decoder),
            // ffmpeg 8.x errors out with "A hardware device reference is required
            // to upload frames to" the moment `hwupload` is initialised on a
            // CPU-decoded source.
            args.push("-vaapi_device".into());
            args.push(device);
            args.push("-hwaccel".into());
            args.push("vaapi".into());
            args.push("-hwaccel_output_format".into());
            args.push("vaapi".into());
        }
        HwAccel::Qsv => {
            args.push("-hwaccel".into());
            args.push("qsv".into());
            args.push("-hwaccel_output_format".into());
            args.push("qsv".into());
        }
        HwAccel::VideoToolbox => {
            args.push("-hwaccel".into());
            args.push("videotoolbox".into());
        }
    }
}

/// Emits the encoder-selection block for the full-transcode tier. Each branch
/// targets one ~2s GOP via `-force_key_frames` so segment boundaries land on the
/// requested cadence regardless of accelerator. Bitrates / quality knobs are
/// tuned for single-rendition LAN streaming, not archival quality.
fn push_hw_encode_args(args: &mut Vec<String>, hw_accel: HwAccel) {
    let force_kf_value = format!("expr:gte(t,n_forced*{HLS_TARGET_SEGMENT_SECONDS})");

    match hw_accel {
        HwAccel::None => {
            args.push("-c:v".into());
            args.push("libx264".into());
            args.push("-preset".into());
            args.push("veryfast".into());
            args.push("-tune".into());
            args.push("zerolatency".into());
            args.push("-pix_fmt".into());
            args.push("yuv420p".into());
            // Force a keyframe every HLS_TARGET_SEGMENT_SECONDS so segment boundaries
            // actually land on the requested cadence. Without this, libx264 defaults
            // to keyint=250 (~10s @ 25fps) and ffmpeg has to wait for the next
            // natural keyframe before closing a segment, blowing up time-to-first-
            // segment for the transcode tier.
            args.push("-force_key_frames".into());
            args.push(force_kf_value);
        }
        HwAccel::Nvenc => {
            // p4 is the balanced NVENC preset (p1=fastest, p7=slowest).
            // `tune ll` mirrors `libx264 -tune zerolatency`. VBR with a soft cap
            // keeps quality up on high-motion scenes without blowing the buffer.
            args.push("-c:v".into());
            args.push("h264_nvenc".into());
            args.push("-preset".into());
            args.push("p4".into());
            args.push("-tune".into());
            args.push("ll".into());
            args.push("-rc".into());
            args.push("vbr".into());
            args.push("-b:v".into());
            args.push("4M".into());
            args.push("-maxrate".into());
            args.push("6M".into());
            args.push("-bufsize".into());
            args.push("8M".into());
            args.push("-force_key_frames".into());
            args.push(force_kf_value);
        }
        HwAccel::Vaapi => {
            // VAAPI requires explicit format conversion before encode. With
            // `-hwaccel_output_format vaapi` the source is already on-GPU, so
            // `hwupload` is a guard that becomes a no-op for the hot path while
            // still working when the decoder couldn't accelerate (CPU-decoded
            // fallback frames get lifted onto the GPU here).
            //
            // The trailing `scale_vaapi=format=nv12` is load-bearing for 10-bit
            // sources (HEVC Main10, common for 1080p anime): the VAAPI decoder
            // produces P010 surfaces, which `format=nv12|vaapi` happily passes
            // through because they already match the `vaapi` alternative. Feeding
            // P010 into `h264_vaapi` fails with "No usable encoding profile
            // found" on every Intel/AMD driver, since there is no 10-bit H.264
            // profile. `scale_vaapi=format=nv12` forces an on-GPU P010→NV12
            // conversion and is a cheap no-op for already-8-bit input.
            args.push("-vf".into());
            args.push("format=nv12|vaapi,hwupload,scale_vaapi=format=nv12".into());
            args.push("-c:v".into());
            args.push("h264_vaapi".into());
            args.push("-qp".into());
            args.push("23".into());
            args.push("-force_key_frames".into());
            args.push(force_kf_value);
        }
        HwAccel::Qsv => {
            // Lookahead would otherwise delay the first encoded frame by ~30
            // frames, which directly inflates time-to-first-segment.
            args.push("-c:v".into());
            args.push("h264_qsv".into());
            args.push("-preset".into());
            args.push("veryfast".into());
            args.push("-global_quality".into());
            args.push("23".into());
            args.push("-look_ahead".into());
            args.push("0".into());
            args.push("-force_key_frames".into());
            args.push(force_kf_value);
        }
        HwAccel::VideoToolbox => {
            // VideoToolbox doesn't accept the same -tune/-preset/-rc knobs as
            // the other vendors and it picks its own pixel format — passing
            // `-pix_fmt yuv420p` produces a hard error. A single `-b:v` is the
            // canonical setting.
            args.push("-c:v".into());
            args.push("h264_videotoolbox".into());
            args.push("-b:v".into());
            args.push("4M".into());
            args.push("-force_key_frames".into());
            args.push(force_kf_value);
        }
    }
}

#[derive(Debug, Clone)]
struct EffectiveAudioStream {
    local_index: usize,
    language: Option<String>,
    title: Option<String>,
    default: bool,
}

fn effective_audio_streams(streams: &[AudioStream]) -> Vec<EffectiveAudioStream> {
    if streams.is_empty() {
        return Vec::new();
    }
    streams
        .iter()
        .enumerate()
        .map(|(idx, stream)| EffectiveAudioStream {
            local_index: idx,
            language: stream.language.clone(),
            title: stream.title.clone(),
            default: stream.default,
        })
        .collect()
}

fn build_var_stream_map(audio: &[EffectiveAudioStream]) -> String {
    if audio.is_empty() {
        return "v:0".into();
    }
    let mut parts: Vec<String> = Vec::with_capacity(1 + audio.len());
    parts.push("v:0,agroup:aud".into());
    let mut default_set = false;
    for stream in audio {
        let mut entry = format!("a:{},agroup:aud", stream.local_index);
        if let Some(lang) = &stream.language {
            entry.push_str(&format!(",language:{}", sanitize_token(lang)));
        }
        entry.push_str(&format!(",name:{}", audio_rendition_name_token(stream)));
        if stream.default && !default_set {
            entry.push_str(",default:yes");
            default_set = true;
        }
        parts.push(entry);
    }
    if !default_set {
        // Promote first audio to default to avoid HLS warning.
        if let Some(first) = parts.get_mut(1) {
            first.push_str(",default:yes");
        }
    }
    parts.join(" ")
}

/// Human-facing name for an audio rendition — preferred order: explicit title,
/// language code, then a synthetic "Track N" fallback. Extracted so the master
/// playlist we synthesize agrees with the `name:` field we feed into
/// `-var_stream_map`; any drift here desyncs the master's `URI=` attribute
/// from the filename ffmpeg actually writes, and the variant request 404s.
fn audio_display_name_raw(stream: &EffectiveAudioStream) -> String {
    stream
        .title
        .clone()
        .or_else(|| stream.language.clone())
        .unwrap_or_else(|| format!("Track {}", stream.local_index + 1))
}

/// Collision-proof rendition token for `-var_stream_map name:X`.
///
/// Human labels are not unique in real libraries (two "English" tracks is
/// common: theatrical + commentary, or duplicate tags). Prefixing the
/// 0-based local index guarantees uniqueness while preserving readability.
fn audio_rendition_name_token(stream: &EffectiveAudioStream) -> String {
    let raw = sanitize_token(&audio_display_name_raw(stream));
    let suffix = if raw.is_empty() {
        "track".to_string()
    } else {
        raw
    };
    format!("a{}_{}", stream.local_index, suffix)
}

/// Filename of the audio variant playlist ffmpeg will write for this stream.
/// Must match ffmpeg's `-var_stream_map name:X` output (`stream_X.m3u8`) byte-
/// for-byte — the master we synthesize references it by this exact name.
fn audio_variant_filename(stream: &EffectiveAudioStream) -> String {
    format!("stream_{}.m3u8", audio_rendition_name_token(stream))
}

/// Filename of the video variant playlist ffmpeg writes for the first video
/// stream under our single-variant layout. Kept as a constant rather than an
/// inferred value because both the ffmpeg args (`-var_stream_map v:0`) and
/// the synthesized master reference it by this exact literal.
const VIDEO_VARIANT_FILENAME: &str = "stream_0.m3u8";

/// Group-id used for the audio `EXT-X-MEDIA` entries. Arbitrary but must be
/// stable and must not collide with any variant-stream id.
const MASTER_AUDIO_GROUP: &str = "aud";

/// Synthesizes the HLS master playlist for a session.
///
/// We own this instead of letting ffmpeg's HLS muxer generate it because
/// ffmpeg decides whether to emit each variant's `EXT-X-STREAM-INF` based on
/// whether it has a bandwidth number at playlist-write time. For a `-c:v
/// copy` session from a VBR source with no `-b:v` hint, ffmpeg logs
/// "Bandwidth info not available, set audio and video bitrates" and silently
/// omits the video STREAM-INF — and never recovers, even when told to
/// republish (`-master_pl_publish_rate 1`). The resulting master carries only
/// audio `EXT-X-MEDIA` rows; hls.js sees no video variant and playback
/// doesn't start no matter how many segments land on disk.
///
/// We have everything needed to write a correct master from the probe:
/// codec, resolution, a bandwidth estimate derived from source size and
/// duration, and audio track metadata. Writing it synchronously before
/// spawning ffmpeg also makes the master available to clients the instant
/// the session exists; only the variant playlist needs to fill before
/// playback can start.
fn build_master_playlist(
    item: &MediaItem,
    browser: BrowserHint,
    audio_streams: &[EffectiveAudioStream],
) -> String {
    let transcoding_video = matches!(item.playback_mode, PlaybackMode::HlsFullTranscode)
        && !matches!(browser, BrowserHint::Safari);
    let video_codec = hls_video_codec_string(item, transcoding_video);
    // AAC-LC. The transcode path always emits this (`-c:a aac`), and the
    // audio-copy path is gated on the source already being AAC elsewhere; if
    // a non-AAC codec does slip through, hls.js inspects the init segment
    // for real capability info and the CODECS string here is informational.
    let audio_codec = "mp4a.40.2";
    let bandwidth_bps = estimate_bandwidth_bps(item);

    let mut out = String::new();
    out.push_str("#EXTM3U\n");
    out.push_str("#EXT-X-VERSION:7\n");
    out.push_str("#EXT-X-INDEPENDENT-SEGMENTS\n");

    let has_audio = !audio_streams.is_empty();
    if has_audio {
        for stream in audio_streams {
            let display_name = audio_display_name_raw(stream);
            let uri = audio_variant_filename(stream);
            let default_attr = if stream.default { "YES" } else { "NO" };
            let language_attr = stream
                .language
                .as_deref()
                .map(|l| format!(",LANGUAGE=\"{}\"", escape_playlist_attr(l)))
                .unwrap_or_default();
            out.push_str(&format!(
                "#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"{group}\",NAME=\"{name}\"{lang},DEFAULT={default},AUTOSELECT=YES,URI=\"{uri}\"\n",
                group = MASTER_AUDIO_GROUP,
                name = escape_playlist_attr(&display_name),
                lang = language_attr,
                default = default_attr,
                uri = uri,
            ));
        }
    }

    // Single video variant — this app doesn't do ABR.
    let codecs = if has_audio {
        format!("{},{}", video_codec, audio_codec)
    } else {
        video_codec.clone()
    };
    let resolution_attr = match (item.width, item.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => format!(",RESOLUTION={}x{}", w, h),
        _ => String::new(),
    };
    let audio_attr = if has_audio {
        format!(",AUDIO=\"{}\"", MASTER_AUDIO_GROUP)
    } else {
        String::new()
    };
    out.push_str(&format!(
        "#EXT-X-STREAM-INF:BANDWIDTH={bw},CODECS=\"{codecs}\"{res}{audio}\n{uri}\n",
        bw = bandwidth_bps,
        codecs = codecs,
        res = resolution_attr,
        audio = audio_attr,
        uri = VIDEO_VARIANT_FILENAME,
    ));

    out
}

/// Escape for HLS attribute-list quoted-string values. Quotes terminate the
/// string and control characters corrupt the tag, so replace them with an
/// underscore. Intentionally strict — a munged display name is better than
/// emitting a syntactically invalid playlist line.
fn escape_playlist_attr(value: &str) -> String {
    value
        .chars()
        .map(|c| if c == '"' || c.is_control() { '_' } else { c })
        .collect()
}

/// Rough source bandwidth in bits/second, used only for the master's
/// `BANDWIDTH` attribute. We serve a single variant, so hls.js has nothing
/// to switch to — this is purely informational. Clamped to a sane window so
/// files with bogus durations don't produce absurd numbers.
fn estimate_bandwidth_bps(item: &MediaItem) -> u32 {
    const DEFAULT: u32 = 5_000_000;
    const MIN: f64 = 500_000.0;
    const MAX: f64 = 80_000_000.0;
    match item.duration_seconds {
        Some(duration) if duration > 0.0 => {
            let bits = (item.size_bytes as f64 * 8.0) / duration;
            bits.clamp(MIN, MAX) as u32
        }
        _ => DEFAULT,
    }
}

/// `CODECS=` string for the video variant. When ffmpeg is actually
/// transcoding (libx264/h264_vaapi/etc.) the output is always H.264
/// main-ish profile, so `avc1.640028` is a safe fixed value. In copy mode we
/// map from the probed source codec to a plausible HLS codec string. hls.js
/// is tolerant here — it trusts the init segment for real capability info,
/// so the master's string only needs to be "close enough" to let hls.js
/// accept the playlist in the first place.
fn hls_video_codec_string(item: &MediaItem, transcoding_video: bool) -> String {
    if transcoding_video {
        // H.264 High profile, level 4.0. Matches the output of every encoder
        // we pick in `push_hw_encode_args`.
        return "avc1.640028".to_string();
    }
    match item.video_codec.as_deref().map(str::to_ascii_lowercase) {
        Some(c) if c == "h264" || c == "avc" || c == "avc1" => "avc1.640028".to_string(),
        Some(c) if c == "hevc" || c == "h265" || c == "hev1" || c == "hvc1" => {
            "hvc1.1.6.L120.90".to_string()
        }
        Some(c) if c == "vp9" => "vp09.02.10.10".to_string(),
        Some(c) if c == "av1" => "av01.0.08M.08".to_string(),
        _ => "avc1.640028".to_string(),
    }
}

fn sanitize_token(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio(index: u32, codec: &str, language: Option<&str>, default: bool) -> AudioStream {
        AudioStream {
            index,
            codec: Some(codec.to_string()),
            channels: Some(2),
            channel_layout: Some("stereo".into()),
            language: language.map(str::to_string),
            title: None,
            default,
        }
    }

    #[test]
    fn segment_plan_fixed_grid_covers_full_duration_with_target_size() {
        let plan = SegmentPlan::fixed_grid(10.0);
        assert_eq!(plan.segment_count(), 5);
        assert_eq!(plan.start_time_of(0), 0.0);
        assert_eq!(plan.start_time_of(4), 8.0);
        assert!((plan.duration_of(4) - 2.0).abs() < f64::EPSILON);
        assert_eq!(plan.target_duration_secs, 2);
    }

    #[test]
    fn segment_plan_fixed_grid_absorbs_short_trailing_remainder() {
        // 10.5s → 5 segments of 2s + 0.5s trailer. 0.5 < MIN_SEGMENT_SECONDS
        // so the remainder is absorbed into the last segment.
        let plan = SegmentPlan::fixed_grid(10.5);
        assert_eq!(plan.segment_count(), 5);
        assert!(
            (plan.duration_of(4) - 2.5).abs() < 1e-9,
            "got {}",
            plan.duration_of(4)
        );
    }

    #[test]
    fn segment_plan_fixed_grid_emits_short_trailer_when_over_threshold() {
        // 11.0s → 5×2s + 1s trailer. 1.0 >= MIN_SEGMENT_SECONDS so the
        // trailer ships as its own segment.
        let plan = SegmentPlan::fixed_grid(11.0);
        assert_eq!(plan.segment_count(), 6);
        assert!((plan.duration_of(5) - 1.0).abs() < 1e-9);
    }

    #[test]
    fn segment_plan_keyframe_mode_matches_coalesced_boundaries() {
        // A 30-second clip with keyframes at 0, 2, 2.3 (too close — coalesce),
        // 4, 10, 20. Expect: 0, 2, 4, 10, 20.
        let plan = SegmentPlan::from_keyframes(30.0, &[0.0, 2.0, 2.3, 4.0, 10.0, 20.0]);
        assert_eq!(plan.boundaries, vec![0.0, 2.0, 4.0, 10.0, 20.0]);
        assert_eq!(plan.segment_count(), 5);
        // Last segment runs from 20 to 30.
        assert!((plan.duration_of(4) - 10.0).abs() < 1e-9);
        // Target duration covers the longest span (10s) ceiled.
        assert_eq!(plan.target_duration_secs, 10);
    }

    #[test]
    fn segment_plan_keyframe_mode_falls_back_when_first_kf_not_at_zero() {
        // If the source doesn't start with a keyframe at 0, we can't build
        // a plan that starts cleanly — degrade to single-segment linear
        // playback so the rest of the system keeps working.
        let plan = SegmentPlan::from_keyframes(30.0, &[1.5, 5.0, 10.0]);
        assert_eq!(plan.boundaries, vec![0.0]);
        assert_eq!(plan.segment_count(), 1);
    }

    #[test]
    fn segment_plan_keyframe_mode_ignores_keyframes_past_duration() {
        // Containers sometimes advertise a trailing keyframe past the stream
        // duration (audio-track only); it should be silently skipped.
        let plan = SegmentPlan::from_keyframes(10.0, &[0.0, 5.0, 15.0]);
        assert_eq!(plan.boundaries, vec![0.0, 5.0]);
    }

    #[test]
    fn segment_plan_for_session_picks_grid_for_transcode() {
        let plan = SegmentPlan::for_session(PlaybackMode::HlsFullTranscode, 8.0, &[]);
        assert_eq!(plan.boundaries, vec![0.0, 2.0, 4.0, 6.0]);
    }

    #[test]
    fn segment_plan_for_session_uses_keyframes_for_copy_mode() {
        let plan = SegmentPlan::for_session(PlaybackMode::HlsRemux, 10.0, &[0.0, 3.5, 7.0]);
        assert_eq!(plan.boundaries, vec![0.0, 3.5, 7.0]);
    }

    #[test]
    fn segment_plan_segment_index_for_time_finds_covering_segment() {
        let plan = SegmentPlan::from_keyframes(30.0, &[0.0, 5.0, 12.0, 20.0]);
        // t within segment 0
        assert_eq!(plan.segment_index_for_time(0.0), 0);
        assert_eq!(plan.segment_index_for_time(4.9), 0);
        // exact boundary of segment 1
        assert_eq!(plan.segment_index_for_time(5.0), 1);
        // inside segment 2
        assert_eq!(plan.segment_index_for_time(15.0), 2);
        // past the last boundary → last segment
        assert_eq!(plan.segment_index_for_time(29.9), 3);
    }

    #[test]
    fn segment_filename_round_trip_supports_five_digit_indices() {
        assert_eq!(video_segment_filename(0), "seg_0_00000.m4s");
        assert_eq!(video_segment_filename(42), "seg_0_00042.m4s");
        assert_eq!(video_segment_filename(3600), "seg_0_03600.m4s");
        assert_eq!(parse_video_segment_filename("seg_0_00042.m4s"), Some(42));
        assert_eq!(parse_video_segment_filename("seg_0_12345.m4s"), Some(12345));
        // Audio-variant segments are intentionally not parseable — those
        // don't have an independent respawn path.
        assert_eq!(parse_video_segment_filename("seg_1_00042.m4s"), None);
        assert_eq!(parse_video_segment_filename("seg_eng_00042.m4s"), None);
        assert_eq!(parse_video_segment_filename("master.m3u8"), None);
        assert_eq!(parse_video_segment_filename("seg_0_.m4s"), None);
    }

    #[test]
    fn synthesized_variant_playlist_has_endlist_and_every_segment() {
        let plan = SegmentPlan::fixed_grid(10.0);
        let body = build_variant_playlist(&plan);
        assert!(body.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(body.contains("#EXT-X-ENDLIST"));
        assert!(body.contains(&format!("#EXT-X-MAP:URI=\"{INIT_SEGMENT_FROZEN}\"")));
        // Target duration is the max EXTINF.
        assert!(body.contains("#EXT-X-TARGETDURATION:2"));
        // All 5 segments listed with 3-decimal EXTINFs.
        for i in 0..5 {
            assert!(
                body.contains(&video_segment_filename(i)),
                "missing segment {i}"
            );
        }
        assert!(body.contains("#EXTINF:2.000,"));
    }

    #[test]
    fn synthesized_variant_playlist_copy_mode_matches_keyframes() {
        let plan = SegmentPlan::from_keyframes(30.0, &[0.0, 3.5, 7.0, 15.0]);
        let body = build_variant_playlist(&plan);
        // Copy-mode EXTINFs are variable; verify each one is present.
        assert!(body.contains("#EXTINF:3.500,\nseg_0_00000.m4s"));
        assert!(body.contains("#EXTINF:3.500,\nseg_0_00001.m4s"));
        assert!(body.contains("#EXTINF:8.000,\nseg_0_00002.m4s"));
        assert!(body.contains("#EXTINF:15.000,\nseg_0_00003.m4s"));
        assert!(body.trim_end().ends_with("#EXT-X-ENDLIST"));
    }

    #[test]
    fn segment_plan_for_session_copy_mode_falls_back_without_keyframes() {
        // Copy-mode session with no keyframe index — single segment (still
        // correct, just not deep-seekable).
        let plan = SegmentPlan::for_session(PlaybackMode::HlsRemux, 10.0, &[]);
        assert_eq!(plan.boundaries, vec![0.0]);
    }

    #[test]
    fn validate_filename_accepts_known_extensions() {
        validate_filename("master.m3u8").unwrap();
        validate_filename("stream_0.m3u8").unwrap();
        validate_filename("seg_0_001.m4s").unwrap();
        validate_filename("init.mp4").unwrap();
    }

    #[test]
    fn validate_filename_rejects_traversal_and_unknown() {
        assert!(matches!(
            validate_filename("../etc/passwd"),
            Err(HlsError::InvalidFilename)
        ));
        assert!(matches!(
            validate_filename("master.txt"),
            Err(HlsError::InvalidFilename)
        ));
        assert!(matches!(
            validate_filename(""),
            Err(HlsError::InvalidFilename)
        ));
    }

    #[test]
    fn safari_user_agent_detection() {
        let safari = "Mozilla/5.0 (Macintosh; Intel Mac OS X 12_6) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/16.6 Safari/605.1.15";
        let chrome = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";
        let firefox = "Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0";

        assert!(matches!(
            BrowserHint::from_user_agent(Some(safari)),
            BrowserHint::Safari
        ));
        assert!(matches!(
            BrowserHint::from_user_agent(Some(chrome)),
            BrowserHint::Generic
        ));
        assert!(matches!(
            BrowserHint::from_user_agent(Some(firefox)),
            BrowserHint::Generic
        ));
        assert!(matches!(
            BrowserHint::from_user_agent(None),
            BrowserHint::Generic
        ));
    }

    #[test]
    fn var_stream_map_with_two_audio_renditions() {
        let streams = effective_audio_streams(&[
            audio(0, "aac", Some("eng"), true),
            audio(1, "ac3", Some("jpn"), false),
        ]);
        let map = build_var_stream_map(&streams);

        assert!(map.starts_with("v:0,agroup:aud"));
        assert!(map.contains("a:0,agroup:aud,language:eng"));
        assert!(map.contains("a:1,agroup:aud,language:jpn"));
        assert!(map.contains("default:yes"));
    }

    #[test]
    fn var_stream_map_promotes_default_when_missing() {
        let streams = effective_audio_streams(&[audio(0, "aac", Some("eng"), false)]);
        let map = build_var_stream_map(&streams);

        assert!(map.contains("default:yes"));
    }

    #[test]
    fn var_stream_map_names_are_unique_when_labels_collide() {
        let mut first = audio(0, "aac", Some("eng"), true);
        first.title = Some("English".into());
        let mut second = audio(1, "aac", Some("eng"), false);
        second.title = Some("English".into());
        let streams = effective_audio_streams(&[first, second]);
        let map = build_var_stream_map(&streams);

        assert!(map.contains("name:a0_English"), "map: {map}");
        assert!(map.contains("name:a1_English"), "map: {map}");
        assert_ne!(
            audio_variant_filename(&streams[0]),
            audio_variant_filename(&streams[1])
        );
    }

    #[test]
    fn master_playlist_contains_video_stream_inf_for_remux_with_audio_group() {
        // This is the invariant that silently broke under ffmpeg's master
        // writer: when it couldn't compute a bandwidth for the video variant
        // (routine for `-c:v copy` VBR sources) it would emit a master with
        // only `EXT-X-MEDIA` audio rows and no `EXT-X-STREAM-INF`. hls.js
        // would then see no video variant and fail to start playback. We now
        // synthesize the master ourselves; this test asserts the STREAM-INF
        // is always present and references the video variant playlist.
        let mut item = make_item(PlaybackMode::HlsRemux);
        item.size_bytes = 1_500_000_000; // ~1.5 GB
        item.duration_seconds = Some(1440.0); // 24 min
        item.width = Some(1920);
        item.height = Some(1080);
        item.audio_streams = vec![
            audio(0, "aac", Some("eng"), true),
            audio(1, "ac3", Some("jpn"), false),
        ];
        let audio_streams = effective_audio_streams(&item.audio_streams);

        let master = build_master_playlist(&item, BrowserHint::Generic, &audio_streams);

        // Master must always start with #EXTM3U and have a STREAM-INF that
        // references the video variant — that's the line whose absence
        // caused the original "playback never starts" bug.
        assert!(master.starts_with("#EXTM3U\n"), "master: {master}");
        assert!(
            master.contains("#EXT-X-STREAM-INF:"),
            "master must carry a STREAM-INF for the video variant: {master}"
        );
        assert!(
            master.contains(&format!("\n{}\n", VIDEO_VARIANT_FILENAME)),
            "STREAM-INF must be followed by the video variant URI: {master}"
        );
        // Resolution is used by clients to size the player; we have it from
        // the probe so it should be emitted.
        assert!(master.contains("RESOLUTION=1920x1080"), "master: {master}");
        // Both audio renditions get EXT-X-MEDIA rows with URIs that match
        // what `-var_stream_map name:X` will cause ffmpeg to write. A drift
        // between the two is an immediate 404 on the audio playlist fetch.
        assert!(
            master.contains(r#"URI="stream_a0_eng.m3u8""#),
            "english audio URI must match ffmpeg's filename: {master}"
        );
        assert!(
            master.contains(r#"URI="stream_a1_jpn.m3u8""#),
            "japanese audio URI must match ffmpeg's filename: {master}"
        );
        assert!(master.contains(r#"AUDIO="aud""#), "master: {master}");
        // Bandwidth must be a real positive number, not dropped silently.
        assert!(
            master.contains("BANDWIDTH=") && !master.contains("BANDWIDTH=0"),
            "master: {master}"
        );
    }

    #[test]
    fn master_playlist_without_audio_has_no_media_rows_and_no_audio_attr() {
        let mut item = make_item(PlaybackMode::HlsRemux);
        item.audio_streams = Vec::new();
        let audio_streams = effective_audio_streams(&item.audio_streams);

        let master = build_master_playlist(&item, BrowserHint::Generic, &audio_streams);

        assert!(master.contains("#EXT-X-STREAM-INF:"), "master: {master}");
        assert!(
            !master.contains("#EXT-X-MEDIA"),
            "no audio → no EXT-X-MEDIA lines: {master}"
        );
        assert!(
            !master.contains(r#"AUDIO=""#),
            "STREAM-INF must not carry an AUDIO= group when there are no audio renditions: {master}"
        );
    }

    #[test]
    fn master_playlist_uri_matches_var_stream_map_name() {
        // The master's `URI="stream_X.m3u8"` is only correct if X matches the
        // sanitized name we feed into `-var_stream_map name:X`. This test
        // exercises both sides of that dependency with a single stream and
        // verifies they agree. If this test fails, the master will reference
        // a variant filename ffmpeg never wrote.
        let streams = effective_audio_streams(&[audio(0, "aac", Some("eng"), true)]);
        let map = build_var_stream_map(&streams);

        // Extract the `name:` token var_stream_map produced.
        let name_token = map
            .split_whitespace()
            .find_map(|part| part.split(',').find_map(|kv| kv.strip_prefix("name:")))
            .expect("var_stream_map entry should include name:");
        let expected_uri = format!("stream_{}.m3u8", name_token);

        assert_eq!(audio_variant_filename(&streams[0]), expected_uri);
    }

    #[test]
    fn hls_video_codec_string_respects_transcode_vs_copy() {
        let mut item = make_item(PlaybackMode::HlsRemux);
        item.video_codec = Some("hevc".into());

        // Copy mode: CODECS must reflect the source codec so hls.js doesn't
        // reject the manifest on codec-string mismatch with the init segment.
        assert!(hls_video_codec_string(&item, false).starts_with("hvc1"));
        // Transcode mode: output is H.264 regardless of source. Claiming the
        // source codec here would mis-advertise the variant.
        assert!(hls_video_codec_string(&item, true).starts_with("avc1"));
    }

    #[test]
    fn estimate_bandwidth_clamps_pathological_inputs() {
        let mut item = make_item(PlaybackMode::HlsRemux);
        // Duration claimed as 0.001s on a 10 GB file would otherwise produce
        // an 80+ Gbps number that HLS clients (correctly) flag as garbage.
        item.size_bytes = 10_000_000_000;
        item.duration_seconds = Some(0.001);
        let bw = estimate_bandwidth_bps(&item);
        assert!(bw <= 80_000_000, "bandwidth must be clamped: {bw}");

        // Missing duration → a usable default rather than zero.
        let mut item = make_item(PlaybackMode::HlsRemux);
        item.duration_seconds = None;
        assert!(estimate_bandwidth_bps(&item) > 0);
    }

    fn make_item(mode: PlaybackMode) -> MediaItem {
        MediaItem {
            id: Uuid::nil(),
            root_path: "/lib".into(),
            relative_path: "movie.mkv".into(),
            file_name: "movie.mkv".into(),
            extension: Some("mkv".into()),
            size_bytes: 0,
            modified_at: String::new(),
            indexed_at: String::new(),
            content_type: None,
            duration_seconds: None,
            container_name: None,
            video_codec: Some("h264".into()),
            audio_codec: Some("aac".into()),
            width: None,
            height: None,
            probed_at: None,
            probe_error: None,
            subtitle_tracks: Vec::new(),
            thumbnail_generated_at: None,
            thumbnail_error: None,
            playback_mode: mode,
            video_profile: None,
            video_level: None,
            video_pix_fmt: None,
            video_bit_depth: None,
            audio_streams: vec![audio(0, "aac", Some("eng"), true)],
            subtitle_streams: Vec::new(),
            hls_master_url: None,
        }
    }

    fn build_args_for(mode: PlaybackMode, hw: HwAccel, vaapi: Option<&Path>) -> Vec<String> {
        build_ffmpeg_args(
            &make_item(mode),
            BrowserHint::Generic,
            Path::new("/in/movie.mkv"),
            Path::new("/cache/sess"),
            hw,
            vaapi,
            SpawnOffset::ZERO,
        )
    }

    fn build_args_for_offset(mode: PlaybackMode, offset: SpawnOffset) -> Vec<String> {
        build_ffmpeg_args(
            &make_item(mode),
            BrowserHint::Generic,
            Path::new("/in/movie.mkv"),
            Path::new("/cache/sess"),
            HwAccel::None,
            None,
            offset,
        )
    }

    /// Helper: index of the first `-i` flag, used to verify HW decode flags
    /// land in the input-options block (before `-i`) rather than the output block.
    fn input_index(args: &[String]) -> usize {
        args.iter().position(|a| a == "-i").expect("missing -i")
    }

    #[test]
    fn build_args_software_unchanged() {
        let args = build_args_for(PlaybackMode::HlsFullTranscode, HwAccel::None, None);
        assert!(args.iter().any(|a| a == "libx264"));
        assert!(args.iter().any(|a| a == "yuv420p"));
        assert!(args.iter().any(|a| a == "zerolatency"));
        assert!(args.iter().any(|a| a == "-force_key_frames"));
        // No HW accel flags should be present in the software path.
        assert!(!args.iter().any(|a| a == "-hwaccel"));
        assert!(!args.iter().any(|a| a == "h264_nvenc"));
    }

    #[test]
    fn build_args_nvenc_emits_cuda_decode_and_h264_nvenc() {
        let args = build_args_for(PlaybackMode::HlsFullTranscode, HwAccel::Nvenc, None);
        let i = input_index(&args);

        // Decode side: `-hwaccel cuda` must precede `-i`.
        let hwaccel_pos = args.iter().position(|a| a == "-hwaccel").expect("hwaccel");
        assert!(hwaccel_pos < i);
        assert_eq!(args[hwaccel_pos + 1], "cuda");
        assert!(args.iter().any(|a| a == "-hwaccel_output_format"));

        // Encode side: NVENC encoder + low-latency tune.
        assert!(args.iter().any(|a| a == "h264_nvenc"));
        assert!(args.iter().any(|a| a == "ll"));
        assert!(args.iter().any(|a| a == "vbr"));
        // Must not also pull in libx264.
        assert!(!args.iter().any(|a| a == "libx264"));
    }

    #[test]
    fn build_args_vaapi_uses_configured_device() {
        let custom = Path::new("/dev/dri/renderD129");
        let args = build_args_for(PlaybackMode::HlsFullTranscode, HwAccel::Vaapi, Some(custom));

        // `-vaapi_device` is what binds the device to filters/encoder; without
        // it `hwupload` fails at filter-graph init.
        let device_pos = args
            .iter()
            .position(|a| a == "-vaapi_device")
            .expect("vaapi_device");
        assert_eq!(args[device_pos + 1], "/dev/dri/renderD129");
        assert!(args.iter().any(|a| a == "h264_vaapi"));
        // Filter chain lifts CPU-decoded frames onto the GPU and forces NV12 so
        // 10-bit sources (P010, from HEVC Main10) don't crash h264_vaapi at
        // profile selection.
        assert!(
            args.iter()
                .any(|a| a == "format=nv12|vaapi,hwupload,scale_vaapi=format=nv12")
        );
    }

    #[test]
    fn build_args_vaapi_defaults_device_when_unset() {
        let args = build_args_for(PlaybackMode::HlsFullTranscode, HwAccel::Vaapi, None);
        let device_pos = args
            .iter()
            .position(|a| a == "-vaapi_device")
            .expect("vaapi_device");
        assert_eq!(args[device_pos + 1], DEFAULT_VAAPI_DEVICE);
    }

    #[test]
    fn build_args_qsv_disables_lookahead() {
        let args = build_args_for(PlaybackMode::HlsFullTranscode, HwAccel::Qsv, None);
        assert!(args.iter().any(|a| a == "h264_qsv"));
        let look_pos = args
            .iter()
            .position(|a| a == "-look_ahead")
            .expect("look_ahead");
        assert_eq!(args[look_pos + 1], "0");
    }

    #[test]
    fn build_args_videotoolbox_omits_pix_fmt() {
        let args = build_args_for(PlaybackMode::HlsFullTranscode, HwAccel::VideoToolbox, None);
        assert!(args.iter().any(|a| a == "h264_videotoolbox"));
        // VT picks its own pixel format; passing yuv420p produces a hard error.
        assert!(!args.iter().any(|a| a == "yuv420p"));
        // It also doesn't take libx264-style tunes.
        assert!(!args.iter().any(|a| a == "zerolatency"));
    }

    #[test]
    fn build_args_remux_ignores_hw_accel() {
        // Even with NVENC requested, copy-only sessions must stay clean — no
        // decode flags, no encoder swap. Otherwise we'd force the GPU driver
        // to load for a stream we're just remuxing through.
        let args = build_args_for(PlaybackMode::HlsRemux, HwAccel::Nvenc, None);
        assert!(!args.iter().any(|a| a == "-hwaccel"));
        assert!(!args.iter().any(|a| a == "h264_nvenc"));
        assert!(!args.iter().any(|a| a == "libx264"));
        // Should be the copy path.
        let cv_pos = args.iter().position(|a| a == "-c:v").expect("-c:v");
        assert_eq!(args[cv_pos + 1], "copy");
    }

    #[test]
    fn build_args_audio_transcode_ignores_hw_accel() {
        // Same invariant as remux: video is copied, so the GPU should stay out of it.
        let args = build_args_for(PlaybackMode::HlsAudioTranscode, HwAccel::Vaapi, None);
        assert!(!args.iter().any(|a| a == "-hwaccel"));
        assert!(!args.iter().any(|a| a == "h264_vaapi"));
        let cv_pos = args.iter().position(|a| a == "-c:v").expect("-c:v");
        assert_eq!(args[cv_pos + 1], "copy");
        // Audio side still gets transcoded to AAC.
        assert!(args.iter().any(|a| a == "aac"));
    }

    #[test]
    fn build_args_zero_offset_has_no_seek_or_start_number() {
        // The canonical first-spawn case — no seek args should leak into
        // the arg list, because `-ss 0.0` is a no-op that only risks
        // confusing the demuxer, and `-start_number 0` is the default.
        let args = build_args_for_offset(PlaybackMode::HlsRemux, SpawnOffset::ZERO);
        assert!(!args.iter().any(|a| a == "-ss"));
        assert!(!args.iter().any(|a| a == "-copyts"));
        assert!(!args.iter().any(|a| a == "-start_number"));
    }

    #[test]
    fn build_args_respawn_inserts_input_seek_and_start_number() {
        // Deep-seek respawn: ffmpeg must land on segment N at source PTS X.
        // Verify `-ss X` lands BEFORE `-i` (so it's an input-side demuxer
        // seek, not a slow pre-decode discard) and `-start_number N` lands
        // in the output section so on-disk filenames align with the
        // synthesized playlist URIs.
        let offset = SpawnOffset {
            segment: 99,
            start_secs: 198.0,
        };
        let args = build_args_for_offset(PlaybackMode::HlsRemux, offset);

        let input_pos = args.iter().position(|a| a == "-i").expect("-i");
        let ss_pos = args
            .iter()
            .position(|a| a == "-ss")
            .expect("respawn must include -ss");
        assert!(
            ss_pos < input_pos,
            "-ss must land before -i for demuxer-level seek: {args:?}"
        );
        assert_eq!(args[ss_pos + 1], "198.000");

        // -copyts is required to keep input PTS through the output;
        // without it ffmpeg rebases output to zero, and segment N would
        // carry timestamps that disagree with the synthesized playlist.
        let copyts_pos = args
            .iter()
            .position(|a| a == "-copyts")
            .expect("respawn must include -copyts");
        assert!(copyts_pos < input_pos, "-copyts must land in input block");

        // -start_number N is the output-side numbering directive.
        let start_pos = args
            .iter()
            .position(|a| a == "-start_number")
            .expect("respawn must include -start_number");
        assert!(start_pos > input_pos, "-start_number must land after -i");
        assert_eq!(args[start_pos + 1], "99");
    }

    #[tokio::test]
    async fn scrape_variant_playlist_frontier_parses_last_segment() {
        // Simulates ffmpeg's own `stream_0.m3u8` tail after it has emitted
        // three segments: the scraper should return 2 (the highest index).
        let tmp = tempfile::tempdir().expect("tempdir");
        let playlist = "\
#EXTM3U\n\
#EXT-X-VERSION:7\n\
#EXT-X-TARGETDURATION:2\n\
#EXTINF:2.000,\n\
seg_0_00000.m4s\n\
#EXTINF:2.000,\n\
seg_0_00001.m4s\n\
#EXTINF:2.000,\n\
seg_0_00002.m4s\n";
        tokio::fs::write(tmp.path().join(VIDEO_VARIANT_FILENAME), playlist)
            .await
            .expect("write playlist");
        let frontier = scrape_variant_playlist_frontier(tmp.path()).await;
        assert_eq!(frontier, Some(2));
    }

    #[tokio::test]
    async fn scrape_variant_playlist_frontier_none_when_missing() {
        // No on-disk playlist → no frontier. The watcher uses this to stay
        // in its "waiting for first segment" state without panicking.
        let tmp = tempfile::tempdir().expect("tempdir");
        assert_eq!(scrape_variant_playlist_frontier(tmp.path()).await, None);
    }

    #[tokio::test]
    async fn scrape_variant_playlist_frontier_handles_respawn_renumbering() {
        // After a respawn the first advertised segment may be numbered
        // anywhere (ffmpeg starts from `-start_number`). Scraper should
        // still return the highest number present.
        let tmp = tempfile::tempdir().expect("tempdir");
        let playlist = "\
#EXTM3U\n\
#EXTINF:2.000,\n\
seg_0_00099.m4s\n\
#EXTINF:2.000,\n\
seg_0_00100.m4s\n";
        tokio::fs::write(tmp.path().join(VIDEO_VARIANT_FILENAME), playlist)
            .await
            .expect("write playlist");
        assert_eq!(
            scrape_variant_playlist_frontier(tmp.path()).await,
            Some(100)
        );
    }

    #[tokio::test]
    async fn freeze_init_segment_atomically_renames_when_present() {
        // First readiness renames ffmpeg's init to the locked name that
        // the synthesized playlist's MAP points at.
        let tmp = tempfile::tempdir().expect("tempdir");
        let raw = tmp.path().join(INIT_SEGMENT_RAW);
        let frozen = tmp.path().join(INIT_SEGMENT_FROZEN);
        tokio::fs::write(&raw, b"init-bytes")
            .await
            .expect("write init");

        freeze_init_segment(tmp.path()).await;

        assert!(!tokio::fs::try_exists(&raw).await.unwrap());
        let frozen_bytes = tokio::fs::read(&frozen).await.expect("read frozen");
        assert_eq!(frozen_bytes, b"init-bytes");
    }

    #[tokio::test]
    async fn freeze_init_segment_is_idempotent_across_respawns() {
        // Second call after a respawn: the lock already exists, a fresh
        // `init_0.mp4` sits alongside it (ffmpeg rewrote it), and we must
        // leave the lock untouched so in-flight MAP readers never see a
        // partial write.
        let tmp = tempfile::tempdir().expect("tempdir");
        let raw = tmp.path().join(INIT_SEGMENT_RAW);
        let frozen = tmp.path().join(INIT_SEGMENT_FROZEN);
        tokio::fs::write(&frozen, b"locked-v1")
            .await
            .expect("write frozen");
        tokio::fs::write(&raw, b"ffmpeg-rewrite")
            .await
            .expect("write raw");

        freeze_init_segment(tmp.path()).await;

        // Locked content is preserved.
        let frozen_bytes = tokio::fs::read(&frozen).await.expect("read frozen");
        assert_eq!(frozen_bytes, b"locked-v1");
        // Raw copy is untouched — ffmpeg can rewrite it freely without
        // disturbing what clients are reading.
        let raw_bytes = tokio::fs::read(&raw).await.expect("read raw");
        assert_eq!(raw_bytes, b"ffmpeg-rewrite");
    }

    #[test]
    fn format_seconds_uses_three_decimals() {
        assert_eq!(format_seconds(0.0), "0.000");
        assert_eq!(format_seconds(198.0), "198.000");
        assert_eq!(format_seconds(42.5), "42.500");
        // Arbitrary floats that arise from keyframe PTS round to 3 dp.
        assert_eq!(format_seconds(198.23199999999997), "198.232");
    }

    #[test]
    fn from_env_value_aliases() {
        assert_eq!(HwAccel::from_env_value(""), Some(HwAccel::None));
        assert_eq!(HwAccel::from_env_value("none"), Some(HwAccel::None));
        assert_eq!(HwAccel::from_env_value("OFF"), Some(HwAccel::None));
        assert_eq!(HwAccel::from_env_value("nvenc"), Some(HwAccel::Nvenc));
        assert_eq!(HwAccel::from_env_value("CUDA"), Some(HwAccel::Nvenc));
        assert_eq!(HwAccel::from_env_value("nvidia"), Some(HwAccel::Nvenc));
        assert_eq!(HwAccel::from_env_value("vaapi"), Some(HwAccel::Vaapi));
        assert_eq!(HwAccel::from_env_value("qsv"), Some(HwAccel::Qsv));
        assert_eq!(HwAccel::from_env_value("quicksync"), Some(HwAccel::Qsv));
        assert_eq!(
            HwAccel::from_env_value("videotoolbox"),
            Some(HwAccel::VideoToolbox)
        );
        assert_eq!(HwAccel::from_env_value("vt"), Some(HwAccel::VideoToolbox));
        assert_eq!(HwAccel::from_env_value("garbage"), None);
    }
}
