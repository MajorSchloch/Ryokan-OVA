//! Layer 5 — ffprobe container / stream analysis.
//!
//! The strongest single post-download signal. Shells out to `ffprobe` with
//! `-show_streams -show_format -show_chapters -of json`, then walks the
//! parsed JSON looking for fingerprints that only appear in one source
//! type. A file with FLAC audio and PGS subtitles is, in practice,
//! always a BluRay rip; a file with AAC as its only audio track is
//! overwhelmingly a Web release; native DVD dimensions (720×480, 704×480,
//! 720×576) are diagnostic of a DVDRip.
//!
//! | Signal                                                  | Confidence |
//! |---------------------------------------------------------|------------|
//! | FLAC / TrueHD / DTS-HD MA / PCM audio                   | 0.90       |
//! | PGS (S_HDMV/PGS) subtitle track                         | 0.90       |
//! | Native DVD dimensions (720×480 / 704×480 / 720×576)     | 0.90       |
//! | Commentary audio track (detected via stream title)      | 0.90       |
//! | HEVC + FLAC + 10-bit fingerprint                        | 0.90       |
//! | AAC as the sole audio codec                             | 0.85       |
//! | H.264 + AAC + 8-bit fingerprint                         | 0.85       |
//! | E-AC-3 / DD+ audio                                      | 0.80       |
//!
//! **Intentionally useless signals** (no evidence produced): Opus, AV1,
//! x265/HEVC alone, AC-3 alone, stream track count. These appear in
//! both BluRay and Web releases and would only add noise.
//!
//! The module exposes a pure `scan_ffprobe_json` for unit testability and
//! an async `classify_ffprobe` wrapper that owns the ffprobe shell-out
//! plus file-identity-keyed caching. Missing `ffprobe` binary,
//! malformed JSON, or probe failures return an empty evidence vec — the
//! aggregator will simply fall back on the other layers.
//!
//! This module does NOT fold evidence into a final decision. It emits
//! a bag of [`SourceEvidence`] plus an observed [`Resolution`] (if any)
//! for the caller to hand to [`crate::services::source::aggregate`].

use std::path::Path;
use std::process::Stdio;
use std::time::SystemTime;

use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::process::Command;

use crate::models::media_probe_cache;
use crate::services::source::{Origin, Resolution, Source, SourceEvidence};

const ORIGIN: Origin = Origin::Ffprobe;

/// Public output of Layer 5.
#[derive(Debug, Clone, Default)]
pub struct FfprobeClassification {
    /// Zero or more pieces of source evidence extracted from the probe JSON.
    pub evidence: Vec<SourceEvidence>,
    /// Observed display resolution if the probe output included a video
    /// stream with usable dimensions. Takes precedence over filename-parsed
    /// resolution at the orchestrator level since it's a direct observation.
    pub resolution: Option<Resolution>,
}

/// Observed identity, not a content hash. Nanosecond timestamps avoid
/// reusing a probe after an in-place rewrite in the same second; Unix
/// device/inode also detects replacement with preserved timestamps.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FileIdentity {
    pub size: u64,
    /// A decimal string preserves nanoseconds in JavaScript clients.
    pub mtime_ns: String,
    pub device: Option<u64>,
    pub inode: Option<u64>,
}

impl FileIdentity {
    pub fn from_metadata(meta: &std::fs::Metadata) -> Result<Self, ProbeError> {
        let modified = meta
            .modified()
            .map_err(|e| ProbeError::new("stat_failed", e.to_string()))?;
        let mtime_ns = match modified.duration_since(SystemTime::UNIX_EPOCH) {
            Ok(delta) => delta.as_nanos().to_string(),
            Err(before) => format!("-{}", before.duration().as_nanos()),
        };
        #[cfg(unix)]
        let (device, inode) = {
            use std::os::unix::fs::MetadataExt;
            (Some(meta.dev()), Some(meta.ino()))
        };
        #[cfg(not(unix))]
        let (device, inode) = (None, None);
        Ok(Self {
            size: meta.len(),
            mtime_ns,
            device,
            inode,
        })
    }

