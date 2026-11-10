//! Observed local media facts and subtitle requirements.
//!
//! Language comes from stream tags or a sidecar's filename. It is a
//! declaration, not proof of the language spoken in the subtitle text.
//! External subtitles are read afresh; a video cache hit cannot hide a
//! missing, changed, empty or invalid sidecar.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::Value;
use sqlx::SqlitePool;
use tokio::io::AsyncReadExt;

use super::media::{self, EpisodeSpan};
use super::post_processing::extras;
pub use super::source_ffprobe::{FileIdentity, ProbeError};

const SIDECAR_BYTE_CAP: usize = 16 << 20;
const SIDECAR_COUNT_CAP: usize = 128;
const DIRECTORY_ENTRY_CAP: usize = 4096;

#[derive(Debug, Clone, Serialize)]
pub struct VideoStream {
    pub index: u32,
    pub codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bit_depth: Option<u32>,
    pub language: Option<String>,
    pub raw_language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioStream {
    pub index: u32,
    pub codec: String,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub sample_rate: Option<u32>,
    pub language: Option<String>,
    pub raw_language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubtitleStream {
    /// ffprobe index, or None for an external subtitle file.
    pub index: Option<u32>,
    pub codec: String,
    /// Normalized declared language. None means unknown/unrecognised.
    pub language: Option<String>,
    pub raw_language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
    pub signs_only: bool,
    pub hearing_impaired: bool,
    pub external: bool,
    pub path: Option<String>,
    /// Embedded tracks are confirmed present in the container, without
    /// claiming that packet contents or translation quality were checked.
    pub valid: bool,
    pub validation_status: String,
    pub cue_count: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaInfo {
    pub identity: FileIdentity,
    pub cache_hit: bool,
    pub inspected_at: String,
    pub container: Option<String>,
    pub duration_seconds: Option<f64>,
    pub video: Vec<VideoStream>,
    pub audio: Vec<AudioStream>,
    pub subtitles: Vec<SubtitleStream>,
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct SubtitleRequirementResult {
    pub enabled: bool,
    pub satisfied: bool,
    pub match_mode: String,
    pub required_languages: Vec<String>,
    pub available_languages: Vec<String>,
    pub missing_languages: Vec<String>,
    pub reasons: Vec<String>,
}

/// Normalize ISO 639 language aliases, human-readable common names and
/// regional variants. Unknown/undefined/multiple-language tags stay None.
pub fn normalize_language(raw: &str) -> Option<String> {
    let lower = raw.trim().to_ascii_lowercase().replace('_', "-");
    let base = lower.split('-').next()?;
    let language = match base {
        "eng" | "english" => "en",
        "deu" | "ger" | "german" => "de",
        "jpn" | "jp" | "japanese" => "ja",
        "fra" | "fre" | "french" => "fr",
        "spa" | "spanish" | "lat" | "latino" => "es",
        "por" | "portuguese" | "ptbr" | "brazilian" => "pt",
        "ita" | "italian" => "it",
        "rus" | "russian" => "ru",
        "ara" | "arabic" => "ar",
        "zho" | "chi" | "chinese" => "zh",
        "kor" | "korean" => "ko",
        "nld" | "dut" | "dutch" => "nl",
        "pol" | "polish" => "pl",
        "tur" | "turkish" => "tr",
        "ind" | "indonesian" => "id",
        "tha" | "thai" => "th",
        "vie" | "vietnamese" => "vi",
        "swe" | "swedish" => "sv",
        "dan" | "danish" => "da",
        "fin" | "finnish" => "fi",
        "hun" | "hungarian" => "hu",
        "ces" | "cze" | "czech" => "cs",
        "ell" | "gre" | "greek" => "el",
        "heb" | "hebrew" => "he",
        "msa" | "may" | "malay" => "ms",
        "ron" | "rum" | "romanian" => "ro",
        "ukr" | "ukrainian" => "uk",
        "hin" | "hindi" => "hi",
        "fil" | "tgl" | "filipino" | "tagalog" => "tl",
        "isl" | "ice" | "icelandic" => "is",
        "nor" | "norwegian" => "no",
        "nob" => "nb",
        "nno" => "nn",
        "slk" | "slo" | "slovak" => "sk",
        "slv" | "slovenian" => "sl",
        "hrv" | "croatian" => "hr",
        "srp" | "serbian" => "sr",
        "bos" | "bosnian" => "bs",
        "bul" | "bulgarian" => "bg",
        "est" | "estonian" => "et",
        "lav" | "latvian" => "lv",
        "lit" | "lithuanian" => "lt",
        "fas" | "per" | "persian" => "fa",
        "urd" | "urdu" => "ur",
        "ben" | "bengali" => "bn",
        "tam" | "tamil" => "ta",
        "tel" | "telugu" => "te",
        "kan" | "kannada" => "kn",
        "mal" | "malayalam" => "ml",
        "mar" | "marathi" => "mr",
        "guj" | "gujarati" => "gu",
        "pan" | "punjabi" => "pa",
        "nep" | "nepali" => "ne",
        "sin" | "sinhala" => "si",
        "mya" | "bur" | "burmese" => "my",
        "khm" | "khmer" => "km",
        "lao" => "lo",
        "mon" | "mongolian" => "mn",
        "kaz" | "kazakh" => "kk",
        "uzb" | "uzbek" => "uz",
        "aze" | "azerbaijani" => "az",
        "kat" | "geo" | "georgian" => "ka",
        "hye" | "arm" | "armenian" => "hy",
        "sqi" | "alb" | "albanian" => "sq",
        "mkd" | "mac" | "macedonian" => "mk",
        "eus" | "baq" | "basque" => "eu",
        "cat" | "catalan" => "ca",
        "glg" | "galician" => "gl",
        "cym" | "wel" | "welsh" => "cy",
        "gle" | "irish" => "ga",
        "afr" | "afrikaans" => "af",
        "swa" | "swahili" => "sw",
        "amh" | "amharic" => "am",
        "som" | "somali" => "so",
        "zul" | "zulu" => "zu",
        "bel" | "belarusian" => "be",
        code if code.len() == 2 && ISO_639_1.split_whitespace().any(|known| known == code) => code,
        _ => return None,
    };
    Some(language.to_string())
}

const ISO_639_1: &str = "aa ab ae af ak am an ar as av ay az ba be bg bh bi bm bn bo br bs ca ce ch co cr cs cu cv cy da de dv dz ee el en eo es et eu fa ff fi fj fo fr fy ga gd gl gn gu gv ha he hi ho hr ht hu hy hz ia id ie ig ii ik io is it iu ja jv ka kg ki kj kk kl km kn ko kr ks ku kv kw ky la lb lg li ln lo lt lu lv mg mh mi mk ml mn mr ms mt my na nb nd ne ng nl nn no nr nv ny oc oj om or os pa pi pl ps pt qu rm rn ro ru rw sa sc sd se sg si sk sl sm sn so sq sr ss st su sv sw ta te tg th ti tk tl tn to tr ts tt tw ty ug uk ur uz ve vi vo wa wo xh yi yo za zh zu";

/// Inspect library media with current adjacent subtitles. This uses exact
/// filename/episode matching, never the single-video fallback which would
/// attach a different episode's otherwise-unmatched subtitles.
pub async fn inspect(db: &SqlitePool, video: &Path) -> Result<MediaInfo, ProbeError> {
    let mut candidates = Vec::new();
    let mut warning = None;
    if let Some(parent) = video.parent() {
        let mut entries = tokio::fs::read_dir(parent).await.map_err(|e| {
            ProbeError::new(
                "sidecar_scan_failed",
                format!("Cannot inspect adjacent subtitles: {e}"),
            )
        })?;
        let mut count = 0;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| ProbeError::new("sidecar_scan_failed", e.to_string()))?
        {
            count += 1;
            if count > DIRECTORY_ENTRY_CAP {
                warning = Some("Adjacent subtitle scan reached its entry limit; some subtitles may be unverified".to_string());
                break;
            }
            if is_subtitle_path(&entry.path())
                && entry.file_type().await.is_ok_and(|kind| kind.is_file())
            {
                candidates.push(entry.path());
            }
        }
    }
    let span = video
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|name| media::parse_episode_span(&name.to_ascii_lowercase()));
    let mut info = inspect_with_sidecars(db, video, &candidates, span, usize::MAX).await?;
    if let Some(warning) = warning {
        info.warnings.push(warning);
    }
    Ok(info)
}

/// Pre-import inspection using the same sidecar matching as the importer.
/// The caller supplies the download's safe, wanted candidate paths and its
/// total video count. A requirement must also be checked after sidecars
/// have landed, before the old usable media can be retired.
pub async fn inspect_with_sidecars(
    db: &SqlitePool,
    video: &Path,
    candidates: &[PathBuf],
    video_span: Option<EpisodeSpan>,
    videos_in_download: usize,
) -> Result<MediaInfo, ProbeError> {
    let probe = super::source_ffprobe::probe_cached(db, video).await?;
    let mut info = parse_probe_json(&probe.json, probe.identity, probe.cache_hit)?;
    let filtered: Vec<PathBuf> = candidates
        .iter()
        .filter(|p| is_subtitle_path(p))
        .cloned()
        .collect();
    let mut matched = extras::find_subtitles(video, video_span, &filtered, videos_in_download);
    matched.dedup();
    if matched.len() > SIDECAR_COUNT_CAP {
        return Err(ProbeError::new(
            "too_many_sidecars",
            "Subtitle file count exceeded its inspection limit",
        ));
    }
    for path in matched {
        info.subtitles.push(inspect_sidecar(video, &path).await);
    }
    Ok(info)
}

fn is_subtitle_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|e| matches!(e.to_ascii_lowercase().as_str(), "srt" | "ass" | "ssa"))
}

