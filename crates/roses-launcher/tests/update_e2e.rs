//! End to end: real launcher exes, a fake release on 127.0.0.1 and a
//! throwaway install in `work/runs/updater/`. The window stays hidden
//! (`ROSES_UPDATE_TEST_HIDDEN`, which also answers the confirmation) and the
//! Update button is pressed with a posted BM_CLICK, as first-run.ps1 does.
//!
//! Build two launchers with the seam, `ROSES_RELEASE=2026-11-05` and
//! `ROSES_RELEASE=2026-11-07`, then:
//!   ROSES_UPDATE_E2E_OLD=<old.exe> ROSES_UPDATE_E2E_NEW=<new.exe>
//!   ROSES_UPDATE_E2E_WORK=<folder> cargo test -p roses-launcher --test update_e2e -- --ignored --test-threads=1
#![cfg(windows)]

use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::windows::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::{EnumWindows, FindWindowExW, GetWindowThreadProcessId, PostMessageW, BM_CLICK};

const NEW: &str = "2026-11-07";
const BUTTON: &str = "Update to 2026-11-07";

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("{name} is not set")))
}

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

/// Every file under `root` with its bytes' hash, optionally skipping a folder.
fn tree(root: &Path, skip: Option<&str>) -> BTreeMap<String, String> {
    fn walk(root: &Path, dir: &Path, skip: Option<&str>, out: &mut BTreeMap<String, String>) {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            let rel = path.strip_prefix(root).unwrap().to_string_lossy().replace('\\', "/");
            if skip.is_some_and(|s| rel == s) {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, skip, out);
            } else {
                out.insert(rel, format!("{:x}", Sha256::digest(fs::read(&path).unwrap())));
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, skip, &mut out);
    out
}

/// What the fake release serves.
#[derive(Clone)]
struct Release {
    api_status: u16,
    /// Serve `latest.json` as an asset.
    manifest: Option<Vec<u8>>,
    zip: Vec<u8>,
    /// Stop after this many bytes of the zip and close the connection.
    cut_at: Option<usize>,
}

struct Server {
    base: String,
    zip_requests: Arc<Mutex<u32>>,
    stop: Arc<AtomicBool>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.base.trim_start_matches("http://"));
    }
}

fn answer(stream: &mut TcpStream, release: &Release, base: &str, zip_requests: &Mutex<u32>) {
    let mut request = Vec::new();
    let mut byte = [0u8];
    while !request.ends_with(b"\r\n\r\n") && stream.read(&mut byte).is_ok_and(|n| n == 1) {
        request.push(byte[0]);
    }
    let text = String::from_utf8_lossy(&request).into_owned();
    let path = text.split_whitespace().nth(1).unwrap_or("/").to_owned();
    let head = |status: &str, length: usize| format!("HTTP/1.1 {status}\r\nContent-Length: {length}\r\n\r\n").into_bytes();
    let send = |stream: &mut TcpStream, body: &[u8]| {
        let _ = stream.write_all(&head("200 OK", body.len()));
        let _ = stream.write_all(body);
    };
    match path.as_str() {
        "/api" if release.api_status != 200 => {
            let _ = stream.write_all(&head(&format!("{} refused", release.api_status), 0));
        }
        "/api" => {
            let mut assets = vec![serde_json::json!({"name": format!("roses-{NEW}.zip"), "size": release.zip.len(), "url": format!("{base}/asset/zip")})];
            if let Some(manifest) = &release.manifest {
                assets.push(serde_json::json!({"name": "latest.json", "size": manifest.len(), "url": format!("{base}/asset/latest")}));
            }
            send(stream, &serde_json::to_vec(&serde_json::json!({"tag_name": NEW, "assets": assets})).unwrap());
        }
        "/asset/latest" => send(stream, release.manifest.as_deref().unwrap_or(b"")),
        // As on GitHub: the asset address redirects to the file.
        "/asset/zip" => {
            let _ = stream.write_all(format!("HTTP/1.1 302 Found\r\nLocation: {base}/file/zip\r\nContent-Length: 0\r\n\r\n").as_bytes());
        }
        "/file/zip" => {
            *zip_requests.lock().unwrap() += 1;
            let _ = stream.write_all(&head("200 OK", release.zip.len()));
            let body = &release.zip[..release.cut_at.unwrap_or(release.zip.len())];
            for chunk in body.chunks(256 * 1024) {
                if stream.write_all(chunk).is_err() {
                    break;
                }
                // Long enough for a progress bar to move.
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        _ => {
            let _ = stream.write_all(&head("404 Not Found", 0));
        }
    }
}

fn serve(release: Release) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let zip_requests = Arc::new(Mutex::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (address, requests, stopping) = (base.clone(), zip_requests.clone(), stop.clone());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            if stopping.load(Ordering::SeqCst) {
                return;
            }
            let Ok(mut stream) = stream else { return };
            let (release, address, requests) = (release.clone(), address.clone(), requests.clone());
            std::thread::spawn(move || answer(&mut stream, &release, &address, &requests));
        }
    });
    Server { base, zip_requests, stop }
}

