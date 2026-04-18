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

use crate::protocol::{AudioStream, MediaItem, PlaybackMode};

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

#[derive(Debug)]
struct StartupState {
    started: bool,
}

#[derive(Debug)]
struct HlsSession {
    media_id: Uuid,
    dir: PathBuf,
    child: Mutex<Option<Child>>,
    last_access_ms: AtomicU64,
    in_flight: AtomicU32,
    ready: Notify,
    is_ready: AtomicU32,
    startup: Mutex<StartupState>,
    spawn_error: Mutex<Option<String>>,
    _permit: OwnedSemaphorePermit,
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

#[derive(Debug)]
pub struct HlsServeResult {
    pub path: PathBuf,
    pub content_type: &'static str,
    pub _guard: InFlightGuard,
}

#[derive(Clone, Debug)]
pub struct HlsSessionManager {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    sessions: Mutex<HashMap<Uuid, Arc<HlsSession>>>,
    semaphore: Arc<Semaphore>,
    config: HlsConfig,
}

impl HlsSessionManager {
    pub fn new(mut config: HlsConfig) -> Self {
        // Force the cache dir to be absolute. ffmpeg uses its own CWD when it
        // resolves output paths; if a relative cache_dir leaks through, the
        // session directory ends up nested under the spawn CWD.
        config.cache_dir = absolutize_path(config.cache_dir);
        let semaphore = Arc::new(Semaphore::new(config.max_concurrent));

        let inner = Arc::new(Inner {
            sessions: Mutex::new(HashMap::new()),
            semaphore,
            config,
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

    /// Number of currently active sessions. Each session corresponds to exactly one
    /// ffmpeg spawn (idempotent under the per-session startup mutex), so this also
    /// answers "how many ffmpeg processes have been spawned and not yet evicted?".
    pub async fn session_count(&self) -> usize {
        self.inner.sessions.lock().await.len()
    }

    pub async fn shutdown(&self) {
        let mut sessions = self.inner.sessions.lock().await;
        let drained: Vec<_> = sessions.drain().collect();
        drop(sessions);

        for (_, session) in drained {
            kill_session(&session).await;
        }
    }

    /// Returns the path to a ready HLS asset (master.m3u8 / variant playlist / segment).
    /// Spawns ffmpeg on demand. Caller MUST hold the returned `HlsServeResult` while the
    /// response is being sent (its drop guard tracks in-flight requests).
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

        // Always wait for readiness before serving any asset (including the master).
        self.wait_for_ready(&session).await?;

        let path = session.dir.join(filename);
        if !fs::try_exists(&path).await.unwrap_or(false) {
            // Single asset may not exist yet (later segment). For first request we already
            // gated on master + first segment existing; later segments stream as ffmpeg
            // produces them. Poll briefly.
            self.wait_for_file(&path).await?;
        }

        session.in_flight.fetch_add(1, Ordering::Relaxed);
        Ok(HlsServeResult {
            path,
            content_type: content_type_for_filename(filename),
            _guard: InFlightGuard {
                session: session.clone(),
            },
        })
    }

    async fn get_or_start(
        &self,
        item: &MediaItem,
        browser: BrowserHint,
    ) -> Result<Arc<HlsSession>, HlsError> {
        // Fast path: existing session.
        {
            let sessions = self.inner.sessions.lock().await;
            if let Some(existing) = sessions.get(&item.id) {
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
        if let Some(existing) = sessions.get(&item.id) {
            return Ok(existing.clone());
        }

        let session_dir = self.inner.config.cache_dir.join(item.id.to_string());
        fs::create_dir_all(&session_dir).await?;
        // Clean any stale files from a prior run.
        clean_directory(&session_dir).await;

        let session = Arc::new(HlsSession {
            media_id: item.id,
            dir: session_dir.clone(),
            child: Mutex::new(None),
            last_access_ms: AtomicU64::new(now_ms()),
            in_flight: AtomicU32::new(0),
            ready: Notify::new(),
            is_ready: AtomicU32::new(0),
            startup: Mutex::new(StartupState { started: false }),
            spawn_error: Mutex::new(None),
            _permit: permit,
        });
        sessions.insert(item.id, session.clone());
        drop(sessions);

        // Per-session startup: spawn ffmpeg under the session's startup lock so concurrent
        // callers wait on the same spawn.
        self.spawn_if_needed(item, browser, &session).await?;
        Ok(session)
    }

    async fn spawn_if_needed(
        &self,
        item: &MediaItem,
        browser: BrowserHint,
        session: &Arc<HlsSession>,
    ) -> Result<(), HlsError> {
        let mut startup = session.startup.lock().await;
        if startup.started {
            return Ok(());
        }

        let ffmpeg_command = self
            .inner
            .config
            .ffmpeg_command
            .as_deref()
            .ok_or_else(|| HlsError::SpawnFailed("ffmpeg binary is not configured".into()))?;
        let input_path = crate::streaming::resolve_media_path(item);
        let mut command = Command::new(ffmpeg_command);
        command.kill_on_drop(true);
        let args = build_ffmpeg_args(
            item,
            browser,
            &input_path,
            &session.dir,
            self.inner.config.hw_accel,
            self.inner.config.vaapi_device.as_deref(),
        );
        for arg in &args {
            command.arg(arg);
        }
        command.current_dir(&session.dir);
        // Discard stdio to prevent it from filling pipes; ffmpeg logs to stderr.
        command.stdout(std::process::Stdio::null());
        command.stderr(std::process::Stdio::piped());

        info!(
            session = %session.media_id,
            mode = ?item.playback_mode,
            "spawning ffmpeg for HLS session"
        );

        let mut child = command.spawn().map_err(|error| {
            HlsError::SpawnFailed(format!("could not start ffmpeg: {error}"))
        })?;

        // Spawn a background reader for stderr so it doesn't block the child.
        if let Some(stderr) = child.stderr.take() {
            let media_id = session.media_id;
            let spawn_error = Arc::new(Mutex::new(String::new()));
            let spawn_error_clone = spawn_error.clone();
            tokio::spawn(async move {
                use tokio::io::{AsyncBufReadExt, BufReader};
                let reader = BufReader::new(stderr);
                let mut lines = reader.lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    warn!(session = %media_id, "ffmpeg: {line}");
                    let mut buffer = spawn_error_clone.lock().await;
                    if buffer.len() < 4096 {
                        buffer.push_str(&line);
                        buffer.push('\n');
                    }
                }
            });
            // Persist the latest stderr buffer in the session so failed startup can surface it.
            // We do this by polling the buffer when readiness fails (not exposed yet — kept simple).
            let _ = spawn_error;
        }

        *session.child.lock().await = Some(child);
        startup.started = true;
        drop(startup);

        // Schedule readiness watcher.
        let session_for_watch = session.clone();
        tokio::spawn(async move {
            watch_readiness(session_for_watch).await;
        });

        Ok(())
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
    let to_evict: Vec<Uuid> = sessions
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
    // Readiness has two components:
    //   1. `master.m3u8` must exist — and it must actually list a variant
    //      (`EXT-X-STREAM-INF`), because ffmpeg emits an early version
    //      containing only audio `EXT-X-MEDIA` tags before the video
    //      variant has produced its first segment.
    //   2. At least one segment-side artifact (`init_*.mp4` or `*.m4s`)
    //      exists, so the variant playlist the master references actually
    //      has something to play.
    //
    // The content-check on master is cheap (file is a few hundred bytes)
    // and strictly stronger than plain existence, which previously let
    // clients fetch a master that had no video STREAM-INF line.
    let master = session.dir.join(MASTER_FILENAME);
    let deadline = Instant::now() + Duration::from_secs(READINESS_DEADLINE_SECS);

    loop {
        if Instant::now() >= deadline {
            let mut buffer = session.spawn_error.lock().await;
            if buffer.is_none() {
                *buffer = Some("ffmpeg did not produce a manifest in time".into());
            }
            session.ready.notify_waiters();
            return;
        }

        if master_has_variant(&master).await && first_segment_exists(&session.dir).await {
            session.is_ready.store(1, Ordering::Release);
            session.ready.notify_waiters();
            return;
        }

        sleep(Duration::from_millis(READINESS_POLL_MS)).await;
    }
}

async fn master_has_variant(master: &Path) -> bool {
    match fs::read(master).await {
        Ok(bytes) => bytes.windows(STREAM_INF_TAG.len()).any(|w| w == STREAM_INF_TAG),
        Err(_) => false,
    }
}

/// The HLS tag that declares a variant (i.e. a playable video rendition).
/// Its presence in `master.m3u8` is ffmpeg's signal that the video variant
/// has been registered and its first segment has closed.
const STREAM_INF_TAG: &[u8] = b"EXT-X-STREAM-INF";

async fn first_segment_exists(dir: &Path) -> bool {
    let mut entries = match fs::read_dir(dir).await {
        Ok(entries) => entries,
        Err(_) => return false,
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let lossy = name.to_string_lossy();
        if lossy.ends_with(".m4s") || lossy.starts_with("init_") {
            return true;
        }
    }
    false
}

async fn kill_session(session: &Arc<HlsSession>) {
    if let Some(mut child) = session.child.lock().await.take() {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    let _ = fs::remove_dir_all(&session.dir).await;
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

pub(crate) fn build_ffmpeg_args(
    item: &MediaItem,
    browser: BrowserHint,
    input_path: &Path,
    session_dir: &Path,
    hw_accel: HwAccel,
    vaapi_device: Option<&Path>,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    args.push("-y".into());
    args.push("-loglevel".into());
    args.push("error".into());
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
    args.push("-hls_playlist_type".into());
    args.push("vod".into());
    args.push("-hls_segment_type".into());
    args.push("fmp4".into());
    args.push("-hls_flags".into());
    args.push("independent_segments+temp_file".into());
    args.push("-hls_segment_filename".into());
    args.push(
        session_dir
            .join("seg_%v_%03d.m4s")
            .to_string_lossy()
            .into_owned(),
    );
    // Intentionally NOT setting -hls_fmp4_init_filename: ffmpeg 8.x prepends the
    // variant playlist's directory to whatever value is supplied, so an absolute
    // path becomes a doubled path (e.g. "/cache/<id>//cache/<id>/init_0.mp4")
    // and a relative path with %v isn't substituted in single-variant runs. The
    // default ("init.mp4" → auto-suffixed to "init_N.mp4" per variant) writes
    // beside the variant playlists, which is exactly what we want.
    args.push("-master_pl_name".into());
    args.push(MASTER_FILENAME.into());

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
            args.push("-vf".into());
            args.push("format=nv12|vaapi,hwupload".into());
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
        let name = stream
            .title
            .clone()
            .or_else(|| stream.language.clone())
            .unwrap_or_else(|| format!("Track {}", stream.local_index + 1));
        entry.push_str(&format!(",name:{}", sanitize_token(&name)));
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
        // Filter chain that lifts CPU-decoded frames onto the GPU.
        assert!(args.iter().any(|a| a == "format=nv12|vaapi,hwupload"));
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
    fn from_env_value_aliases() {
        assert_eq!(HwAccel::from_env_value(""), Some(HwAccel::None));
        assert_eq!(HwAccel::from_env_value("none"), Some(HwAccel::None));
        assert_eq!(HwAccel::from_env_value("OFF"), Some(HwAccel::None));
        assert_eq!(HwAccel::from_env_value("nvenc"), Some(HwAccel::Nvenc));
        assert_eq!(HwAccel::from_env_value("CUDA"), Some(HwAccel::Nvenc));
        assert_eq!(HwAccel::from_env_value("nvidia"), Some(HwAccel::Nvenc));
        assert_eq!(HwAccel::from_env_value("vaapi"), Some(HwAccel::Vaapi));
        assert_eq!(HwAccel::from_env_value("qsv"), Some(HwAccel::Qsv));
        assert_eq!(
            HwAccel::from_env_value("quicksync"),
            Some(HwAccel::Qsv)
        );
        assert_eq!(
            HwAccel::from_env_value("videotoolbox"),
            Some(HwAccel::VideoToolbox)
        );
        assert_eq!(HwAccel::from_env_value("vt"), Some(HwAccel::VideoToolbox));
        assert_eq!(HwAccel::from_env_value("garbage"), None);
    }
}
