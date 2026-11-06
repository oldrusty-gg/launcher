//! One-click updates of the program files, in place.
//!
//! At start the launcher asks GitHub's public API for the latest release
//! ([`check`]). A release that can update the launcher carries a small asset,
//! `latest.json` (`scripts/release/write-latest.ps1`):
//! `{"version": "2026-11-07", "zip": "roses-2026-11-07.zip", "size": 1, "sha256": "..."}`.
//! A release without it (2026-11-06b and older) only keeps the link to the
//! release page. Clicking Update downloads the zip, checks its size and
//! SHA-256 and unpacks it into [`STAGING_DIR`] beside the launcher
//! ([`prepare`]); a copy of the running launcher then swaps the files and
//! starts the launcher again ([`swap`]).
//!
//! Until the swap starts the program files are not touched. `roses-data` is
//! never touched: a package holding that folder is refused. Files of the old
//! version that the new zip does not hold stay where they are.
//!
//! The checksum proves the zip is the file the release published; it comes
//! from the same release, so it is no signature: it protects against broken
//! downloads, and https to GitHub protects the rest.

pub mod http;
pub mod swap;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

/// The release asset holding the version and the checksum.
pub const MANIFEST_ASSET: &str = "latest.json";
/// The working folder beside the launcher (same disk, so moves are renames).
pub const STAGING_DIR: &str = "roses-update-staging";
/// The player's imports, saves, logs and session: never written.
pub const DATA_DIR: &str = "roses-data";
pub const LAUNCHER_EXE: &str = "roses-launcher.exe";
/// The API host, the one `platform.json` may name.
pub const API_HOST: &str = "api.github.com";
/// Where GitHub redirects a release asset (the second is its older name).
pub const ASSET_HOSTS: [&str; 2] = ["release-assets.githubusercontent.com", "objects.githubusercontent.com"];

const JSON_ACCEPT: &str = "application/vnd.github+json";
const FILE_ACCEPT: &str = "application/octet-stream";
const MAX_RELEASE_JSON: u64 = 1024 * 1024;
const MAX_MANIFEST: u64 = 64 * 1024;
const MAX_ZIP: u64 = 1 << 30;
const MAX_UNPACKED: u64 = 4 << 30;
const MAX_ENTRIES: usize = 10_000;

/// A release label: `YYYY-MM-DD` and optional lower-case letters
/// (`2026-11-06b`). Later dates are newer; on one date no letter comes first,
/// then `a`..`z`, then `aa`.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    date: (u32, u32, u32),
    letters_len: usize,
    letters: String,
}

impl Version {
    pub fn parse(text: &str) -> Option<Self> {
        let bytes = text.as_bytes();
        if bytes.len() < 10 || bytes[4] != b'-' || bytes[7] != b'-' {
            return None;
        }
        let number = |range: std::ops::Range<usize>| -> Option<u32> {
            let part = text.get(range)?;
            part.bytes().all(|b| b.is_ascii_digit()).then(|| part.parse().ok())?
        };
        let date = (number(0..4)?, number(5..7)?, number(8..10)?);
        let letters = text.get(10..)?;
        if !(1..=12).contains(&date.1) || !(1..=31).contains(&date.2) || !letters.bytes().all(|b| b.is_ascii_lowercase()) {
            return None;
        }
        Some(Self { date, letters_len: letters.len(), letters: letters.to_owned() })
    }
}

/// Whether `latest` is a newer release than `current`. A label that cannot be
/// read is never newer: no button rather than a wrong one.
pub fn is_newer(latest: &str, current: &str) -> bool {
    matches!((Version::parse(latest), Version::parse(current)), (Some(l), Some(c)) if l > c)
}

/// What may be fetched. Shipped builds: https to GitHub only.
#[derive(Clone, Copy, Debug)]
pub struct Policy {
    /// The `update-test-seam` feature (never in a release): also plain http
    /// to 127.0.0.1, the fake release of `tests/update_e2e.rs`.
    loopback_http: bool,
}

impl Policy {
    pub fn for_build() -> Self {
        Self { loopback_http: cfg!(feature = "update-test-seam") }
    }

    /// Whether `platform.json` may name `url` as the API address.
    pub fn allows_api(&self, url: &str) -> bool {
        http::Target::parse(url).is_some_and(|t| (t.tls && t.host == API_HOST && t.port == 443) || (self.loopback(&t)))
    }