    /// Versioned so old second-resolution cache entries cannot match.
    pub fn cache_key(&self) -> String {
        format!(
            "v2:{}:{}:{:?}:{:?}",
            self.size, self.mtime_ns, self.device, self.inode
        )
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ProbeError {
    pub code: &'static str,
    pub message: String,
}

impl ProbeError {
    pub fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for ProbeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Media probe unavailable: {}", self.message)
    }
}

impl std::error::Error for ProbeError {}

#[derive(Debug, Clone)]
pub struct CachedProbe {
    pub json: String,
    pub identity: FileIdentity,
    pub cache_hit: bool,
}

/// Probe one regular local file. Failure is distinct from a successful
/// probe with no subtitle tracks. Only raw container facts are cached;
/// callers must inspect current external subtitles separately.
pub async fn probe_cached(db: &SqlitePool, path: &Path) -> Result<CachedProbe, ProbeError> {
    let is_stub = |path: &Path| {
        path.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(crate::services::media::is_stream_stub)
    };
    if is_stub(path) {
        return Err(ProbeError::new(
            "stream_stub",
            "Stream links cannot be inspected locally",
        ));
    }
    let canonical = tokio::fs::canonicalize(path).await.map_err(|e| {
        ProbeError::new("stat_failed", format!("Cannot open local media file: {e}"))
    })?;
    if is_stub(&canonical) {
        return Err(ProbeError::new(
            "stream_stub",
            "Stream links cannot be inspected locally",
        ));
    }
    let path_str = canonical
        .to_str()
        .ok_or_else(|| ProbeError::new("invalid_path", "Media path is not valid UTF-8"))?;
    let meta = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| ProbeError::new("stat_failed", e.to_string()))?;
    if !meta.is_file() {
        return Err(ProbeError::new(
            "not_regular_file",
            "Media path is not a regular file",
        ));
    }
    let identity = FileIdentity::from_metadata(&meta)?;
    let key = identity.cache_key();
    let cached = media_probe_cache::get_identified(db, path_str, &key).await;
    let cache_hit = cached.is_some();
    let json = match cached {
        Some(json) => json,
        None => run_ffprobe(&canonical).await?,
    };
    let value: Value = serde_json::from_str(&json)
        .map_err(|e| ProbeError::new("invalid_output", format!("Invalid ffprobe output: {e}")))?;
    if !value.get("streams").is_some_and(Value::is_array) {
        return Err(ProbeError::new(
            "invalid_output",
            "ffprobe output has no stream list",
        ));
    }
    let after = tokio::fs::metadata(&canonical)
        .await
        .map_err(|e| ProbeError::new("stat_failed", e.to_string()))?;
    if FileIdentity::from_metadata(&after)? != identity {
        return Err(ProbeError::new(
            "file_changed",
            "Media file changed during inspection; retry after processing finishes",
        ));
    }
    if !cache_hit {
        media_probe_cache::upsert_identified(
            db,
            path_str,
            &key,
            identity.size.min(i64::MAX as u64) as i64,
            &json,
        )
        .await;
    }
    Ok(CachedProbe {
        json,
        identity,
        cache_hit,
    })
}

/// Run ffprobe against `path` (or return a cached result), parse the JSON,
/// and emit Layer 5 evidence. Returns an empty classification on any error —
/// missing binary, missing file, cache miss that then fails to spawn, probe
/// timeout, malformed JSON — so the caller can always safely aggregate the
/// result without null-checking.
///
/// Failure branches emit `tracing::warn!` with the offending path and the
/// underlying error, so a silently missing-ffprobe install or a corrupt
/// file is visible in the logs. Successful zero-evidence results (the
/// "intentionally useless" fingerprints documented above) are not warned
/// about — they're the happy path for audio-only or AV1/Opus files.
pub async fn classify_ffprobe(db: &SqlitePool, path: &Path) -> FfprobeClassification {
    // A `.strm` stub (issue #287) is a text file holding a URL. There is
    // no video to measure, and following the URL is the media server's
    // business, never Ryokan's.
    if path
        .file_name()
        .and_then(|n| n.to_str())
        .is_some_and(crate::services::media::is_stream_stub)
    {
        return FfprobeClassification::default();
    }
    let probe = match probe_cached(db, path).await {
        Ok(probe) => probe,
        Err(error) => {
            tracing::warn!(target: "ryokan::source::ffprobe", path = %path.display(), %error, "ffprobe inspection failed");
            return FfprobeClassification::default();
        }
    };
    tracing::debug!(
        target: "ryokan::source::ffprobe",
        path = %path.display(),
        cache_hit = probe.cache_hit,
        size = probe.identity.size,
        "ffprobe probe ready"
    );

    scan_ffprobe_json_logged(&path.to_string_lossy(), &probe.json)
}

/// Largest ffprobe stdout Ryokan keeps. A real episode's probe (streams,
/// format, chapters) is 25-70 KB; a crafted 13 MB mkv with 50k chapters
/// and an 8 MB tag printed 25 MB, all of which used to be buffered,
/// parsed into a `Value`, and cached. A probe over the cap is dropped.
const FFPROBE_STDOUT_CAP: usize = 4 << 20;
/// stderr only feeds the non-zero-exit warn line.
const FFPROBE_STDERR_CAP: usize = 64 << 10;