fn string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string)
}

fn number(value: Option<&Value>) -> Option<u32> {
    value.and_then(|v| {
        v.as_u64()
            .and_then(|n| u32::try_from(n).ok())
            .or_else(|| v.as_str()?.parse().ok())
    })
}

fn disposition(stream: &Value, key: &str) -> bool {
    stream
        .get("disposition")
        .and_then(|d| d.get(key))
        .is_some_and(|v| v.as_i64() == Some(1) || v.as_bool() == Some(true))
}

fn tag(stream: &Value, name: &str) -> Option<String> {
    stream
        .get("tags")
        .and_then(Value::as_object)
        .and_then(|tags| tags.iter().find(|(key, _)| key.eq_ignore_ascii_case(name)))
        .and_then(|(_, value)| string(Some(value)))
}

fn title_flags(title: Option<&str>) -> (bool, bool, bool) {
    let words: Vec<String> = title
        .unwrap_or_default()
        .to_ascii_lowercase()
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_string)
        .collect();
    let has = |word: &str| words.iter().any(|w| w == word);
    let full = has("full") || has("dialogue") || has("dialog");
    let signs = !full
        && (has("signs") || has("sign") || has("songs") || has("song") || (has("op") && has("ed")));
    (
        has("forced"),
        signs,
        has("sdh") || has("cc") || (has("hearing") && has("impaired")),
    )
}