/// The package of the new version: a real Compress-Archive zip, with the
/// versioned top folder, and latest.json from the real script.
struct Package {
    zip: Vec<u8>,
    manifest: Vec<u8>,
    new_files: BTreeMap<String, String>,
}

fn powershell(script: &str, arguments: &[&str]) {
    let status = Command::new("powershell").args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", script]).args(arguments).stdout(Stdio::null()).status().unwrap();
    assert!(status.success(), "{script}");
}

fn build_package(work: &Path) -> Package {
    let dir = work.join("package");
    let _ = fs::remove_dir_all(&dir);
    let root = dir.join(format!("roses-{NEW}"));
    fs::create_dir_all(root.join("assets").join("deep")).unwrap();
    fs::copy(env_path("ROSES_UPDATE_E2E_NEW"), root.join("roses-launcher.exe")).unwrap();
    fs::write(root.join("roses.exe"), "new importer").unwrap();
    fs::write(root.join("roses-bevy-scene.exe"), "new game").unwrap();
    fs::write(root.join("README.txt"), "new readme").unwrap();
    fs::write(root.join("assets").join("a.wgsl"), "new shader").unwrap();
    fs::write(root.join("assets").join("deep").join("added.wgsl"), "added shader").unwrap();
    let zip = dir.join(format!("roses-{NEW}.zip"));
    powershell(&format!("Compress-Archive -Path '{}' -DestinationPath '{}' -CompressionLevel Optimal", root.display(), zip.display()), &[]);
    let script = repo().join("scripts/release/write-latest.ps1").canonicalize().unwrap();
    let manifest = dir.join("latest.json");
    let status = Command::new("powershell")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-Zip")
        .arg(&zip)
        .args(["-Release", NEW, "-Out"])
        .arg(&manifest)
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    Package { zip: fs::read(zip).unwrap(), manifest: fs::read(manifest).unwrap(), new_files: tree(&root, None) }
}

/// A launcher running from a throwaway install. Ends what it started.
struct Install {
    dir: PathBuf,
    launcher: Option<Child>,
    data: BTreeMap<String, String>,
    program: BTreeMap<String, String>,
}

impl Drop for Install {
    fn drop(&mut self) {
        if let Some(child) = &mut self.launcher {
            let _ = child.kill();
            let _ = child.wait();
        }
        // A restarted launcher, found by its path.
        let filter = format!("Get-Process roses-launcher -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -like '{}*' }} | Stop-Process -Force", self.dir.display());
        let _ = Command::new("powershell").args(["-NoProfile", "-Command", &filter]).status();
    }
}