/// Spawn `ffprobe` and capture its JSON output. Returns an explicit error
/// on any failure including a missing binary. We pass `-v error` to suppress
/// ffprobe's banner so cache hits compare byte-for-byte.
///
/// Each failure branch emits a targeted `warn!` so the user can see
/// *why* ffprobe didn't contribute evidence: missing binary vs. probe
/// refusal vs. oversized or non-UTF8 output are distinct failure modes
/// with different remediation.
async fn run_ffprobe(path: &Path) -> Result<String, ProbeError> {
    // Capture stderr so the non-zero-exit branch can surface the reason.
    // kill_on_drop ensures the child is reaped if the surrounding
    // timeout fires (and would otherwise leave an orphan ffprobe
    // chewing CPU forever on a corrupt container).
    let spawned = Command::new("ffprobe")
        .arg("-v")
        .arg("error")
        // Containers must never cause ffprobe to follow network URLs.
        .arg("-protocol_whitelist")
        .arg("file")
        .arg("-show_streams")
        .arg("-show_format")
        .arg("-show_chapters")
        .arg("-of")
        .arg("json")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(err) => {
            tracing::warn!(
                target: "ryokan::source::ffprobe",
                path = %path.display(),
                %err,
                "ffprobe spawn failed — is the `ffprobe` binary installed and on PATH?"
            );
            return Err(ProbeError::new(
                "spawn_failed",
                format!("Cannot start ffprobe: {err}"),
            ));
        }
    };
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let probe = async {
        let (out, err) = tokio::join!(
            read_capped(stdout, FFPROBE_STDOUT_CAP),
            read_capped(stderr, FFPROBE_STDERR_CAP),
        );
        (out, err, child.wait().await)
    };

    // Cap ffprobe at 60s. A partially-downloaded mkv with a corrupt
    // container or a Blu-ray .m2ts with an inconsistent index will spin
    // ffprobe forever, and the post-processing pass holds POST_PROC_LOCK
    // for the duration — one bad file otherwise pins the entire import
    // queue. ffprobe finishes in milliseconds for healthy files; 60s is
    // generous enough that any non-pathological input completes well
    // under the cap.
    const FFPROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    let (stdout, stderr, status) = match tokio::time::timeout(FFPROBE_TIMEOUT, probe).await {
        Ok((Ok(out), Ok(err), Ok(status))) => (out, err, status),
        Ok((out, err, status)) => {
            let err = [out.err(), err.err(), status.err()]
                .into_iter()
                .flatten()
                .next()
                .map(|e| e.to_string())
                .unwrap_or_default();
            tracing::warn!(
                target: "ryokan::source::ffprobe",
                path = %path.display(),
                %err,
                "ffprobe output read failed"
            );
            return Err(ProbeError::new(
                "read_failed",
                format!("Cannot read ffprobe output: {err}"),
            ));
        }
        Err(_) => {
            tracing::warn!(
                target: "ryokan::source::ffprobe",
                path = %path.display(),
                timeout_secs = FFPROBE_TIMEOUT.as_secs(),
                "ffprobe timed out — file likely has a corrupt container or partial download"
            );
            return Err(ProbeError::new(
                "timeout",
                "ffprobe exceeded the 60 second time limit",
            ));
        }
    };
    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr.kept);
        tracing::warn!(
            target: "ryokan::source::ffprobe",
            path = %path.display(),
            code = ?status.code(),
            stderr = stderr.trim(),
            "ffprobe exited non-zero"
        );
        return Err(ProbeError::new(
            "probe_failed",
            "ffprobe could not read the media file",
        ));
    }
    if stdout.truncated {
        tracing::warn!(
            target: "ryokan::source::ffprobe",
            path = %path.display(),
            cap_mb = FFPROBE_STDOUT_CAP >> 20,
            "ffprobe output exceeded the size cap; skipping probe"
        );
        return Err(ProbeError::new(
            "output_too_large",
            "ffprobe output exceeded its size limit",
        ));
    }
    match String::from_utf8(stdout.kept) {
        Ok(s) => Ok(s),
        Err(err) => {
            tracing::warn!(
                target: "ryokan::source::ffprobe",
                path = %path.display(),
                %err,
                "ffprobe stdout was not valid UTF-8"
            );
            Err(ProbeError::new(
                "invalid_output",
                "ffprobe output is not valid UTF-8",
            ))
        }
    }
}

/// What [`read_capped`] kept of a pipe.
struct CappedRead {
    kept: Vec<u8>,
    /// More than `cap` bytes arrived; `kept` holds the first `cap`.
    truncated: bool,
}

/// Read `pipe` to EOF keeping at most `cap` bytes. It drains past the
/// cap rather than stopping there: a full pipe would block ffprobe
/// until the timeout. A missing pipe reads as empty.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(
    pipe: Option<R>,
    cap: usize,
) -> std::io::Result<CappedRead> {
    use tokio::io::AsyncReadExt;

    let mut out = CappedRead {
        kept: Vec::new(),
        truncated: false,
    };
    let Some(mut pipe) = pipe else {
        return Ok(out);
    };
    let mut chunk = vec![0_u8; 64 << 10];
    loop {
        let n = pipe.read(&mut chunk).await?;
        if n == 0 {
            return Ok(out);
        }
        let room = cap.saturating_sub(out.kept.len());
        out.truncated |= n > room;
        out.kept.extend_from_slice(&chunk[..n.min(room)]);
    }
}

/// Thin wrapper around `scan_ffprobe_json` that adds path-aware warn
/// logging on JSON parse failures and on the empty-video-stream branch.
/// The pure scanner is kept free of I/O and logging so unit tests can
/// keep calling it directly with canned fixtures.
fn scan_ffprobe_json_logged(path: &str, json: &str) -> FfprobeClassification {
    if serde_json::from_str::<Value>(json).is_err() {
        tracing::warn!(
            target: "ryokan::source::ffprobe",
            path,
            "ffprobe JSON parse failed; skipping probe"
        );
        return FfprobeClassification::default();
    }
    let (facts, out) = scan_ffprobe_json_with_facts(json);
    log_ffprobe_summary(path, &facts, &out);
    if out.evidence.is_empty() && out.resolution.is_none() {
        // No video stream at all — log at debug since this is normal for
        // audio-only files or probes of weird container formats, but
        // still worth tracing when debugging.
        tracing::debug!(
            target: "ryokan::source::ffprobe",
            path,
            "ffprobe: no video stream present, no evidence emitted"
        );
    }
    out
}