/// Pure parser used by source fixtures and metadata regression tests.
pub fn parse_probe_json(
    json: &str,
    identity: FileIdentity,
    cache_hit: bool,
) -> Result<MediaInfo, ProbeError> {
    let value: Value = serde_json::from_str(json)
        .map_err(|e| ProbeError::new("invalid_output", format!("Invalid ffprobe output: {e}")))?;
    let streams = value
        .get("streams")
        .and_then(Value::as_array)
        .ok_or_else(|| ProbeError::new("invalid_output", "ffprobe output has no stream list"))?;
    let mut info = MediaInfo {
        identity,
        cache_hit,
        inspected_at: chrono::Utc::now().to_rfc3339(),
        container: string(value.get("format").and_then(|f| f.get("format_name"))),
        duration_seconds: value
            .get("format")
            .and_then(|f| f.get("duration"))
            .and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok()))
            .filter(|d| d.is_finite() && *d >= 0.0),
        video: Vec::new(),
        audio: Vec::new(),
        subtitles: Vec::new(),
        warnings: Vec::new(),
    };
    for stream in streams {
        let index = number(stream.get("index")).unwrap_or(0);
        let codec = string(stream.get("codec_name")).unwrap_or_else(|| "unknown".to_string());
        let raw_language = tag(stream, "language");
        let language = raw_language.as_deref().and_then(normalize_language);
        let title = tag(stream, "title");
        let default = disposition(stream, "default");
        let (title_forced, signs_only, title_hi) = title_flags(title.as_deref());
        let forced = disposition(stream, "forced") || title_forced;
        match stream.get("codec_type").and_then(Value::as_str) {
            Some("video") if !disposition(stream, "attached_pic") => info.video.push(VideoStream {
                index,
                codec,
                width: number(stream.get("width")),
                height: number(stream.get("height")),
                bit_depth: number(stream.get("bits_per_raw_sample"))
                    .filter(|bits| *bits > 0)
                    .or_else(|| {
                        bit_depth_from_pixel_format(stream.get("pix_fmt").and_then(Value::as_str))
                    }),
                language,
                raw_language,
                title,
                default,
                forced,
            }),
            Some("audio") => info.audio.push(AudioStream {
                index,
                codec,
                channels: number(stream.get("channels")),
                channel_layout: string(stream.get("channel_layout")),
                sample_rate: number(stream.get("sample_rate")),
                language,
                raw_language,
                title,
                default,
                forced,
            }),
            Some("subtitle") => {
                // nb_read_packets is normally absent: this is a metadata
                // probe, not an expensive whole-file subtitle packet scan.
                let known_empty = stream
                    .get("nb_read_packets")
                    .is_some_and(|v| v.as_str() == Some("0") || v.as_u64() == Some(0));
                info.subtitles.push(SubtitleStream {
                    index: Some(index),
                    codec,
                    language,
                    raw_language,
                    title,
                    default,
                    forced,
                    signs_only,
                    hearing_impaired: disposition(stream, "hearing_impaired") || title_hi,
                    external: false,
                    path: None,
                    valid: !known_empty,
                    validation_status: if known_empty {
                        "empty_track"
                    } else {
                        "container_track_present"
                    }
                    .to_string(),
                    cue_count: None,
                });
            }
            _ => {}
        }
    }
    if info.video.is_empty() {
        return Err(ProbeError::new(
            "no_video_stream",
            "The file contains no usable video stream",
        ));
    }
    if info.subtitles.iter().any(|subtitle| subtitle.valid) {
        info.warnings.push("Embedded subtitle tracks are present in the container. Packet contents and translation language have not been verified.".to_string());
    }
    Ok(info)
}

