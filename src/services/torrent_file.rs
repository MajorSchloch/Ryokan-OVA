//! Reading a torrent's v1 info-hash from a magnet link or a `.torrent`
//! file, for grabs that arrive without one (an autobrr push whose
//! template had no `{{.InfoHash}}` to fill). Without a hash the grab
//! can only be found in the client by name, which a single-file
//! torrent named after its file never matches, so post-processing
//! marked it removed while it was still downloading.

use std::sync::LazyLock;
use std::time::Duration;

/// A real `.torrent` is well under this; a season pack with small
/// pieces reaches a few MB.
pub(crate) const TORRENT_BODY_CAP: usize = 10 << 20;

static HTTP_CLIENT: LazyLock<reqwest::Client> = LazyLock::new(|| {
    reqwest::Client::builder()
        .user_agent("Ryokan/0.1")
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::limited(5))
        .build()
        .expect("torrent-file HTTP client")
});

/// The info-hash a magnet link names (`xt=urn:btih:`, 40 hex or 32
/// base32 characters), as lowercase hex.
pub fn magnet_info_hash(uri: &str) -> Option<String> {
    let query = uri.strip_prefix("magnet:?")?;
    query.split('&').find_map(|pair| {
        let (key, value) = pair.split_once('=')?;
        if !key.eq_ignore_ascii_case("xt") {
            return None;
        }
        let hash = value
            .get(..9)
            .filter(|p| p.eq_ignore_ascii_case("urn:btih:"))
            .map(|_| &value[9..])?;
        match hash.len() {
            40 => super::download_client::normalize_info_hash(hash),
            32 => base32_to_hex(hash),
            _ => None,
        }
    })
}

fn base32_to_hex(s: &str) -> Option<String> {
    let mut bits: u64 = 0;
    let mut count = 0;
    let mut out = Vec::with_capacity(20);
    for c in s.bytes() {
        let v = match c.to_ascii_uppercase() {
            c @ b'A'..=b'Z' => c - b'A',
            c @ b'2'..=b'7' => c - b'2' + 26,
            _ => return None,
        };
        bits = (bits << 5) | u64::from(v);
        count += 5;
        if count >= 8 {
            count -= 8;
            out.push((bits >> count) as u8);
        }
    }
    (out.len() == 20).then(|| hex::encode(out))
}

/// The v1 info-hash of a `.torrent`: the SHA-1 of the top-level `info`
/// value's bencoded bytes. `None` for anything that isn't a dictionary
/// with an `info` dictionary (an NZB, an HTML error page).
pub fn torrent_info_hash(bytes: &[u8]) -> Option<String> {
    if bytes.first() != Some(&b'd') {
        return None;
    }
    let mut i = 1;
    while *bytes.get(i)? != b'e' {
        let (key, after_key) = bencode_string(bytes, i)?;
        let end = bencode_end(bytes, after_key, 0)?;
        if key == b"info" {
            if bytes[after_key] != b'd' {
                return None;
            }
            let mut hasher = sha1_smol::Sha1::new();
            hasher.update(&bytes[after_key..end]);
            return Some(hasher.digest().to_string());
        }
        i = end;
    }
    None
}

/// A byte string at `start`: its contents and the index past it.
fn bencode_string(bytes: &[u8], start: usize) -> Option<(&[u8], usize)> {
    let colon = start + bytes.get(start..)?.iter().position(|&b| b == b':')?;
    let len: usize = std::str::from_utf8(&bytes[start..colon])
        .ok()?
        .parse()
        .ok()?;
    let end = colon.checked_add(1)?.checked_add(len)?;
    Some((bytes.get(colon + 1..end)?, end))
}

/// The index just past the bencoded value at `start`. Depth-limited so
/// a hostile file can't exhaust the stack.
fn bencode_end(bytes: &[u8], start: usize, depth: u32) -> Option<usize> {
    if depth > 64 {
        return None;
    }
    match *bytes.get(start)? {
        b'd' | b'l' => {
            let mut i = start + 1;
            while *bytes.get(i)? != b'e' {
                i = bencode_end(bytes, i, depth + 1)?;
            }
            Some(i + 1)
        }
        b'i' => Some(start + bytes.get(start..)?.iter().position(|&b| b == b'e')? + 1),
        b'0'..=b'9' => bencode_string(bytes, start).map(|(_, end)| end),
        _ => None,
    }
}