/// Emit a one-line debug summary of every ffprobe pass. Captures the
/// observed dimensions / resolution, video + audio codec fingerprints,
/// subtitle / commentary flags, and the evidence vec the rules
/// produced. Without this the only signal a probe ran was whatever
/// pieces of evidence happened to bubble up into `log_classification`
/// in `services/source/mod.rs` — a probe that emitted **zero** evidence
/// (the "intentionally useless" path for AV1/Opus, or any file that
/// didn't fingerprint a single rule) was completely silent, and the
/// observed resolution was never logged anywhere even when it was the
/// signal that overrode the filename layer.
fn log_ffprobe_summary(path: &str, facts: &ProbeFacts, out: &FfprobeClassification) {
    if !facts.has_video {
        return;
    }
    let resolution_label = out
        .resolution
        .map(|r| r.as_str().to_string())
        .unwrap_or_else(|| "unknown".to_string());
    let audio_codecs = if facts.audio_codecs.is_empty() {
        "(none)".to_string()
    } else {
        facts.audio_codecs.join(",")
    };
    let evidence_summary = if out.evidence.is_empty() {
        "(none)".to_string()
    } else {
        out.evidence
            .iter()
            .map(|e| format!("{}:{:.2} \"{}\"", e.source.as_str(), e.confidence, e.detail))
            .collect::<Vec<_>>()
            .join(", ")
    };
    tracing::debug!(
        target: "ryokan::source::ffprobe",
        path,
        width = facts.width,
        height = facts.height,
        resolution = %resolution_label,
        video_codec = %facts.video_codec,
        bit_depth = facts.bit_depth,
        audio_codecs = %audio_codecs,
        has_pgs_subs = facts.has_pgs_subs,
        has_commentary = facts.has_commentary,
        evidence = %evidence_summary,
        "ffprobe scan"
    );
}

/// Pure scanner: takes a ffprobe JSON document as a string and emits
/// classification evidence. Kept free of I/O and fs access so the unit
/// tests can feed in canned fixtures with no shell-out.
///
/// Production code now routes through [`scan_ffprobe_json_with_facts`]
/// so the logged wrapper can surface the intermediate facts bag; this
/// thin wrapper stays public for the existing test suite.
#[cfg_attr(not(test), allow(dead_code))]
pub fn scan_ffprobe_json(json: &str) -> FfprobeClassification {
    scan_ffprobe_json_with_facts(json).1
}

fn video_disposition(stream: &Value, name: &str) -> bool {
    // Match the disposition flags used by media_info and requirements.
    stream
        .get("disposition")
        .and_then(|flags| flags.get(name))
        .is_some_and(|value| value.as_i64() == Some(1) || value.as_bool() == Some(true))
}