fn install(work: &Path, name: &str, server: &str, extra_env: &[(&str, &str)]) -> Install {
    // Plain, not canonicalize's \?\ form: the Path filters below compare it.
    let dir = std::path::absolute(work).unwrap().join(format!("install-{name}"));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(dir.join("assets")).unwrap();
    fs::create_dir_all(dir.join("roses-data").join("memory-card")).unwrap();
    fs::create_dir_all(dir.join("roses-data").join("game-import")).unwrap();
    fs::copy(env_path("ROSES_UPDATE_E2E_OLD"), dir.join("roses-launcher.exe")).unwrap();
    fs::write(dir.join("roses.exe"), "old importer").unwrap();
    fs::write(dir.join("roses-bevy-scene.exe"), "old game").unwrap();
    fs::write(dir.join("README.txt"), "old readme").unwrap();
    fs::write(dir.join("assets").join("a.wgsl"), "old shader").unwrap();
    fs::write(dir.join("assets").join("gone-in-new.wgsl"), "stays").unwrap();
    fs::write(dir.join("roses-data").join("memory-card").join("card.bin"), "the player's save").unwrap();
    fs::write(dir.join("roses-data").join("game-import").join("file.bin"), "the player's import").unwrap();
    fs::write(dir.join("roses-data").join("session.json"), "{}").unwrap();
    fs::write(dir.join("platform.json"), format!(r#"{{"launcher_update_api": "{server}/api"}}"#)).unwrap();
    let data = tree(&dir.join("roses-data"), None);
    let program = tree(&dir, Some("roses-data"));
    let mut command = Command::new(dir.join("roses-launcher.exe"));
    command.env("ROSES_UPDATE_TEST_HIDDEN", "1").env("ROSES_MUTE", "1").current_dir(&dir);
    for (name, value) in extra_env {
        command.env(name, value);
    }
    let launcher = Some(command.spawn().unwrap());
    Install { dir, launcher, data, program }
}

impl Install {
    /// launcher.log and its previous copy, the old run's after a restart.
    fn log(&self) -> String {
        let logs = self.dir.join("roses-data").join("logs");
        let read = |name: &str| fs::read(logs.join(name)).map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
        read("launcher-previous.log") + &read("launcher.log")
    }

    fn wait_log(&self, text: &str, seconds: u64) {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            if self.log().contains(text) {
                return;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        panic!("no {text:?} in the log:\n{}", self.log());
    }

    fn pid(&self) -> u32 {
        self.launcher.as_ref().unwrap().id()
    }

    /// Press the Update button through its window, hidden as it is.
    fn click_update(&self) {
        struct Search {
            pid: u32,
            found: Vec<HWND>,
        }
        unsafe extern "system" fn each(hwnd: HWND, data: LPARAM) -> i32 {
            let search = &mut *(data as *mut Search);
            let mut pid = 0;
            GetWindowThreadProcessId(hwnd, &mut pid);
            if pid == search.pid {
                search.found.push(hwnd);
            }
            1
        }
        let mut search = Search { pid: self.pid(), found: Vec::new() };
        let class = wide("Button");
        let text = wide(BUTTON);
        unsafe {
            EnumWindows(Some(each), &mut search as *mut Search as LPARAM);
            let button = search.found.iter().map(|&main| FindWindowExW(main, std::ptr::null_mut(), class.as_ptr(), text.as_ptr())).find(|b| !b.is_null());
            let button = button.unwrap_or_else(|| panic!("no {BUTTON:?} button"));
            assert_ne!(PostMessageW(button, BM_CLICK, 0, 0), 0);
        }
    }

    fn assert_old_version_works(&mut self) {
        assert_eq!(tree(&self.dir, Some("roses-data")), self.program, "the program files are the old ones and no staging folder is left");
        self.assert_data_untouched();
    }

    fn assert_data_untouched(&self) {
        let now: BTreeMap<_, _> = tree(&self.dir.join("roses-data"), None).into_iter().filter(|(name, _)| !name.starts_with("logs/")).collect();
        assert_eq!(now, self.data, "roses-data");
    }

    fn exited(&mut self, seconds: u64) -> bool {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            if self.launcher.as_mut().unwrap().try_wait().unwrap().is_some() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        false
    }

    /// Whether a launcher from this install is running (`powershell`, as the
    /// restarted one is no child of ours).
    fn restarted_is_running(&self) -> bool {
        let filter = format!("@(Get-Process roses-launcher -ErrorAction SilentlyContinue | Where-Object {{ $_.Path -like '{}*' }}).Count", self.dir.display());
        let out = Command::new("powershell").args(["-NoProfile", "-Command", &filter]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim() != "0"
    }
}

fn work() -> PathBuf {
    let work = env_path("ROSES_UPDATE_E2E_WORK");
    fs::create_dir_all(&work).unwrap();
    work
}

fn good(package: &Package) -> Release {
    Release { api_status: 200, manifest: Some(package.manifest.clone()), zip: package.zip.clone(), cut_at: None }
}

/// The release with `zip` and a manifest that really matches it.
fn republished(package: &Package, zip: Vec<u8>) -> Release {
    let mut manifest: serde_json::Value = serde_json::from_slice(&package.manifest).unwrap();
    manifest["size"] = zip.len().into();
    manifest["sha256"] = format!("{:x}", Sha256::digest(&zip)).into();
    Release { api_status: 200, manifest: Some(serde_json::to_vec(&manifest).unwrap()), zip, cut_at: None }
}

/// A failure before the swap: said in the log, the launcher stays open on the
/// old version, and the button comes back.
fn fails_before_swap(name: &str, release: Release, words: &str) {
    let work = work();
    let server = serve(release);
    let mut run = install(&work, name, &server.base, &[]);
    run.wait_log("is available", 30);
    run.click_update();
    run.wait_log(words, 60);
    std::thread::sleep(Duration::from_millis(800));
    assert!(!run.exited(1), "the launcher stays open");
    run.assert_old_version_works();
    // The button is back and works again.
    run.click_update();
    let deadline = Instant::now() + Duration::from_secs(10);
    while run.log().matches("Downloading").count() < 2 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(run.log().matches("Downloading").count() >= 2, "{}", run.log());
    println!("{name}: {}", run.log().lines().filter(|l| l.contains("did not finish")).last().unwrap_or(""));
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_01_a_good_update_replaces_the_programs_and_restarts() {
    let work = work();
    let package = build_package(&work);
    let server = serve(good(&package));
    let mut run = install(&work, "good", &server.base, &[]);
    run.wait_log("2026-11-07 is available", 30);
    run.click_update();
    assert!(run.exited(60), "the old launcher closes:\n{}", run.log());
    run.wait_log("Updated to 2026-11-07.", 60);
    run.wait_log("this is the latest version", 30);
    assert!(run.log().contains("release 2026-11-07"), "the new exe is the one running");
    assert!(run.restarted_is_running());
    let staging = run.dir.join("roses-update-staging");
    let deadline = Instant::now() + Duration::from_secs(10);
    while staging.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(!staging.exists(), "the staging folder is removed");
    let now = tree(&run.dir, Some("roses-data"));
    for (name, hash) in &package.new_files {
        assert_eq!(now.get(name), Some(hash), "{name}");
    }
    assert!(now.contains_key("assets/gone-in-new.wgsl"), "files the new zip does not hold stay");
    run.assert_data_untouched();
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_02_a_damaged_download_is_refused_by_its_checksum() {
    let package = build_package(&work());
    let mut release = good(&package);
    let middle = release.zip.len() / 2;
    release.zip[middle] ^= 0xff;
    fails_before_swap("checksum", release, "checksum does not match");
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_03_a_corrupt_zip_with_a_matching_checksum_is_refused() {
    let package = build_package(&work());
    let zip = vec![0x42; 300_000];
    fails_before_swap("corrupt", republished(&package, zip), "it is damaged");
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_04_a_killed_download_is_refused() {
    let package = build_package(&work());
    let mut release = good(&package);
    release.cut_at = Some(release.zip.len() / 2);
    fails_before_swap("killed", release, "stopped before it finished");
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_05_a_running_program_blocks_the_update_before_any_download() {
    let work = work();
    let package = build_package(&work);
    let server = serve(good(&package));
    let mut run = install(&work, "running", &server.base, &[]);
    // A real running exe as the game: ping under the game's name.
    let game = run.dir.join("roses-bevy-scene.exe");
    fs::copy(Path::new(&std::env::var("SystemRoot").unwrap()).join("System32").join("PING.EXE"), &game).unwrap();
    run.program = tree(&run.dir, Some("roses-data"));
    let mut running = Command::new(&game).args(["-n", "60", "127.0.0.1"]).stdout(Stdio::null()).spawn().unwrap();
    run.wait_log("is available", 30);
    run.click_update();
    run.wait_log("roses-bevy-scene.exe is running. Close the game first", 20);
    assert_eq!(*server.zip_requests.lock().unwrap(), 0, "nothing was downloaded");
    running.kill().unwrap();
    running.wait().unwrap();
    run.assert_old_version_works();
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_06_an_exe_locked_during_the_download_stops_the_update() {
    let work = work();
    let package = build_package(&work);
    let server = serve(good(&package));
    let mut run = install(&work, "lockedexe", &server.base, &[]);
    run.wait_log("is available", 30);
    run.click_update();
    let deadline = Instant::now() + Duration::from_secs(20);
    while *server.zip_requests.lock().unwrap() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let lock = fs::OpenOptions::new().read(true).write(true).share_mode(0).open(run.dir.join("roses.exe")).unwrap();
    run.wait_log("roses.exe is running", 60);
    assert!(!run.exited(1));
    drop(lock);
    run.assert_old_version_works();
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_07_a_file_locked_at_swap_time_restarts_the_old_version() {
    let work = work();
    let package = build_package(&work);
    let server = serve(good(&package));
    let mut run = install(&work, "lockedfile", &server.base, &[]);
    run.wait_log("is available", 30);
    run.click_update();
    // Not an exe, so the launcher's own check passes and the helper meets it.
    let deadline = Instant::now() + Duration::from_secs(20);
    while *server.zip_requests.lock().unwrap() == 0 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let lock = fs::OpenOptions::new().read(true).write(true).share_mode(0).open(run.dir.join("README.txt")).unwrap();
    assert!(run.exited(60));
    run.wait_log("README.txt is in use by another program", 60);
    drop(lock);
    assert!(run.restarted_is_running(), "the old launcher started again");
    let deadline = Instant::now() + Duration::from_secs(10);
    while run.dir.join("roses-update-staging").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    run.assert_old_version_works();
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_08_a_swap_failing_half_way_rolls_back_and_restarts_the_old_version() {
    let work = work();
    let package = build_package(&work);
    let server = serve(good(&package));
    let mut run = install(&work, "halfway", &server.base, &[("ROSES_UPDATE_TEST_FAIL_AFTER", "3")]);
    run.wait_log("is available", 30);
    run.click_update();
    assert!(run.exited(60));
    run.wait_log("so the old version was put back", 30);
    run.wait_log("2026-11-07 is available", 30);
    assert!(run.restarted_is_running(), "the old launcher started again");
    let deadline = Instant::now() + Duration::from_secs(10);
    while run.dir.join("roses-update-staging").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    run.assert_old_version_works();
}

/// The check alone: no button, and the link stays.
fn no_button(name: &str, release: Release, words: &str) {
    let work = work();
    let server = serve(release);
    let mut run = install(&work, name, &server.base, &[]);
    run.wait_log(words, 30);
    assert!(!run.log().contains("is available"));
    run.assert_old_version_works();
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_09_a_rate_limit_keeps_the_link() {
    let package = build_package(&work());
    no_button("ratelimit", Release { api_status: 403, ..good(&package) }, "GitHub is limiting requests");
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_10_a_release_without_latest_json_keeps_the_link() {
    let package = build_package(&work());
    no_button("nofile", Release { manifest: None, ..good(&package) }, "it has no latest.json");
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_11_offline_keeps_the_link() {
    let work = work();
    let free = TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap();
    let mut run = install(&work, "offline", &format!("http://{free}"), &[]);
    run.wait_log("Update check failed: Could not reach 127.0.0.1", 30);
    run.assert_old_version_works();
}

#[test]
#[ignore = "needs the two built launchers"]
fn e2e_12_a_package_with_a_data_folder_is_refused() {
    let work = work();
    let dir = work.join("evil");
    let _ = fs::remove_dir_all(&dir);
    let root = dir.join("roses-evil");
    fs::create_dir_all(root.join("roses-data").join("memory-card")).unwrap();
    fs::copy(env_path("ROSES_UPDATE_E2E_NEW"), root.join("roses-launcher.exe")).unwrap();
    fs::write(root.join("roses-data").join("memory-card").join("card.bin"), "overwritten").unwrap();
    let zip = dir.join("evil.zip");
    powershell(&format!("Compress-Archive -Path '{}' -DestinationPath '{}'", root.display(), zip.display()), &[]);
    let package = build_package(&work);
    fails_before_swap("evil", republished(&package, fs::read(zip).unwrap()), "holds a roses-data folder");
}

/// Writes the page with the Update button to `<work>/update-button.png`.
#[test]
#[ignore = "needs the two built launchers"]
fn e2e_13_the_button_can_be_seen() {
    let work = work();
    let package = build_package(&work);
    let server = serve(good(&package));
    let png = std::path::absolute(work.join("update-button.png")).unwrap();
    let mut run = install(&work, "picture", &server.base, &[("ROSES_LAUNCHER_CAPTURE", png.to_str().unwrap())]);
    assert!(run.exited(60), "{}", run.log());
    assert!(png.is_file(), "{}", run.log());
}