/// Fetch a `.torrent` and read its info-hash. `None` on any failure;
/// the caller goes on without a hash, as before.
pub async fn fetch_torrent_info_hash(url: &str) -> Option<String> {
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return None;
    }
    let resp = HTTP_CLIENT.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let body = super::http_body::read_capped(resp, TORRENT_BODY_CAP)
        .await
        .ok()?;
    // A bencode walk and a SHA-1 over up to the cap: off the runtime.
    tokio::task::spawn_blocking(move || torrent_info_hash(&body))
        .await
        .ok()
        .flatten()
}

/// A single bounded read binds a public HTTP torrent to an exact native
/// identity. Do not let the daemon fetch the URL a second time. Private or
/// webseed-dependent sources need review rather than a lossy conversion.
pub(crate) async fn public_source_magnet(url: &str) -> Result<(String, String), String> {
    let response = HTTP_CLIENT
        .get(url)
        .send()
        .await
        .map_err(|_| "Source identity unavailable; no download started")?;
    if !response.status().is_success() {
        return Err("Source identity unavailable; no download started".into());
    }
    let body = super::http_body::read_capped(response, TORRENT_BODY_CAP)
        .await
        .map_err(|_| "Source identity unavailable; no download started")?;
    tokio::task::spawn_blocking(move || public_magnet_from_bytes(&body))
        .await
        .map_err(|e| e.to_string())?
}

fn fields(bytes: &[u8], start: usize) -> Option<Vec<(&[u8], usize, usize)>> {
    if bytes.get(start) != Some(&b'd') {
        return None;
    }
    let mut out = Vec::new();
    let mut pos = start + 1;
    while bytes.get(pos) != Some(&b'e') {
        let (key, value) = bencode_string(bytes, pos)?;
        let end = bencode_end(bytes, value, 0)?;
        out.push((key, value, end));
        pos = end;
        if out.len() > 4096 {
            return None;
        }
    }
    Some(out)
}
fn trackers(bytes: &[u8], pos: usize, depth: usize, out: &mut Vec<String>) -> Option<()> {
    if depth > 4 || out.len() >= 128 {
        return None;
    }
    if bytes.get(pos) == Some(&b'l') {
        let mut pos = pos + 1;
        while bytes.get(pos) != Some(&b'e') {
            trackers(bytes, pos, depth + 1, out)?;
            pos = bencode_end(bytes, pos, 0)?;
        }
    } else {
        let (raw, _) = bencode_string(bytes, pos)?;
        let tracker = std::str::from_utf8(raw).ok()?;
        if tracker.len() > 4096 {
            return None;
        }
        let parsed = reqwest::Url::parse(tracker).ok()?;
        if !matches!(parsed.scheme(), "http" | "https" | "udp" | "ws" | "wss") {
            return None;
        }
        out.push(tracker.to_owned());
    }
    Some(())
}
fn public_magnet_from_bytes(bytes: &[u8]) -> Result<(String, String), String> {
    let hash =
        torrent_info_hash(bytes).ok_or("Source identity unavailable; no download started")?;
    let fields = fields(bytes, 0).ok_or("Source metadata unavailable; review required")?;
    let mut announced = Vec::new();
    for (key, start, end) in fields {
        match key {
            b"info" => {
                let info = self::fields(bytes, start)
                    .ok_or("Source metadata unavailable; review required")?;
                if info
                    .iter()
                    .any(|(k, a, b)| *k == b"private" && &bytes[*a..*b] != b"i0e")
                {
                    return Err("Private HTTP torrent requires review; exact source dispatch cannot preserve its transport metadata".into());
                }
            }
            b"announce" | b"announce-list" => trackers(bytes, start, 0, &mut announced)
                .ok_or("Source tracker metadata cannot be preserved; review required")?,
            b"url-list" | b"httpseeds" if !matches!(&bytes[start..end], b"le" | b"0:") => {
                return Err("HTTP webseed source requires review; exact source dispatch cannot preserve its transport metadata".into());
            }
            _ => {}
        }
    }
    announced.sort();
    announced.dedup();
    let mut magnet =
        reqwest::Url::parse(&format!("magnet:?xt=urn:btih:{hash}")).map_err(|e| e.to_string())?;
    for tracker in announced {
        magnet.query_pairs_mut().append_pair("tr", &tracker);
    }
    Ok((hash, magnet.into()))
}