/// Internal variant that also returns the intermediate [`ProbeFacts`]
/// bag. Used by `scan_ffprobe_json_logged` to emit a structured debug
/// summary of every probe — the observed dimensions, codecs, and
/// fingerprint flags — alongside the final evidence vec. Tests stick
/// with the thinner [`scan_ffprobe_json`] wrapper so they don't have
/// to spell out facts that aren't relevant to whatever rule they're
/// exercising.
fn scan_ffprobe_json_with_facts(json: &str) -> (ProbeFacts, FfprobeClassification) {
    let Ok(root) = serde_json::from_str::<Value>(json) else {
        return (ProbeFacts::default(), FfprobeClassification::default());
    };
    let Some(streams) = root.get("streams").and_then(|s| s.as_array()) else {
        return (ProbeFacts::default(), FfprobeClassification::default());
    };

    // Use the same primary video as requirements: first default real
    // video, otherwise first real video. A later auxiliary image may
    // lack attached_pic and must not overwrite the main video's facts.
    let mut videos = streams.iter().enumerate().filter(|(_, stream)| {
        stream
            .get("codec_type")
            .and_then(Value::as_str)
            .is_some_and(|kind| kind.eq_ignore_ascii_case("video"))
            && !video_disposition(stream, "attached_pic")
    });
    let primary_video = videos
        .clone()
        .find(|(_, stream)| video_disposition(stream, "default"))
        .or_else(|| videos.next())
        .map(|(index, _)| index);

    // First pass: collect the facts we need for the rule evaluation.
    let mut facts = ProbeFacts::default();
    for (index, s) in streams.iter().enumerate() {
        let codec_type = s
            .get("codec_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let codec_name = s
            .get("codec_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_ascii_lowercase();

        match codec_type.as_str() {
            "video" => {
                if Some(index) != primary_video {
                    continue;
                }
                facts.has_video = true;
                facts.video_codec = codec_name.clone();
                facts.width = s.get("width").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                facts.height = s.get("height").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                // Bit depth comes from pix_fmt (yuv420p10le → 10-bit, etc.)
                // or bits_per_raw_sample when present.
                let pix_fmt = s
                    .get("pix_fmt")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if pix_fmt.contains("10") {
                    facts.bit_depth = 10;
                } else if pix_fmt.contains("12") {
                    facts.bit_depth = 12;
                } else if !pix_fmt.is_empty() {
                    facts.bit_depth = 8;
                }
                if let Some(bps) = s.get("bits_per_raw_sample").and_then(|v| v.as_str())
                    && let Ok(n) = bps.parse::<u8>()
                    && n > 0
                {
                    facts.bit_depth = n;
                }
            }
            "audio" => {
                facts.audio_codecs.push(codec_name.clone());
                // Detect commentary tracks via stream title metadata. Title
                // typically lives under tags.title.
                let title = s
                    .get("tags")
                    .and_then(|t| t.get("title"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                if !title.is_empty()
                    && (title.contains("commentary")
                        || title.contains("audio commentary")
                        || title.contains("director"))
                {
                    facts.has_commentary = true;
                }
            }
            // PGS (Blu-ray) subtitles come through as "hdmv_pgs_subtitle"
            // under codec_name. Also accept the older S_HDMV/PGS form that
            // shows up in some mkvtoolnix-produced files.
            "subtitle" if codec_name.contains("pgs") || codec_name.contains("hdmv_pgs") => {
                facts.has_pgs_subs = true;
            }
            _ => {}
        }
    }

    let mut out = FfprobeClassification::default();
    if !facts.has_video {
        return (facts, out);
    }

    // Observed resolution — always set if we have a video stream with
    // readable dimensions, even when no source evidence fires.
    if facts.width > 0 && facts.height > 0 {
        let res = Resolution::from_observed_dimensions(facts.width, facts.height);
        if res != Resolution::Unknown {
            out.resolution = Some(res);
        }
    }

    // Rule: DVD-native dimensions. 720×480, 704×480 (NTSC), 720×576 (PAL).
    // This is diagnostic regardless of codec — any file with these exact
    // dimensions is a DVD rip.
    if matches!(
        (facts.width, facts.height),
        (720, 480) | (704, 480) | (720, 576)
    ) {
        out.evidence.push(SourceEvidence::new(
            Source::Dvd,
            0.90,
            ORIGIN,
            format!("native DVD dimensions {}x{}", facts.width, facts.height),
        ));
    }

    // Rule: PGS subtitles → BluRay. PGS is the bitmap subtitle format used
    // on retail Blu-ray discs. Web releases never ship PGS.
    if facts.has_pgs_subs {
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            "PGS subtitle track present",
        ));
    }

    // Rule: commentary track → BluRay. Only retail BD releases ship
    // director / cast commentary tracks.
    if facts.has_commentary {
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            "commentary audio track",
        ));
    }

    // Rule: high-fidelity audio codecs → BluRay. FLAC, TrueHD, DTS-HD MA,
    // and PCM are the four lossless / master-audio formats that streaming
    // services don't ship.
    let has_flac = facts.audio_codecs.iter().any(|c| c == "flac");
    let has_truehd = facts.audio_codecs.iter().any(|c| c == "truehd");
    // ffprobe reports DTS-HD MA as `dts_hd_ma` and DTS-HD HRA as
    // `dts_hd_hra`. Match those codec IDs explicitly rather than doing a
    // loose "contains dts && contains hd" substring scan, which could
    // false-positive on e.g. a hypothetical "dts_hdcam" codec and misses
    // the point of being a strict BD-exclusive signal.
    let has_dts_hd = facts
        .audio_codecs
        .iter()
        .any(|c| c == "dts_hd_ma" || c == "dts_hd_hra");
    // PCM on ffprobe comes through as e.g. "pcm_s16le" / "pcm_s24le".
    let has_pcm = facts.audio_codecs.iter().any(|c| c.starts_with("pcm_"));
    if has_flac || has_truehd || has_dts_hd || has_pcm {
        let codec = if has_flac {
            "FLAC"
        } else if has_truehd {
            "TrueHD"
        } else if has_dts_hd {
            "DTS-HD MA"
        } else {
            "PCM"
        };
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            format!("{} audio codec", codec),
        ));
    }

    // Rule: E-AC-3 / DD+ → Web. Streaming services (Amazon, Netflix,
    // Disney+) have standardized on DDP for their HD releases. Not
    // diagnostic on its own — but good enough to lean Web.
    let has_ddp = facts
        .audio_codecs
        .iter()
        .any(|c| c == "eac3" || c == "ec-3" || c == "e-ac-3");
    if has_ddp {
        out.evidence.push(SourceEvidence::new(
            Source::Web,
            0.80,
            ORIGIN,
            "E-AC-3 / DD+ audio codec",
        ));
    }

    // Rule: AAC as the sole audio codec → Web. AAC is uncommon on BD
    // releases — BDs ship lossless. An AAC-only file is almost always a
    // streaming rip. The check is "contains aac and no high-fidelity
    // codec" rather than "only aac" because the streamable-set vs.
    // BluRay distinction is the whole point.
    let has_aac = facts.audio_codecs.iter().any(|c| c == "aac");
    if has_aac && !has_flac && !has_truehd && !has_dts_hd && !has_pcm {
        out.evidence.push(SourceEvidence::new(
            Source::Web,
            0.85,
            ORIGIN,
            "AAC audio without lossless track",
        ));
    }

    // Combo fingerprint: H.264 + AAC + 8-bit → Web encode. Catches the
    // typical streaming profile so files that would otherwise pull only
    // the weaker AAC rule get a dedicated high-confidence signal.
    let is_h264 = facts.video_codec == "h264" || facts.video_codec == "avc1";
    if is_h264 && has_aac && facts.bit_depth == 8 && !has_flac && !has_truehd && !has_dts_hd {
        out.evidence.push(SourceEvidence::new(
            Source::Web,
            0.85,
            ORIGIN,
            "H.264 + AAC + 8-bit streaming fingerprint",
        ));
    }

    // Combo fingerprint: HEVC + FLAC + 10-bit → BluRay encode. This is
    // the signature of community BD re-encodes (VCB, Beatrice-Raws, etc.).
    let is_hevc = facts.video_codec == "hevc" || facts.video_codec == "h265";
    if is_hevc && has_flac && facts.bit_depth == 10 {
        out.evidence.push(SourceEvidence::new(
            Source::BluRay,
            0.90,
            ORIGIN,
            "HEVC + FLAC + 10-bit BD encode fingerprint",
        ));
    }

    (facts, out)
}