fn bit_depth_from_pixel_format(format: Option<&str>) -> Option<u32> {
    let format = format?;
    for suffix in ["le", "be"] {
        if let Some(base) = format.strip_suffix(suffix) {
            let digits: String = base
                .chars()
                .rev()
                .take_while(char::is_ascii_digit)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            if let Ok(bits) = digits.parse::<u32>()
                && matches!(bits, 9..=16)
            {
                return Some(bits);
            }
        }
    }
    if format.starts_with("yuv") || format == "nv12" || format == "nv21" {
        Some(8)
    } else {
        None
    }
}

fn sidecar_labels(video: &Path, path: &Path) -> (Option<String>, Option<String>, bool, bool, bool) {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let video_stem = video
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let tail = stem.strip_prefix(&video_stem).unwrap_or(&stem);
    let (forced, signs_only, mut hearing_impaired) = title_flags(Some(tail));
    let tokens: Vec<&str> = tail
        .split(['.', '_', ' ', '[', ']', '(', ')'])
        .filter(|t| !t.is_empty())
        .collect();
    let mut raw_language = None;
    for token in tokens.iter().rev() {
        if matches!(
            *token,
            "forced"
                | "sdh"
                | "cc"
                | "hi"
                | "signs"
                | "sign"
                | "songs"
                | "song"
                | "full"
                | "dialogue"
                | "subtitles"
                | "subs"
                | "only"
                | "and"
                | "&"
        ) || token.parse::<u32>().is_ok()
        {
            hearing_impaired |= matches!(*token, "sdh" | "cc" | "hi");
            continue;
        }
        if normalize_language(token).is_some() {
            raw_language = Some((*token).to_string());
        }
        break;
    }
    let language = raw_language.as_deref().and_then(normalize_language);
    (language, raw_language, forced, signs_only, hearing_impaired)
}

async fn inspect_sidecar(video: &Path, path: &Path) -> SubtitleStream {
    let (language, raw_language, forced, signs_only, hearing_impaired) =
        sidecar_labels(video, path);
    let codec = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut stream = SubtitleStream {
        index: None,
        codec: codec.clone(),
        language,
        raw_language,
        title: path
            .file_name()
            .and_then(|n| n.to_str())
            .map(str::to_string),
        default: false,
        forced,
        signs_only,
        hearing_impaired,
        external: true,
        path: Some(path.to_string_lossy().into_owned()),
        valid: false,
        validation_status: "unreadable".to_string(),
        cue_count: None,
    };
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_and_validate_sidecar(path, &codec),
    )
    .await
    .unwrap_or_else(|_| Err("inspection_timeout".to_string()));
    match result {
        Ok(cues) => {
            stream.valid = true;
            stream.validation_status = "timed_text_present".to_string();
            stream.cue_count = Some(cues);
        }
        Err(reason) => stream.validation_status = reason,
    }
    stream
}