/// Send only the hash-bound dictionary during a native duplicate add. If
/// another client add won a race, qBittorrent cannot merge connection details
/// from the upload.
pub(crate) fn recovery_metainfo(bytes: &[u8], expected: &str) -> Result<Vec<u8>, String> {
    if bencode_end(bytes, 0, 0) != Some(bytes.len()) {
        return Err("Published torrent metadata is malformed.".into());
    }
    let (hash, _) = public_magnet_from_bytes(bytes)?;
    if !hash.eq_ignore_ascii_case(expected) {
        return Err("Published torrent metadata differs from the saved source identity.".into());
    }
    let fields = fields(bytes, 0).ok_or("Published torrent metadata is malformed.")?;
    let mut info = fields.into_iter().filter(|(key, _, _)| *key == b"info");
    let (_, start, end) = info
        .next()
        .ok_or("Published torrent metadata has no info dictionary.")?;
    if info.next().is_some() {
        return Err("Published torrent metadata has conflicting info dictionaries.".into());
    }
    let mut payload = Vec::with_capacity(end - start + 8);
    payload.extend_from_slice(b"d4:info");
    payload.extend_from_slice(&bytes[start..end]);
    payload.push(b'e');
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn public_http_source_retains_tracker_and_binds_one_canonical_identity() {
        let tracker = "https://tracker.test/announce?key=fixture";
        let body = format!(
            "d8:announce{}:{}4:infod6:lengthi5e4:name5:a.mkv12:piece lengthi16384e6:pieces0:ee",
            tracker.len(),
            tracker
        );
        let (hash, magnet) = public_magnet_from_bytes(body.as_bytes()).unwrap();
        assert_eq!(Some(hash.clone()), torrent_info_hash(body.as_bytes()));
        let parsed = reqwest::Url::parse(&magnet).unwrap();
        let pairs: Vec<_> = parsed.query_pairs().collect();
        assert_eq!(pairs.iter().filter(|(k, _)| k == "xt").count(), 1);
        assert!(
            pairs
                .iter()
                .any(|(k, v)| k == "xt" && v == &format!("urn:btih:{hash}"))
        );
        assert!(pairs.iter().any(|(k, v)| k == "tr" && v == tracker));
    }

    #[test]
    fn recovery_metadata_keeps_only_the_verified_raw_info_dictionary() {
        let info = "d6:lengthi5e4:name5:a.mkv12:piece lengthi16384e6:pieces0:e";
        let tracker = "http://localhost:9999/announce";
        let file = format!("d8:announce{}:{tracker}4:info{info}e", tracker.len());
        let hash = torrent_info_hash(file.as_bytes()).unwrap();
        let sanitized = recovery_metainfo(file.as_bytes(), &hash).unwrap();
        assert_eq!(sanitized, format!("d4:info{info}e").into_bytes());
        assert_eq!(
            torrent_info_hash(&sanitized).as_deref(),
            Some(hash.as_str())
        );
        assert!(recovery_metainfo(file.as_bytes(), &"0".repeat(40)).is_err());
        let duplicate = format!("d4:info{info}4:info{info}e");
        assert!(recovery_metainfo(duplicate.as_bytes(), &hash).is_err());
    }

    #[test]
    fn public_http_source_preserves_mixed_websocket_tracker_urls() {
        let trackers = [
            "http://tracker.test:7777/announce",
            "https://tracker.test/announce?key=a%2Fb&pass=x+y",
            "udp://tracker.test:1337/announce",
            "ws://tracker.test:8000/announce?key=fixture",
            "wss://tracker.openwebtorrent.com",
        ];
        let mut body = format!(
            "d8:announce{}:{}13:announce-listl",
            trackers[0].len(),
            trackers[0]
        );
        for tracker in trackers {
            body.push_str(&format!("l{}:{tracker}e", tracker.len()));
        }
        body.push_str("e4:infod6:lengthi5e4:name5:a.mkv12:piece lengthi16384e6:pieces0:ee");

        let (hash, magnet) = public_magnet_from_bytes(body.as_bytes()).unwrap();
        assert_eq!(Some(hash.clone()), torrent_info_hash(body.as_bytes()));
        let parsed = reqwest::Url::parse(&magnet).unwrap();
        let identities: Vec<_> = parsed
            .query_pairs()
            .filter(|(key, _)| key == "xt")
            .map(|(_, value)| value.into_owned())
            .collect();
        assert_eq!(identities, [format!("urn:btih:{hash}")]);
        let mut retained: Vec<_> = parsed
            .query_pairs()
            .filter(|(key, _)| key == "tr")
            .map(|(_, value)| value.into_owned())
            .collect();
        retained.sort();
        let mut expected = trackers.map(str::to_owned);
        expected.sort();
        assert_eq!(retained, expected);
    }

    #[test]
    fn public_http_source_still_holds_invalid_or_unsupported_tracker_urls() {
        for tracker in [
            "not a URL",
            "file:///tmp/announce",
            "ftp://tracker.test/announce",
        ] {
            let body = format!(
                "d8:announce{}:{}4:infod6:lengthi5e4:name5:a.mkvee",
                tracker.len(),
                tracker
            );
            assert!(
                public_magnet_from_bytes(body.as_bytes())
                    .unwrap_err()
                    .contains("Source tracker metadata cannot be preserved")
            );
        }
    }

    #[test]
    fn private_and_webseed_http_sources_hold_instead_of_losing_transport_metadata() {
        for body in [
            "d4:infod6:lengthi5e4:name5:a.mkv7:privatei1eee",
            "d8:url-list17:https://web.test/4:infod6:lengthi5e4:name5:a.mkvee",
        ] {
            assert!(
                public_magnet_from_bytes(body.as_bytes())
                    .unwrap_err()
                    .contains("requires review")
            );
        }
    }

    #[test]
    fn magnet_hashes_read_in_hex_and_base32() {
        let hex = "c12fe1c06bba254a9dc9f519b335aa7c1367a88a";
        assert_eq!(
            magnet_info_hash(&format!(
                "magnet:?dn=x&xt=urn:btih:{}&tr=y",
                hex.to_uppercase()
            )),
            Some(hex.to_string())
        );
        // The same hash in base32.
        assert_eq!(
            magnet_info_hash("magnet:?xt=urn:btih:YEX6DQDLXISUVHOJ6UM3GNNKPQJWPKEK"),
            Some(hex.to_string())
        );
        assert_eq!(magnet_info_hash("magnet:?xt=urn:btih:abc"), None);
        assert_eq!(magnet_info_hash("https://x/y.torrent"), None);
    }

    #[test]
    fn the_info_hash_is_the_top_level_info_dict_only() {
        let info = b"d6:lengthi5e4:name5:a.mkv12:piece lengthi16384e6:pieces0:e";
        // An `info` key inside another value (here, the announce URL's
        // text and a nested dict) must not be taken for the real one.
        let mut file = b"d8:announce15:http://x/4:info6:nested".to_vec();
        file.extend_from_slice(b"d4:info");
        file.extend_from_slice(b"d1:ai1eee");
        file.extend_from_slice(b"4:info");
        file.extend_from_slice(info);
        file.push(b'e');
        let mut hasher = sha1_smol::Sha1::new();
        hasher.update(info);
        assert_eq!(torrent_info_hash(&file), Some(hasher.digest().to_string()));

        assert_eq!(torrent_info_hash(b"<?xml version=\"1.0\"?><nzb/>"), None);
        assert_eq!(
            torrent_info_hash(b"d4:infoi1ee"),
            None,
            "info must be a dict"
        );
        assert_eq!(torrent_info_hash(b"d4:info"), None, "truncated");
        let deep = format!("d4:info{}{}e", "l".repeat(100), "e".repeat(100));
        assert_eq!(
            torrent_info_hash(deep.as_bytes()),
            None,
            "nesting is bounded"
        );
        assert_eq!(torrent_info_hash(b"d99999999999999999999:x"), None);
    }
}