#[derive(Default)]
struct ProbeFacts {
    has_video: bool,
    video_codec: String,
    width: u32,
    height: u32,
    bit_depth: u8,
    audio_codecs: Vec<String>,
    has_pgs_subs: bool,
    has_commentary: bool,
}

// ───────────────────────────────────────────────────────────────────────────
// Tests
// ───────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_beast_tamer_main_video_is_not_replaced_by_later_jpeg() {
        // Actual cached E01: default HEVC 1080p precedes a JPEG video
        // without attached_pic. The old last-video rule reported 576p.
        let json = include_str!("../../tests/fixtures/ffprobe/beast-tamer-e01-primary-video.json");
        let (facts, out) = scan_ffprobe_json_with_facts(json);
        assert_eq!(out.resolution, Some(Resolution::R1080p));
        assert_eq!((facts.width, facts.height), (1920, 1080));
        assert_eq!(facts.video_codec, "hevc");
        assert_eq!(facts.bit_depth, 10);
        assert_eq!(facts.audio_codecs, ["aac", "aac"]);
    }

    #[test]
    fn primary_video_uses_first_default_or_first_real_stream() {
        let plain = video_stream("hevc", 1280, 720, "yuv420p10le");
        let mut default = video_stream("h264", 1920, 1080, "yuv420p");
        default["disposition"] = serde_json::json!({"default":1});
        let mut later_default = video_stream("av1", 3840, 2160, "yuv420p12le");
        later_default["disposition"] = serde_json::json!({"default":true});
        for (streams, expected, codec, depth) in [
            (
                vec![plain.clone(), default.clone()],
                Resolution::R1080p,
                "h264",
                8,
            ),
            (
                vec![default.clone(), later_default.clone()],
                Resolution::R1080p,
                "h264",
                8,
            ),
            (
                vec![plain.clone(), later_default],
                Resolution::R2160p,
                "av1",
                12,
            ),
            (
                vec![plain, video_stream("h264", 1920, 1080, "yuv420p")],
                Resolution::R720p,
                "hevc",
                10,
            ),
        ] {
            let (facts, out) = scan_ffprobe_json_with_facts(&probe_json(streams));
            assert_eq!(out.resolution, Some(expected));
            assert_eq!(facts.video_codec, codec);
            assert_eq!(facts.bit_depth, depth);
        }
    }

    #[test]
    fn primary_video_skips_attached_defaults_without_falling_past_missing_dimensions() {
        let mut attached = attached_pic_stream("mjpeg", 3840, 2160);
        attached["disposition"]["default"] = serde_json::json!(true);
        attached["disposition"]["attached_pic"] = serde_json::json!(true);
        assert!(
            scan_ffprobe_json(&probe_json(vec![attached.clone()]))
                .resolution
                .is_none()
        );
        let main = video_stream("hevc", 1280, 720, "yuv420p10le");
        let out = scan_ffprobe_json(&probe_json(vec![attached.clone(), main, attached]));
        assert_eq!(out.resolution, Some(Resolution::R720p));

        let mut missing = video_stream("hevc", 0, 0, "yuv420p10le");
        missing.as_object_mut().unwrap().remove("width");
        missing.as_object_mut().unwrap().remove("height");
        missing["disposition"] = serde_json::json!({"default":1});
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            missing,
        ]));
        assert!(out.resolution.is_none());
    }

    #[test]
    fn primary_video_keeps_audio_and_subtitle_evidence_from_all_streams() {
        let (facts, out) = scan_ffprobe_json_with_facts(&probe_json(vec![
            audio_stream_with_title("eac3", "Director Commentary"),
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            video_stream("h264", 400, 564, "yuv420p"),
            audio_stream("flac"),
            subtitle_stream("hdmv_pgs_subtitle"),
        ]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
        assert_eq!(facts.video_codec, "hevc");
        assert_eq!(facts.bit_depth, 10);
        assert_eq!(facts.audio_codecs, ["eac3", "flac"]);
        assert!(facts.has_commentary);
        assert!(facts.has_pgs_subs);
        assert!(
            out.evidence
                .iter()
                .any(|e| e.detail == "HEVC + FLAC + 10-bit BD encode fingerprint")
        );
    }

    #[test]
    fn cropped_hd_video_keeps_its_observed_resolution_tier() {
        for (width, height, expected) in [
            (1920, 816, Resolution::R1080p),
            (1920, 800, Resolution::R1080p),
            (3840, 1600, Resolution::R2160p),
            (1280, 536, Resolution::R720p),
        ] {
            let out = scan_ffprobe_json(&probe_json(vec![video_stream(
                "hevc",
                width,
                height,
                "yuv420p10le",
            )]));
            assert_eq!(out.resolution, Some(expected), "{width}x{height}");
        }
    }

    #[test]
    fn observed_resolution_preserves_sd_and_missing_dimension_controls() {
        for (width, height, expected) in [
            (1280, 720, Some(Resolution::R720p)),
            (704, 480, Some(Resolution::R480p)),
            (720, 576, Some(Resolution::R576p)),
            (640, 360, None),
            (0, 1080, None),
            (1920, 0, None),
        ] {
            let out = scan_ffprobe_json(&probe_json(vec![video_stream(
                "hevc",
                width,
                height,
                "yuv420p10le",
            )]));
            assert_eq!(out.resolution, expected, "{width}x{height}");
        }
    }

    /// Minimal ffprobe-shaped JSON builder for tests. Composes a root with
    /// a single video stream and an arbitrary list of audio/subtitle streams.
    fn probe_json(streams: Vec<Value>) -> String {
        serde_json::to_string(&serde_json::json!({
            "streams": streams,
            "format": {},
        }))
        .unwrap()
    }

    fn video_stream(codec: &str, width: u32, height: u32, pix_fmt: &str) -> Value {
        serde_json::json!({
            "codec_type": "video",
            "codec_name": codec,
            "width": width,
            "height": height,
            "pix_fmt": pix_fmt,
        })
    }

    fn audio_stream(codec: &str) -> Value {
        serde_json::json!({
            "codec_type": "audio",
            "codec_name": codec,
        })
    }

    /// Cover-art / attached-picture stream — what ffprobe emits for the
    /// embedded poster/thumbnail many MKV releases carry. ffprobe tags
    /// these with `codec_type = "video"` but marks them via
    /// `disposition.attached_pic = 1`. Used to pin down the attached-pic
    /// regression where these streams overwrote the real video track.
    fn attached_pic_stream(codec: &str, width: u32, height: u32) -> Value {
        serde_json::json!({
            "codec_type": "video",
            "codec_name": codec,
            "width": width,
            "height": height,
            "disposition": { "attached_pic": 1 },
        })
    }

    fn audio_stream_with_title(codec: &str, title: &str) -> Value {
        serde_json::json!({
            "codec_type": "audio",
            "codec_name": codec,
            "tags": { "title": title },
        })
    }

    fn subtitle_stream(codec: &str) -> Value {
        serde_json::json!({
            "codec_type": "subtitle",
            "codec_name": codec,
        })
    }

    #[test]
    fn malformed_json_is_empty() {
        let out = scan_ffprobe_json("not json");
        assert!(out.evidence.is_empty());
        assert!(out.resolution.is_none());
    }

    #[test]
    fn empty_streams_is_empty() {
        let out = scan_ffprobe_json(&probe_json(vec![]));
        assert!(out.evidence.is_empty());
        assert!(out.resolution.is_none());
    }

    #[test]
    fn video_only_sets_resolution() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "h264", 1920, 1080, "yuv420p",
        )]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    /// Attached-picture streams (MKV cover art) must not clobber the main
    /// video's width/height. Regression guard for an EMBER Wajutsushi
    /// release where a ~150-tall thumbnail sat after the real 1080p
    /// track and dropped the reported height below
    /// `Resolution::from_dimensions`'s 460-pixel floor — classification
    /// for 11 of 12 episodes came back with no resolution tag at all.
    #[test]
    fn attached_pic_after_main_video_does_not_overwrite_resolution() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            attached_pic_stream("mjpeg", 500, 150),
            audio_stream("eac3"),
        ]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    /// Same bug, reversed stream order — attached_pic first (as some
    /// containers emit), then the real video. Without the disposition
    /// check the real video's dims would overwrite the thumbnail's and
    /// the test would coincidentally pass, so we keep both orderings.
    #[test]
    fn attached_pic_before_main_video_does_not_overwrite_resolution() {
        let out = scan_ffprobe_json(&probe_json(vec![
            attached_pic_stream("mjpeg", 500, 150),
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("eac3"),
        ]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    #[test]
    fn dvd_dimensions_fire_dvd_rule() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "mpeg2video",
            720,
            480,
            "yuv420p",
        )]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Dvd));
        assert_eq!(out.resolution, Some(Resolution::R480p));
    }

    #[test]
    fn pal_dvd_dimensions_also_fire() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "mpeg2video",
            720,
            576,
            "yuv420p",
        )]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Dvd));
    }

    #[test]
    fn flac_audio_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            audio_stream("flac"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn hevc_flac_10bit_combo_fires_bd_encode_rule() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            audio_stream("flac"),
        ]));
        // Should have both the FLAC rule AND the combo-fingerprint rule.
        let bd_count = out
            .evidence
            .iter()
            .filter(|e| e.source == Source::BluRay)
            .count();
        assert!(bd_count >= 2);
    }

    #[test]
    fn truehd_audio_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("truehd"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn dts_hd_ma_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("dts"), // fake — dts_hd_ma shows up as "dts" with a profile
        ]));
        // Plain DTS doesn't fire — we need "dts" + "hd" in the name.
        assert!(
            !out.evidence
                .iter()
                .any(|e| { e.source == Source::BluRay && e.detail.contains("DTS") })
        );
        let out2 = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("dts_hd_ma"),
        ]));
        assert!(out2.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn pcm_audio_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("mpeg2video", 1920, 1080, "yuv420p"),
            audio_stream("pcm_s16le"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn aac_only_is_web() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("aac"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Web));
    }

    #[test]
    fn aac_with_flac_is_not_web() {
        // Dual audio BDs sometimes carry both an AAC downmix and a FLAC
        // master. FLAC wins.
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p10le"),
            audio_stream("aac"),
            audio_stream("flac"),
        ]));
        assert!(!out.evidence.iter().any(|e| e.source == Source::Web));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn h264_aac_8bit_fires_streaming_fingerprint() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("aac"),
        ]));
        // AAC-only rule + combo fingerprint = at least 2 Web hits.
        let web_count = out
            .evidence
            .iter()
            .filter(|e| e.source == Source::Web)
            .count();
        assert!(web_count >= 2);
    }

    #[test]
    fn eac3_is_web() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream("eac3"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::Web));
    }

    #[test]
    fn pgs_subtitles_are_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            subtitle_stream("hdmv_pgs_subtitle"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn commentary_track_is_bluray() {
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("hevc", 1920, 1080, "yuv420p"),
            audio_stream_with_title("ac3", "Director Commentary"),
        ]));
        assert!(out.evidence.iter().any(|e| e.source == Source::BluRay));
    }

    #[test]
    fn opus_audio_produces_no_evidence() {
        // Opus is explicitly on the "useless signal" list.
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("av1", 1920, 1080, "yuv420p"),
            audio_stream("opus"),
        ]));
        assert!(out.evidence.is_empty());
    }

    #[test]
    fn bare_ac3_produces_no_evidence() {
        // AC-3 alone is too ambiguous (both BD and Web ship it for
        // backwards-compatible tracks).
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("h264", 1920, 1080, "yuv420p"),
            audio_stream("ac3"),
        ]));
        assert!(out.evidence.is_empty());
    }

    #[test]
    fn hevc_alone_produces_no_evidence() {
        let out = scan_ffprobe_json(&probe_json(vec![video_stream(
            "hevc", 1920, 1080, "yuv420p",
        )]));
        assert!(out.evidence.is_empty());
    }

    #[test]
    fn resolution_set_even_when_no_source_evidence() {
        // Useless-signal file — we should still know it's 1080p.
        let out = scan_ffprobe_json(&probe_json(vec![
            video_stream("av1", 1920, 1080, "yuv420p"),
            audio_stream("opus"),
        ]));
        assert_eq!(out.resolution, Some(Resolution::R1080p));
    }

    #[test]
    fn missing_video_stream_emits_nothing() {
        let out = scan_ffprobe_json(&probe_json(vec![audio_stream("flac")]));
        assert!(out.evidence.is_empty());
        assert!(out.resolution.is_none());
    }

    #[tokio::test]
    async fn a_strm_stub_is_never_probed() {
        // Issue #287. A cached probe for the path would otherwise be
        // served without running ffprobe, so one is seeded for both
        // files: the `.mkv` reads it back and the `.strm` does not.
        let db = crate::test_support::in_memory_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let json = probe_json(vec![video_stream("hevc", 1920, 1080, "yuv420p10le")]);
        let mut results = Vec::new();
        for name in ["Show - S01E06.mkv", "Show - S01E06.strm"] {
            let path = tmp.path().join(name);
            std::fs::write(&path, b"https://example.com/6").unwrap();
            let meta = std::fs::metadata(&path).unwrap();
            let identity = FileIdentity::from_metadata(&meta).unwrap();
            let canonical = std::fs::canonicalize(&path).unwrap();
            media_probe_cache::upsert_identified(
                &db,
                canonical.to_str().unwrap(),
                &identity.cache_key(),
                meta.len() as i64,
                &json,
            )
            .await;
            results.push(classify_ffprobe(&db, &path).await);
        }
        assert_eq!(results[0].resolution, Some(Resolution::R1080p));
        assert!(results[1].evidence.is_empty());
        assert!(results[1].resolution.is_none());
    }

    #[tokio::test]
    async fn read_capped_keeps_a_pipe_under_the_cap_whole() {
        let out = read_capped(Some(&b"{\"streams\":[]}"[..]), 64)
            .await
            .unwrap();
        assert_eq!(out.kept, b"{\"streams\":[]}");
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn read_capped_at_exactly_the_cap_is_not_truncated() {
        let out = read_capped(Some(&[7_u8; 10][..]), 10).await.unwrap();
        assert_eq!(out.kept.len(), 10);
        assert!(!out.truncated);
    }

    #[tokio::test]
    async fn read_capped_keeps_the_prefix_and_drains_the_rest() {
        // Larger than one 64 KiB read, so the cap lands mid-stream and
        // later reads still have to be drained.
        let body: Vec<u8> = (0..200_000_u32).map(|i| (i % 251) as u8).collect();
        let mut reader = &body[..];
        let out = read_capped(Some(&mut reader), 100_000).await.unwrap();
        assert_eq!(out.kept, &body[..100_000]);
        assert!(out.truncated);
        assert!(reader.is_empty(), "the pipe was drained to EOF");
    }

    #[tokio::test]
    async fn read_capped_reads_a_missing_pipe_as_empty() {
        let out = read_capped(None::<&[u8]>, 10).await.unwrap();
        assert!(out.kept.is_empty());
        assert!(!out.truncated);
    }
}