    fn loopback(&self, target: &http::Target) -> bool {
        self.loopback_http && !target.tls && target.host == "127.0.0.1"
    }

    fn allows(&self, target: &http::Target) -> bool {
        let github = target.tls && target.port == 443 && (target.host == API_HOST || ASSET_HOSTS.contains(&target.host.as_str()));
        github || self.loopback(target)
    }
}

/// With the `update-test-seam` feature: the window stays hidden.
pub fn test_hidden() -> bool {
    cfg!(feature = "update-test-seam") && std::env::var_os("ROSES_UPDATE_TEST_HIDDEN").is_some()
}

/// With the `update-test-seam` feature: the Yes of the confirmation.
pub fn test_confirmed() -> bool {
    test_hidden()
}

#[derive(Debug, Deserialize)]
struct GitHubRelease {
    tag_name: String,
    #[serde(default)]
    assets: Vec<GitHubAsset>,
}

#[derive(Debug, Deserialize)]
struct GitHubAsset {
    name: String,
    /// The API address of the asset (`Accept: application/octet-stream`
    /// redirects to its file).
    url: String,
    size: u64,
}

/// `latest.json`.
#[derive(Debug, Deserialize)]
struct Manifest {
    version: String,
    zip: String,
    size: u64,
    sha256: String,
}

/// A release the launcher can update to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Available {
    pub version: String,
    pub zip_url: String,
    pub size: u64,
    pub sha256: [u8; 32],
}

/// What the check at start found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Check {
    UpToDate,
    /// A newer release without a usable `latest.json`: the release page only.
    NoUpdateFile { tag: String, why: String },
    Available(Available),
}

/// What the background jobs tell the window.
#[derive(Debug)]
pub enum UpdateEvent {
    Checked(Result<Check, String>),
    /// Bytes downloaded of the total.
    Progress(u64, u64),
    Prepared(Result<(), String>),
}

fn refused(status: u16) -> String {
    match status {
        403 | 429 => "GitHub is limiting requests from this connection for now; try again in an hour.".into(),
        404 => "GitHub does not have that release.".into(),
        _ => format!("GitHub answered with an error ({status})."),
    }
}

fn fetch_small(url: &str, accept: &str, limit: u64, policy: &Policy) -> Result<Vec<u8>, String> {
    let response = http::get(url, accept, policy)?;
    if response.status != 200 {
        return Err(refused(response.status));
    }
    let mut body = Vec::new();
    response.take(limit + 1).read_to_end(&mut body).map_err(|e| format!("The answer from GitHub stopped early ({e})."))?;
    if body.len() as u64 > limit {
        return Err("The answer from GitHub is too large.".into());
    }
    Ok(body)
}

/// Ask for the latest release; `current` is this build's label.
pub fn check(api_url: &str, current: &str, policy: &Policy) -> Result<Check, String> {
    let body = fetch_small(api_url, JSON_ACCEPT, MAX_RELEASE_JSON, policy)?;
    let release: GitHubRelease = serde_json::from_slice(&body).map_err(|e| format!("GitHub's answer could not be read ({e})."))?;
    if !is_newer(&release.tag_name, current) {
        return Ok(Check::UpToDate);
    }
    let tag = release.tag_name.clone();
    let unusable = |why: String| Ok(Check::NoUpdateFile { tag: tag.clone(), why });
    let Some(asset) = release.assets.iter().find(|a| a.name == MANIFEST_ASSET) else {
        return unusable(format!("it has no {MANIFEST_ASSET}"));
    };
    let body = fetch_small(&asset.url, FILE_ACCEPT, MAX_MANIFEST, policy)?;
    let manifest: Manifest = match serde_json::from_slice(&body) {
        Ok(manifest) => manifest,
        Err(e) => return unusable(format!("its {MANIFEST_ASSET} could not be read ({e})")),
    };
    // A build's label is `version`: with any other tag it would be offered
    // the same update again after installing it.
    if manifest.version != release.tag_name {
        return unusable(format!("its {MANIFEST_ASSET} names version {}", manifest.version));
    }
    let Some(zip) = release.assets.iter().find(|a| a.name == manifest.zip) else {
        return unusable(format!("it has no {}", manifest.zip));
    };
    if zip.size != manifest.size || manifest.size == 0 || manifest.size > MAX_ZIP {
        return unusable(format!("{} has the wrong size", manifest.zip));
    }
    let Ok(sha256) = platform_net::tls::parse_fingerprint(&manifest.sha256) else {
        return unusable(format!("its {MANIFEST_ASSET} has no usable checksum"));
    };
    Ok(Check::Available(Available { version: manifest.version, zip_url: zip.url.clone(), size: manifest.size, sha256 }))
}