async fn read_and_validate_sidecar(path: &Path, codec: &str) -> Result<usize, String> {
    let before = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| "unreadable".to_string())?;
    if !before.is_file() {
        return Err("not_regular_file".to_string());
    }
    if before.len() > SIDECAR_BYTE_CAP as u64 {
        return Err("size_limit_exceeded".to_string());
    }
    let identity = FileIdentity::from_metadata(&before).map_err(|_| "unreadable".to_string())?;
    let file = tokio::fs::File::open(path)
        .await
        .map_err(|_| "unreadable".to_string())?;
    let opened = file
        .metadata()
        .await
        .map_err(|_| "unreadable".to_string())?;
    if FileIdentity::from_metadata(&opened).map_err(|_| "unreadable".to_string())? != identity {
        return Err("file_changed".to_string());
    }
    let mut bytes = Vec::with_capacity((before.len() as usize).min(SIDECAR_BYTE_CAP));
    file.take((SIDECAR_BYTE_CAP + 1) as u64)
        .read_to_end(&mut bytes)
        .await
        .map_err(|_| "unreadable".to_string())?;
    if bytes.len() > SIDECAR_BYTE_CAP {
        return Err("size_limit_exceeded".to_string());
    }
    let after = tokio::fs::symlink_metadata(path)
        .await
        .map_err(|_| "unreadable".to_string())?;
    if FileIdentity::from_metadata(&after).map_err(|_| "unreadable".to_string())? != identity {
        return Err("file_changed".to_string());
    }
    let codec = codec.to_string();
    tokio::task::spawn_blocking(move || {
        let text = decode_subtitle_text(&bytes)?;
        validate_subtitle_text(&text, &codec)
    })
    .await
    .map_err(|_| "validation_unavailable".to_string())?
}

fn decode_subtitle_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        if !bytes.len().is_multiple_of(2) {
            return Err("invalid_encoding".to_string());
        }
        let little = bytes.starts_with(&[0xff, 0xfe]);
        let units: Vec<u16> = bytes[2..]
            .chunks_exact(2)
            .map(|pair| {
                if little {
                    u16::from_le_bytes([pair[0], pair[1]])
                } else {
                    u16::from_be_bytes([pair[0], pair[1]])
                }
            })
            .collect();
        String::from_utf16(&units).map_err(|_| "invalid_encoding".to_string())
    } else {
        std::str::from_utf8(bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes))
            .map(str::to_string)
            .map_err(|_| "invalid_encoding".to_string())
    }
}

/// Check for actual non-empty timed text cues. No content-language claim
/// is made, and legacy/non-UTF encodings remain explicitly unverified.
fn validate_subtitle_text(text: &str, codec: &str) -> Result<usize, String> {
    if text.trim().is_empty() {
        return Err("empty_subtitle".to_string());
    }
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let cues = match codec {
        "srt" => normalized.split("\n\n").filter(|block| {
            let lines: Vec<&str> = block.lines().collect();
            let Some(timing) = lines.iter().position(|line| line.contains("-->")) else { return false; };
            let Some((start, end)) = lines[timing].split_once("-->") else { return false; };
            let end = end.split_whitespace().next().unwrap_or_default();
            matches!((timestamp(start.trim()), timestamp(end)), (Some(start), Some(end)) if end > start)
                && lines[timing + 1..].iter().any(|line| visible_text(line))
        }).count(),
        "ass" | "ssa" => {
            let mut in_events = false;
            let mut format: Vec<String> = Vec::new();
            let mut cues = 0;
            for line in normalized.lines().map(str::trim) {
                if line.starts_with('[') { in_events = line.eq_ignore_ascii_case("[events]"); continue; }
                if !in_events { continue; }
                if let Some((name, rest)) = line.split_once(':') {
                    if name.eq_ignore_ascii_case("format") { format = rest.split(',').map(|s| s.trim().to_ascii_lowercase()).collect(); }
                    if name.eq_ignore_ascii_case("dialogue") && !format.is_empty() {
                        let values: Vec<&str> = rest.splitn(format.len(), ',').collect();
                        let field = |key: &str| format.iter().position(|name| name == key).and_then(|index| values.get(index).copied());
                        if matches!((field("start").and_then(timestamp), field("end").and_then(timestamp)), (Some(start), Some(end)) if end > start)
                            && field("text").is_some_and(visible_text) { cues += 1; }
                    }
                }
            }
            cues
        }
        _ => return Err("unsupported_format".to_string()),
    };
    if cues == 0 {
        Err("no_valid_timed_text".to_string())
    } else {
        Ok(cues)
    }
}

fn timestamp(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let mut parts = raw.split(':');
    let hours: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let seconds = parts.next()?;
    if parts.next().is_some() || minutes >= 60 {
        return None;
    }
    let (seconds, fraction) = seconds.split_once([',', '.'])?;
    let seconds: u64 = seconds.parse().ok()?;
    if seconds >= 60
        || fraction.is_empty()
        || fraction.len() > 3
        || !fraction.chars().all(|c| c.is_ascii_digit())
    {
        return None;
    }
    let millis: u64 = fraction.parse::<u64>().ok()? * 10_u64.pow(3 - fraction.len() as u32);
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes * 60_000)?
        .checked_add(seconds * 1_000)?
        .checked_add(millis)
}