/// Download `available` to `to`, checking size and SHA-256. A `to` that fails
/// is left for the caller to remove.
pub fn download(available: &Available, to: &Path, policy: &Policy, progress: &mut dyn FnMut(u64, u64)) -> Result<(), String> {
    let mut response = http::get(&available.zip_url, FILE_ACCEPT, policy)?;
    if response.status != 200 {
        return Err(refused(response.status));
    }
    if response.length.is_some_and(|length| length != available.size) {
        return Err("The download is not the size the release says.".into());
    }
    let mut file = File::create(to).map_err(|e| format!("Could not save the download ({e})."))?;
    let mut hash = Sha256::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut done = 0;
    loop {
        let read = match response.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(format!("The download stopped before it finished ({e}).")),
        };
        done += read as u64;
        if done > available.size {
            return Err("The download is larger than the release says.".into());
        }
        hash.update(&buffer[..read]);
        file.write_all(&buffer[..read]).map_err(|e| format!("Could not save the download; is the disk full? ({e})"))?;
        progress(done, available.size);
    }
    if done != available.size {
        return Err("The download stopped before it finished.".into());
    }
    file.sync_all().map_err(|e| format!("Could not save the download ({e})."))?;
    if <[u8; 32]>::from(hash.finalize()) != available.sha256 {
        return Err("The download is damaged: its checksum does not match the release.".into());
    }
    Ok(())
}

/// One path of a zip as safe components; Compress-Archive of Windows
/// PowerShell writes `\`. A trailing separator is a folder: `None`.
fn entry_path(name: &str) -> Result<Option<Vec<String>>, String> {
    let name = name.replace('\\', "/");
    if name.ends_with('/') {
        return Ok(None);
    }
    let parts: Vec<String> = name.split('/').map(str::to_owned).collect();
    for part in &parts {
        // Windows drops trailing dots and spaces, so `roses-data.` is
        // `roses-data`; a colon is a drive or a hidden stream.
        let bad = part.is_empty()
            || part == "."
            || part == ".."
            || part.ends_with(['.', ' '])
            || part.chars().any(|c| c.is_control() || r#":*?"<>|"#.contains(c));
        if bad {
            return Err(format!("The package holds an unsafe path: {name}"));
        }
    }
    Ok(Some(parts))
}

/// Where each file of a package goes: the shared top folder (`roses-<date>/`)
/// removed, one launcher at the root, nothing in `roses-data`.
fn plan(names: &[Vec<String>]) -> Result<Vec<PathBuf>, String> {
    let top = names.first().map(|parts| parts[0].clone()).unwrap_or_default();
    let shared = names.iter().all(|parts| parts.len() > 1 && parts[0].eq_ignore_ascii_case(&top));
    let skip = usize::from(shared);
    let mut seen = BTreeSet::new();
    let mut launcher = false;
    let mut out = Vec::with_capacity(names.len());
    for parts in names {
        let parts = &parts[skip..];
        if parts[0].eq_ignore_ascii_case(DATA_DIR) || parts[0].eq_ignore_ascii_case(STAGING_DIR) {
            return Err(format!("The package holds a {} folder; it was refused.", parts[0]));
        }
        launcher |= parts.len() == 1 && parts[0].eq_ignore_ascii_case(LAUNCHER_EXE);
        if !seen.insert(parts.join("/").to_lowercase()) {
            return Err(format!("The package holds {} twice.", parts.join("/")));
        }
        out.push(parts.iter().collect());
    }
    if !launcher {
        return Err(format!("The package has no {LAUNCHER_EXE}."));
    }
    Ok(out)
}

/// Unpack `zip` into the empty folder `to`, without the versioned top folder.
pub fn unpack(zip: &Path, to: &Path) -> Result<(), String> {
    let damaged = |e: &dyn std::fmt::Display| format!("The package could not be unpacked; it is damaged ({e}).");
    let file = File::open(zip).map_err(|e| damaged(&e))?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| damaged(&e))?;
    if archive.len() > MAX_ENTRIES {
        return Err("The package holds too many files.".into());
    }
    let mut indices = Vec::new();
    let mut names = Vec::new();
    for index in 0..archive.len() {
        let entry = archive.by_index_raw(index).map_err(|e| damaged(&e))?;
        if let Some(parts) = entry_path(entry.name())? {
            indices.push(index);
            names.push(parts);
        }
    }
    let paths = plan(&names)?;
    let mut left = MAX_UNPACKED;
    for (index, path) in indices.into_iter().zip(paths) {
        let target = to.join(&path);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(|e| format!("Could not unpack the update ({e})."))?;
        }
        let entry = archive.by_index(index).map_err(|e| damaged(&e))?;
        let mut out = File::create(&target).map_err(|e| format!("Could not unpack the update ({e})."))?;
        // Reading to the end also checks the entry's CRC.
        let written = io::copy(&mut entry.take(left + 1), &mut out).map_err(|e| damaged(&e))?;
        if written > left {
            return Err("The package unpacks to too much data.".into());
        }
        left -= written;
    }
    Ok(())
}

/// Download, check and unpack into the staging folder. On failure the
/// staging folder is gone again and the program files are as they were.
pub fn prepare(available: &Available, install: &Path, policy: &Policy, progress: &mut dyn FnMut(u64, u64)) -> Result<(), String> {
    let staging = swap::Staging::of(install);
    let result = (|| {
        staging.remove();
        fs::create_dir_all(staging.new_dir()).map_err(|e| format!("Could not make the update folder beside the launcher ({e})."))?;
        let zip = staging.root.join("download.zip");
        download(available, &zip, policy, progress)?;
        unpack(&zip, &staging.new_dir())?;
        let _ = fs::remove_file(&zip);
        Ok(())
    })();
    if result.is_err() {
        staging.remove();
    }
    result
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::Policy;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    pub fn seam() -> Policy {
        Policy { loopback_http: true }
    }

    /// A fresh folder under the system temp folder.
    pub fn folder(name: &str) -> PathBuf {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!("roses-updater-{}-{}-{name}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// A server that answers each connection with `answer(path)` raw bytes.
    /// Returns its base address.
    pub fn serve(answer: impl Fn(&str) -> Vec<u8> + Send + 'static) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream): Result<TcpStream, _> = stream else { return };
                let mut request = Vec::new();
                let mut byte = [0u8];
                while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
                    request.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&request).into_owned();
                let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
                let _ = stream.write_all(&answer(&path));
            }
        });
        base
    }

    pub fn ok(body: &[u8]) -> Vec<u8> {
        let mut out = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", body.len()).into_bytes();
        out.extend_from_slice(body);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use std::sync::{Arc, Mutex};
    use zip::write::SimpleFileOptions;

    #[test]
    fn versions_compare_by_date_then_letters() {
        assert!(is_newer("2026-11-07", "2026-11-06b"));
        assert!(is_newer("2026-11-06c", "2026-11-06b"));
        assert!(is_newer("2026-11-06a", "2026-11-06"));
        assert!(is_newer("2026-11-06aa", "2026-11-06z"));
        assert!(is_newer("2027-01-01", "2026-12-31z"));
        assert!(!is_newer("2026-11-06b", "2026-11-06b"));
        assert!(!is_newer("2026-11-06", "2026-11-06a"));
        assert!(!is_newer("2026-10-30", "2026-11-06"));
    }

    #[test]
    fn an_unreadable_version_is_never_newer() {
        for bad in ["", "latest", "v2026-11-07", "2026-13-01", "2026-11-32", "2026-11-07B", "2026-11-7", "2026-11-07-1", "２０２６-11-07"] {
            assert_eq!(Version::parse(bad), None, "{bad}");
            assert!(!is_newer(bad, "2026-11-06"), "{bad}");
            assert!(!is_newer("2026-11-07", bad), "{bad}");
        }
    }

    #[test]
    fn only_https_to_the_api_host_may_be_configured() {
        let shipped = Policy { loopback_http: false };
        assert!(shipped.allows_api("https://api.github.com/repos/oldrusty-gg/rust-yugi-dotr/releases/latest"));
        for bad in ["http://api.github.com/x", "https://example.org/x", "https://github.com/x", "https://api.github.com:444/x", "http://127.0.0.1:1/x", "https://user@api.github.com/x"] {
            assert!(!shipped.allows_api(bad), "{bad}");
        }
        assert!(seam().allows_api("http://127.0.0.1:1/x"));
        assert!(!seam().allows_api("http://example.org/x"));
    }

    /// A zip as Compress-Archive writes it: `\` and a versioned top folder.
    fn package(path: &Path, entries: &[(&str, &[u8])]) {
        let mut zip = zip::ZipWriter::new(File::create(path).unwrap());
        for (name, body) in entries {
            zip.start_file(*name, SimpleFileOptions::default()).unwrap();
            zip.write_all(body).unwrap();
        }
        zip.finish().unwrap();
    }

    fn unpacked(entries: &[(&str, &[u8])]) -> (PathBuf, Result<(), String>) {
        let dir = folder("unpack");
        package(&dir.join("p.zip"), entries);
        let out = dir.join("out");
        fs::create_dir(&out).unwrap();
        let result = unpack(&dir.join("p.zip"), &out);
        (out, result)
    }

    #[test]
    fn the_versioned_top_folder_is_removed() {
        let (out, result) = unpacked(&[
            ("roses-2026-11-07\\roses-launcher.exe", b"exe"),
            ("roses-2026-11-07\\assets\\", b""),
            ("roses-2026-11-07\\assets\\a.wgsl", b"shader"),
        ]);
        result.unwrap();
        assert_eq!(fs::read(out.join("roses-launcher.exe")).unwrap(), b"exe");
        assert_eq!(fs::read(out.join("assets").join("a.wgsl")).unwrap(), b"shader");
        assert!(!out.join("roses-2026-11-07").exists());
    }

    #[test]
    fn a_zip_without_a_top_folder_works_too() {
        let (out, result) = unpacked(&[("roses-launcher.exe", b"exe"), ("assets/a.wgsl", b"s")]);
        result.unwrap();
        assert!(out.join("assets").join("a.wgsl").is_file());
    }

    #[test]
    fn unsafe_packages_are_refused_and_write_nothing_outside() {
        for name in ["../evil.txt", "roses-1/../../evil.txt", "..\\evil.txt", "C:\\evil.txt", "/evil.txt", "a/b:stream", "a/evil.", "a/evil "] {
            let (out, result) = unpacked(&[("roses-1/roses-launcher.exe", b"exe"), (name, b"x")]);
            assert!(result.is_err(), "{name}");
            assert_eq!(fs::read_dir(&out).unwrap().count(), 0, "{name}: nothing is written before the whole list is checked");
            assert!(!out.parent().unwrap().join("evil.txt").exists());
        }
    }

    #[test]
    fn a_package_touching_the_data_folder_is_refused() {
        for name in ["r/roses-data/memory-card/x", "r/ROSES-DATA/x", "r/roses-update-staging/x"] {
            let (out, result) = unpacked(&[("r/roses-launcher.exe", b"exe"), (name, b"x")]);
            assert!(result.unwrap_err().contains("refused"), "{name}");
            assert_eq!(fs::read_dir(&out).unwrap().count(), 0);
        }
    }

    #[test]
    fn a_package_without_the_launcher_is_refused() {
        let (_, result) = unpacked(&[("r/roses.exe", b"exe")]);
        assert!(result.unwrap_err().contains(LAUNCHER_EXE));
        let (_, result) = unpacked(&[("r/sub/roses-launcher.exe", b"exe")]);
        assert!(result.is_err(), "the launcher must be at the root");
    }

    #[test]
    fn a_corrupt_zip_is_refused() {
        let dir = folder("corrupt");
        fs::write(dir.join("p.zip"), b"this is not a zip").unwrap();
        assert!(unpack(&dir.join("p.zip"), &dir).unwrap_err().contains("damaged"));
        // A real zip with damaged compressed data (the entry's CRC fails).
        let body = vec![7u8; 100_000];
        package(&dir.join("q.zip"), &[("r/roses-launcher.exe", &body)]);
        let mut bytes = fs::read(dir.join("q.zip")).unwrap();
        for byte in &mut bytes[60..80] {
            *byte ^= 0xff;
        }
        fs::write(dir.join("q.zip"), bytes).unwrap();
        let out = dir.join("out");
        fs::create_dir(&out).unwrap();
        assert!(unpack(&dir.join("q.zip"), &out).unwrap_err().contains("damaged"));
    }

    fn sha(body: &[u8]) -> [u8; 32] {
        Sha256::digest(body).into()
    }

    fn release_json(base: &str, tag: &str, assets: &[(&str, u64)]) -> Vec<u8> {
        let assets: Vec<_> = assets.iter().map(|(name, size)| serde_json::json!({"name": name, "size": size, "url": format!("{base}/asset/{name}")})).collect();
        serde_json::to_vec(&serde_json::json!({"tag_name": tag, "assets": assets, "ignored": 1})).unwrap()
    }

    /// A fake release; `/api` is the API address.
    fn fake(tag: &'static str, assets: &'static [(&'static str, u64)], manifest: &'static str) -> String {
        let base = Arc::new(Mutex::new(String::new()));
        let shared = base.clone();
        let url = serve(move |path| match path {
            "/api" => ok(&release_json(&shared.lock().unwrap(), tag, assets)),
            "/asset/latest.json" => ok(manifest.as_bytes()),
            _ => b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec(),
        });
        *base.lock().unwrap() = url.clone();
        url
    }

    const SHA: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[test]
    fn a_newer_release_with_its_file_is_offered() {
        let manifest: &'static str = Box::leak(format!(r#"{{"version":"2026-11-07","zip":"r.zip","size":5,"sha256":"{SHA}"}}"#).into_boxed_str());
        let url = fake("2026-11-07", &[("r.zip", 5), ("latest.json", 90)], manifest);
        let Check::Available(found) = check(&format!("{url}/api"), "2026-11-06b", &seam()).unwrap() else { panic!() };
        assert_eq!((found.version.as_str(), found.size, found.sha256), ("2026-11-07", 5, [0xaa; 32]));
        assert_eq!(found.zip_url, format!("{url}/asset/r.zip"));
        assert_eq!(check(&format!("{url}/api"), "2026-11-07", &seam()).unwrap(), Check::UpToDate);
    }

    #[test]
    fn today_s_release_without_the_file_only_keeps_the_link() {
        let url = fake("2026-11-06b", &[("roses-2026-11-06b.zip", 5)], "");
        assert!(matches!(check(&format!("{url}/api"), "2026-11-05", &seam()).unwrap(), Check::NoUpdateFile { .. }));
    }

    #[test]
    fn a_wrong_manifest_offers_nothing() {
        for (manifest, why) in [
            ("not json", "could not be read"),
            (r#"{"version":"2026-11-08","zip":"r.zip","size":5,"sha256":"aa"}"#, "names version"),
            (r#"{"version":"2026-11-07","zip":"other.zip","size":5,"sha256":"aa"}"#, "no other.zip"),
            (r#"{"version":"2026-11-07","zip":"r.zip","size":6,"sha256":"aa"}"#, "wrong size"),
            (r#"{"version":"2026-11-07","zip":"r.zip","size":5,"sha256":"aa"}"#, "checksum"),
        ] {
            let url = fake("2026-11-07", &[("r.zip", 5), ("latest.json", 90)], manifest);
            match check(&format!("{url}/api"), "2026-11-06b", &seam()).unwrap() {
                Check::NoUpdateFile { why: found, .. } => assert!(found.contains(why), "{found}"),
                other => panic!("{other:?}"),
            }
        }
    }

    #[test]
    fn a_rate_limit_and_an_unreachable_host_are_plain_errors() {
        let url = serve(|_| b"HTTP/1.1 403 rate limit exceeded\r\nContent-Length: 0\r\n\r\n".to_vec());
        assert!(check(&format!("{url}/api"), "2026-11-06", &seam()).unwrap_err().contains("limiting"));
        // A port nothing listens on.
        let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
        assert!(check(&format!("http://{free}/api"), "2026-11-06", &seam()).unwrap_err().contains("Could not reach"));
    }

    fn available(url: String, body: &[u8]) -> Available {
        Available { version: "2026-11-07".into(), zip_url: url, size: body.len() as u64, sha256: sha(body) }
    }

    #[test]
    fn a_good_download_reports_progress_and_passes() {
        let body = vec![3u8; 200_000];
        let served = body.clone();
        let url = serve(move |_| ok(&served));
        let dir = folder("download");
        let mut last = 0;
        download(&available(format!("{url}/z"), &body), &dir.join("z"), &seam(), &mut |done, total| {
            assert_eq!(total, 200_000);
            last = done;
        })
        .unwrap();
        assert_eq!(last, 200_000);
        assert_eq!(fs::read(dir.join("z")).unwrap(), body);
    }

    #[test]
    fn a_wrong_checksum_is_refused() {
        let url = serve(|_| ok(b"xxxxx"));
        let dir = folder("sha");
        let error = download(&available(format!("{url}/z"), b"yyyyy"), &dir.join("z"), &seam(), &mut |_, _| {}).unwrap_err();
        assert!(error.contains("checksum"), "{error}");
    }

    #[test]
    fn a_dropped_download_is_refused() {
        // Promises 10 bytes, sends 4 and closes.
        let url = serve(|_| b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nabcd".to_vec());
        let dir = folder("drop");
        let error = download(&available(format!("{url}/z"), b"abcdefghij"), &dir.join("z"), &seam(), &mut |_, _| {}).unwrap_err();
        assert!(error.contains("stopped"), "{error}");
    }

    #[test]
    fn a_failed_prepare_leaves_no_staging_folder() {
        let url = serve(|_| ok(b"xxxxx"));
        let install = folder("prepare");
        let result = prepare(&available(format!("{url}/z"), b"yyyyy"), &install, &seam(), &mut |_, _| {});
        assert!(result.is_err());
        assert!(!install.join(STAGING_DIR).exists());
    }

    #[test]
    fn prepare_stages_a_good_package() {
        let dir = folder("package");
        package(&dir.join("p.zip"), &[("roses-1\\roses-launcher.exe", b"new"), ("roses-1\\assets\\a.wgsl", b"s")]);
        let body = fs::read(dir.join("p.zip")).unwrap();
        let served = body.clone();
        let url = serve(move |_| ok(&served));
        let install = folder("install");
        prepare(&available(format!("{url}/z"), &body), &install, &seam(), &mut |_, _| {}).unwrap();
        let staged = install.join(STAGING_DIR).join("new");
        assert_eq!(fs::read(staged.join("roses-launcher.exe")).unwrap(), b"new");
        assert!(!install.join(STAGING_DIR).join("download.zip").exists());
    }

    /// `ROSES_REAL_SHA256=<the latest zip's sha256>`: the real API and the
    /// real release asset over TLS, through its redirect. Read-only.
    #[test]
    #[ignore = "reaches GitHub"]
    fn the_real_github_release_can_be_read_and_downloaded() {
        let policy = Policy { loopback_http: false };
        let api = "https://api.github.com/repos/oldrusty-gg/rust-yugi-dotr/releases/latest";
        let body = fetch_small(api, JSON_ACCEPT, MAX_RELEASE_JSON, &policy).unwrap();
        let release: GitHubRelease = serde_json::from_slice(&body).unwrap();
        println!("tag {}, {} assets", release.tag_name, release.assets.len());
        println!("check from 2026-11-05: {:?}", check(api, "2026-11-05", &policy).unwrap());
        assert_eq!(check(api, &release.tag_name, &policy).unwrap(), Check::UpToDate);
        let asset = release.assets.iter().find(|a| a.name.ends_with(".zip")).unwrap();
        let expected = std::env::var("ROSES_REAL_SHA256").unwrap();
        let sha256 = platform_net::tls::parse_fingerprint(&expected).unwrap();
        let wanted = Available { version: release.tag_name.clone(), zip_url: asset.url.clone(), size: asset.size, sha256 };
        let dir = folder("real");
        download(&wanted, &dir.join("real.zip"), &policy, &mut |_, _| {}).unwrap();
        let staged = dir.join("new");
        fs::create_dir(&staged).unwrap();
        unpack(&dir.join("real.zip"), &staged).unwrap();
        assert!(staged.join(LAUNCHER_EXE).is_file());
        assert!(!staged.join(format!("roses-{}", release.tag_name)).exists());
        let _ = fs::remove_dir_all(dir);
    }
}