fn visible_text(text: &str) -> bool {
    let mut braces = false;
    let mut html = false;
    let mut visible = String::new();
    for c in text.chars() {
        match c {
            '{' => braces = true,
            '}' => braces = false,
            '<' if !braces => html = true,
            '>' if !braces => html = false,
            _ if !braces && !html => visible.push(c),
            _ => {}
        }
    }
    let visible = visible
        .replace("\\N", "")
        .replace("\\n", "")
        .replace("\\h", "")
        .replace("&nbsp;", "");
    visible
        .chars()
        .any(|c| !c.is_whitespace() && !c.is_control())
}

/// Evaluate declared full-dialogue languages, keeping unknown/missing
/// facts explicit. Empty requirements disable this check for upgrades.
pub fn evaluate_subtitle_requirements(
    info: &MediaInfo,
    required: &[String],
    match_mode: &str,
) -> SubtitleRequirementResult {
    let mode = if match_mode == "any" { "any" } else { "all" };
    let mut languages = BTreeSet::new();
    let mut invalid = BTreeSet::new();
    for raw in required.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        match normalize_language(raw) {
            Some(language) => {
                languages.insert(language);
            }
            None => {
                invalid.insert(raw.to_string());
            }
        }
    }
    let available: BTreeSet<String> = info
        .subtitles
        .iter()
        .filter(|s| s.valid && !s.forced && !s.signs_only)
        .filter_map(|s| s.language.clone())
        .collect();
    let mut missing: Vec<String> = languages.difference(&available).cloned().collect();
    missing.extend(invalid.iter().cloned());
    let enabled = !languages.is_empty() || !invalid.is_empty();
    let satisfied = !enabled
        || (invalid.is_empty()
            && if mode == "any" {
                languages
                    .iter()
                    .any(|language| available.contains(language))
            } else {
                missing.is_empty()
            });
    let mut reasons = Vec::new();
    if !invalid.is_empty() {
        reasons.push("Some required language codes are unrecognised".to_string());
    }
    if info.subtitles.iter().any(|s| s.language.is_none()) {
        reasons.push("Some subtitle tracks have an unknown declared language".to_string());
    }
    if info.subtitles.iter().any(|s| s.forced || s.signs_only) {
        reasons.push(
            "Forced and signs/songs-only tracks do not count as full dialogue subtitles"
                .to_string(),
        );
    }
    if info.subtitles.iter().any(|s| !s.valid) {
        reasons.push("Some subtitle files or tracks could not be validated".to_string());
    }
    if enabled && !satisfied {
        reasons.push(format!(
            "Required full dialogue subtitles are missing ({mode}: {})",
            missing.join(", ")
        ));
    }
    SubtitleRequirementResult {
        enabled,
        satisfied,
        match_mode: mode.to_string(),
        required_languages: languages.into_iter().chain(invalid).collect(),
        available_languages: available.into_iter().collect(),
        missing_languages: missing,
        reasons,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn identity() -> FileIdentity {
        FileIdentity {
            size: 100,
            mtime_ns: "123000000001".to_string(),
            device: Some(1),
            inode: Some(2),
        }
    }

    fn info_with(subtitles: Vec<Value>) -> MediaInfo {
        let mut streams = vec![
            json!({"index":0,"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,"pix_fmt":"yuv420p10le"}),
        ];
        streams.extend(subtitles);
        parse_probe_json(&json!({"streams":streams,"format":{"format_name":"matroska,webm","duration":"1440.5"}}).to_string(), identity(), false).unwrap()
    }

    fn sub(language: &str, title: &str) -> Value {
        json!({"index":2,"codec_type":"subtitle","codec_name":"ass","tags":{"language":language,"title":title},"disposition":{"default":1,"forced":0}})
    }

    #[test]
    fn file_facts_preserve_tracks_dispositions_and_declared_language() {
        let info = parse_probe_json(&json!({
            "streams":[
                {"index":0,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},
                {"index":1,"codec_type":"video","codec_name":"hevc","width":1920,"height":1080,"pix_fmt":"yuv420p10le"},
                {"index":2,"codec_type":"audio","codec_name":"aac","channels":2,"channel_layout":"stereo","sample_rate":"48000","tags":{"LANGUAGE":"jpn"},"disposition":{"default":1}},
                sub("eng", "Full dialogue"),
                {"index":4,"codec_type":"subtitle","codec_name":"subrip","tags":{"language":"und"},"disposition":{"forced":1}}
            ],"format":{"format_name":"matroska,webm","duration":"1440.5"}
        }).to_string(), identity(), true).unwrap();
        assert_eq!(info.video.len(), 1);
        assert_eq!(info.video[0].bit_depth, Some(10));
        assert_eq!(info.audio[0].language.as_deref(), Some("ja"));
        assert_eq!(info.audio[0].sample_rate, Some(48000));
        assert!(info.audio[0].default);
        assert_eq!(info.subtitles[0].language.as_deref(), Some("en"));
        assert_eq!(info.subtitles[0].raw_language.as_deref(), Some("eng"));
        assert_eq!(
            info.subtitles[0].validation_status,
            "container_track_present"
        );
        assert!(info.subtitles[1].forced);
        assert!(info.subtitles[1].language.is_none());
        assert_eq!(info.duration_seconds, Some(1440.5));
        assert!(info.cache_hit);
    }

    #[test]
    fn no_subtitles_is_a_successful_observation_but_no_video_is_not() {
        assert!(info_with(Vec::new()).subtitles.is_empty());
        assert!(parse_probe_json("{}", identity(), false).is_err());
        assert!(
            parse_probe_json(r#"{"streams":[{"codec_type":"audio"}]}"#, identity(), false).is_err()
        );
    }

    #[test]
    fn language_aliases_and_regions_normalize_without_guessing_unknowns() {
        for (raw, expected) in [
            (" eng ", "en"),
            ("English", "en"),
            ("deu", "de"),
            ("ger", "de"),
            ("DE-de", "de"),
            ("en_US", "en"),
            ("es-419", "es"),
            ("fil", "tl"),
        ] {
            assert_eq!(normalize_language(raw).as_deref(), Some(expected));
        }
        for raw in ["", "und", "mul", "zxx", "unknown", "xx", "nonsense"] {
            assert!(normalize_language(raw).is_none(), "{raw}");
        }
    }

    #[test]
    fn subtitle_requirements_support_all_any_and_disabled() {
        let info = info_with(vec![sub("eng", "Full dialogue")]);
        assert!(evaluate_subtitle_requirements(&info, &[], "all").satisfied);
        assert!(!evaluate_subtitle_requirements(&info, &[], "all").enabled);
        let required = vec!["en".to_string(), "deu".to_string()];
        let all = evaluate_subtitle_requirements(&info, &required, "all");
        assert!(!all.satisfied);
        assert_eq!(all.missing_languages, vec!["de"]);
        assert!(evaluate_subtitle_requirements(&info, &required, "any").satisfied);
        assert!(
            evaluate_subtitle_requirements(&info, &["ENG".to_string(), "en-US".to_string()], "all")
                .satisfied
        );
        assert!(!evaluate_subtitle_requirements(&info, &["xx".to_string()], "any").satisfied);
    }

    #[test]
    fn forced_signs_unknown_and_known_empty_tracks_do_not_satisfy_dialogue() {
        let mut forced = sub("eng", "English");
        forced["disposition"]["forced"] = json!(1);
        let mut empty = sub("eng", "Full");
        empty["nb_read_packets"] = json!("0");
        let info = info_with(vec![
            forced,
            sub("en", "Signs & Songs"),
            sub("und", "English"),
            empty,
        ]);
        let result = evaluate_subtitle_requirements(&info, &["en".to_string()], "all");
        assert!(!result.satisfied);
        assert!(result.available_languages.is_empty());
        assert_eq!(info.subtitles[3].validation_status, "empty_track");
        assert!(
            evaluate_subtitle_requirements(
                &info_with(vec![sub("en", "Full dialogue, signs and songs")]),
                &["en".to_string()],
                "all"
            )
            .satisfied
        );
    }

    #[test]
    fn timed_text_validation_rejects_empty_garbage_and_style_only_files() {
        assert_eq!(
            validate_subtitle_text("1\n00:00:01,000 --> 00:00:02,000\nHello!\n", "srt"),
            Ok(1)
        );
        assert!(validate_subtitle_text("", "srt").is_err());
        assert!(validate_subtitle_text("Downloaded by website", "srt").is_err());
        assert!(validate_subtitle_text("1\n00:00:02,000 --> 00:00:01,000\nHello", "srt").is_err());
        assert!(
            validate_subtitle_text("1\n00:00:01,000 --> 00:00:02,000\n<i> </i>", "srt").is_err()
        );
        let ass = "[Script Info]\nTitle: Test\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.50,Default,,0,0,0,,{\\i1}Hello, world!";
        assert_eq!(validate_subtitle_text(ass, "ass"), Ok(1));
        assert!(validate_subtitle_text("[Script Info]\nTitle: Empty\n[Events]\n", "ass").is_err());
        assert!(
            validate_subtitle_text(
                "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,{\\i1}\\N",
                "ass"
            )
            .is_err()
        );
    }

    #[tokio::test]
    async fn cached_video_still_checks_live_sidecar_contents_and_matching() {
        let db = crate::test_support::in_memory_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let video = tmp.path().join("Show - S01E06.mkv");
        tokio::fs::write(&video, b"fixture").await.unwrap();
        let canonical = tokio::fs::canonicalize(&video).await.unwrap();
        let id =
            FileIdentity::from_metadata(&tokio::fs::metadata(&canonical).await.unwrap()).unwrap();
        let json = json!({"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1280,"height":720}]}).to_string();
        crate::models::media_probe_cache::upsert_identified(
            &db,
            canonical.to_str().unwrap(),
            &id.cache_key(),
            id.size as i64,
            &json,
        )
        .await;
        let valid = "1\n00:00:01,000 --> 00:00:02,000\nHello\n";
        let english = tmp.path().join("Show - S01E06.en.srt");
        let german = tmp.path().join("Show - S01E06.de.forced.ass");
        let other = tmp.path().join("Show - S01E01.en.srt");
        tokio::fs::write(&english, valid).await.unwrap();
        tokio::fs::write(
            &german,
            "[Events]\nFormat: Start, End, Text\nDialogue: 0:00:01.00,0:00:02.00,Hallo",
        )
        .await
        .unwrap();
        tokio::fs::write(&other, valid).await.unwrap();
        let info = inspect(&db, &video).await.unwrap();
        assert!(info.cache_hit);
        assert_eq!(info.subtitles.len(), 2);
        assert!(evaluate_subtitle_requirements(&info, &["en".to_string()], "all").satisfied);
        assert!(!evaluate_subtitle_requirements(&info, &["de".to_string()], "all").satisfied);
        tokio::fs::write(&english, "empty").await.unwrap();
        let info = inspect(&db, &video).await.unwrap();
        assert!(info.cache_hit);
        assert!(!evaluate_subtitle_requirements(&info, &["en".to_string()], "all").satisfied);
        assert!(
            info.subtitles
                .iter()
                .any(|s| s.validation_status == "no_valid_timed_text")
        );
        tokio::fs::remove_file(&english).await.unwrap();
        let info = inspect(&db, &video).await.unwrap();
        assert_eq!(info.subtitles.len(), 1);
        assert!(!evaluate_subtitle_requirements(&info, &["en".to_string()], "any").satisfied);
    }

    #[tokio::test]
    async fn external_matching_uses_importer_pack_rules_and_preserves_labels() {
        let db = crate::test_support::in_memory_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let video = tmp.path().join("Show - 06.mkv");
        tokio::fs::write(&video, b"fixture").await.unwrap();
        let canonical = tokio::fs::canonicalize(&video).await.unwrap();
        let id =
            FileIdentity::from_metadata(&tokio::fs::metadata(&canonical).await.unwrap()).unwrap();
        crate::models::media_probe_cache::upsert_identified(
            &db,
            canonical.to_str().unwrap(),
            &id.cache_key(),
            id.size as i64,
            r#"{"streams":[{"codec_type":"video","codec_name":"h264"}]}"#,
        )
        .await;
        let subs = tmp.path().join("Subs").join("Show - 06");
        tokio::fs::create_dir_all(&subs).await.unwrap();
        let sidecar = subs.join("English.sdh.srt");
        tokio::fs::write(&sidecar, "1\n00:00:01,000 --> 00:00:02,000\nHello\n")
            .await
            .unwrap();
        let info = inspect_with_sidecars(
            &db,
            &video,
            &[sidecar],
            Some(EpisodeSpan::single(None, 6)),
            12,
        )
        .await
        .unwrap();
        assert_eq!(info.subtitles.len(), 1);
        assert!(info.subtitles[0].hearing_impaired);
        assert_eq!(info.subtitles[0].language.as_deref(), Some("en"));
        assert_eq!(info.subtitles[0].cue_count, Some(1));
    }

    #[tokio::test]
    async fn stream_links_and_missing_files_return_explicit_errors() {
        let db = crate::test_support::in_memory_pool().await;
        let tmp = tempfile::tempdir().unwrap();
        let stream = tmp.path().join("Show.strm");
        tokio::fs::write(&stream, "https://example.invalid/video.mkv")
            .await
            .unwrap();
        assert_eq!(inspect(&db, &stream).await.unwrap_err().code, "stream_stub");
        assert_eq!(
            inspect(&db, &tmp.path().join("missing.mkv"))
                .await
                .unwrap_err()
                .code,
            "stat_failed"
        );
    }
}
