//! Local launcher jobs. Every game is described by its `platform_sdk` game
//! manifest: importing runs the game's importer, checking an import runs
//! its import check, and playing starts its executable, each a separate
//! program of the game's own (see `registry`). This crate depends on no
//! game: no disc parser, game assets, rules or game protocol.

pub mod profile;
pub mod registry;

use serde::Deserialize;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_OUTPUT_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone)]
pub struct Options {
    pub data_root: PathBuf,
    pub iso: Option<PathBuf>,
    /// The folder the launcher runs from (its games' programs, text files).
    pub launcher_dir: PathBuf,
    /// Development only: a workspace whose private research captures (Simon's
    /// scene, the history cloud checkpoint) the game may use until those are
    /// derived from the disc.
    pub research_root: Option<PathBuf>,
}

/// Release label stamped in by the packaging script (`ROSES_RELEASE` at build
/// time, e.g. `2026-10-06`); development builds have none.
pub const RELEASE: Option<&str> = option_env!("ROSES_RELEASE");

pub fn version_label() -> String {
    match RELEASE {
        Some(release) => format!("version {} (release {release})", env!("CARGO_PKG_VERSION")),
        None => format!("version {} (development build)", env!("CARGO_PKG_VERSION")),
    }
}

/// Log files a player can send when reporting a problem:
/// `launcher.log` (this window's activity, including the import) and
/// `game.log` (the game's own messages), each with a `-previous` copy.
pub fn logs_dir(data_root: &Path) -> PathBuf {
    data_root.join("logs")
}

pub const LAUNCHER_LOG: &str = "launcher.log";
pub const GAME_LOG: &str = "game.log";

/// Start a fresh log file, keeping the last one as `<name>-previous.log`.
pub fn start_log(data_root: &Path, name: &str) -> std::io::Result<File> {
    let directory = logs_dir(data_root);
    fs::create_dir_all(&directory)?;
    let path = directory.join(name);
    if path.is_file() {
        let previous = directory.join(name.replace(".log", "-previous.log"));
        let _ = fs::remove_file(&previous);
        let _ = fs::rename(&path, &previous);
    }
    let mut file = File::create(&path)?;
    use std::io::Write;
    writeln!(
        file,
        "Roses {} - {name} started {} UTC\nProgram folder: {}\nData folder: {}\n",
        version_label(),
        utc_timestamp(SystemTime::now()),
        std::env::current_exe()
            .ok()
            .and_then(|exe| exe.parent().map(Path::to_path_buf))
            .unwrap_or_default()
            .display(),
        data_root.display()
    )?;
    Ok(file)
}

/// `YYYY-MM-DD HH:MM:SS` in UTC without a date library.
pub fn utc_timestamp(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_secs());
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

impl Options {
    pub fn parse(
        arguments: impl IntoIterator<Item = OsString>,
        executable: &Path,
    ) -> Result<Self, String> {
        let directory = executable
            .parent()
            .ok_or("launcher executable has no parent directory")?;
        let mut data_root = None;
        let mut iso = None;
        let mut research_root = None;
        let mut arguments = arguments.into_iter();
        while let Some(option) = arguments.next() {
            let destination = if option == "--data-root" {
                &mut data_root
            } else if option == "--iso" {
                &mut iso
            } else if option == "--research-root" {
                &mut research_root
            } else {
                return Err(format!(
                    "Unknown option: {}\nUse --data-root <folder>, --iso <game.iso> or --research-root <folder>.",
                    option.to_string_lossy()
                ));
            };
            if destination.is_some() {
                return Err(format!("Duplicate option: {}", option.to_string_lossy()));
            }
            let value = arguments
                .next()
                .filter(|value| !value.is_empty())
                .ok_or_else(|| format!("Missing path after {}", option.to_string_lossy()))?;
            if value.to_str().is_some_and(|value| value.starts_with("--")) {
                return Err(format!("Missing path after {}", option.to_string_lossy()));
            }
            *destination = Some(PathBuf::from(value));
        }
        Ok(Self {
            data_root: absolute(data_root.unwrap_or_else(|| directory.join("roses-data")))?,
            iso: iso.map(absolute).transpose()?,
            launcher_dir: directory.to_path_buf(),
            research_root: research_root.map(absolute).transpose()?,
        })
    }
}

fn absolute(path: PathBuf) -> Result<PathBuf, String> {
    if path.is_absolute() {
        Ok(path)
    } else {
        std::env::current_dir()
            .map(|directory| directory.join(path))
            .map_err(|error| error.to_string())
    }
}

/// What a game's import check found ([`platform_sdk::contract::ImportReport`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verified {
    /// The usable import's folder, when the check named one.
    pub import_dir: Option<PathBuf>,
    /// What it holds, one line ("854 cards, ...").
    pub summary: Option<String>,
}

#[derive(Debug)]
pub enum Event {
    Log(String),
    /// A game with an import check: the check of the newest import (or of
    /// the one just made); None: not imported yet.
    Complete(Result<Option<Verified>, String>),
    /// A game without an import check: its newest completed import's
    /// marker file (None: not imported yet).
    Imported(Result<Option<PathBuf>, String>),
}

fn read_json_bounded<T: serde::de::DeserializeOwned>(path: &Path, limit: u64) -> Result<T, String> {
    let file =
        File::open(path).map_err(|error| format!("Cannot open {}: {error}", path.display()))?;
    if file.metadata().map_err(|error| error.to_string())?.len() > limit {
        return Err(format!(
            "{} exceeds the {} MiB limit.",
            path.display(),
            limit / (1024 * 1024)
        ));
    }
    let mut bytes = Vec::new();
    file.take(limit + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() as u64 > limit {
        return Err(format!("{} grew beyond the size limit.", path.display()));
    }
    serde_json::from_slice(&bytes).map_err(|error| format!("Invalid {}: {error}", path.display()))
}

fn fresh_import_path(root: &Path) -> Result<PathBuf, String> {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_millis();
    for _ in 0..100 {
        let sequence = NEXT.fetch_add(1, Ordering::Relaxed);
        let output = root.join(format!(
            "game-{timestamp}-{}-{sequence}",
            std::process::id()
        ));
        if !output.exists() {
            return Ok(output);
        }
    }
    Err("Could not choose a new import directory.".into())
}

/// The development options given on the command line that a game's
/// manifest takes (`dev_options`), as arguments to pass on.
pub fn dev_args(options: &Options, allowed: &[String]) -> Vec<OsString> {
    let mut given: Vec<(&str, &Path)> = Vec::new();
    if let Some(root) = &options.research_root {
        given.push(("--research-root", root));
    }
    given
        .into_iter()
        .filter(|(name, _)| allowed.iter().any(|a| a == name))
        .flat_map(|(name, value)| [OsString::from(name), value.as_os_str().to_owned()])
        .collect()
}

/// `v0.1.0 · 2026-10-06` (or `· dev build`) for the title bar.
pub fn short_version() -> String {
    match RELEASE {
        Some(release) => format!("v{} · {release}", env!("CARGO_PKG_VERSION")),
        None => format!("v{} · dev build", env!("CARGO_PKG_VERSION")),
    }
}

/// One progress line of an importer (`"progress": "steps"`): `[4/12] Campaign map`
/// gives `(4, 12, "Campaign map")`.
pub fn parse_step(line: &str) -> Option<(usize, usize, String)> {
    let rest = line.trim().strip_prefix('[')?;
    let (fraction, text) = rest.split_once(']')?;
    let (step, total) = fraction.split_once('/')?;
    let (step, total) = (step.parse().ok()?, total.parse().ok()?);
    (step >= 1 && step <= total && total <= 100).then(|| (step, total, text.trim().to_owned()))
}

/// A short, player-facing name for an importer step: parentheticals go and
/// the two long technical step names get plain ones.
pub fn friendly_step(text: &str) -> String {
    let text = text.split(" (").next().unwrap_or(text).trim();
    if text.starts_with("Opening") {
        return "Opening and intro movie".into();
    }
    if text.starts_with("Deck choice") {
        return "Story events and ending movies".into();
    }
    if text.chars().count() > 48 {
        let short: String = text.chars().take(46).collect();
        return format!("{}…", short.trim_end());
    }
    text.to_owned()
}

/// `hh:mm:ss` (hours grow past 99 if needed).
pub fn format_play_time(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

/// Up to two initials of a game's name for its sidebar tile: the first
/// letters of its capitalised words ("Example Game" → "EG").
pub fn initials(name: &str) -> String {
    let caps: String = name
        .split_whitespace()
        .filter_map(|word| word.chars().next())
        .filter(|c| c.is_uppercase() || c.is_ascii_digit())
        .take(2)
        .collect();
    if caps.is_empty() { name.chars().take(1).collect::<String>().to_uppercase() } else { caps }
}

/// Small per-install launcher state in `roses-data/launcher-state.json`.
#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(default)]
pub struct LauncherState {
    /// Seconds the game process has been running, summed over sessions.
    pub played_seconds: u64,
    /// The version label for which the beta banner was dismissed.
    pub banner_dismissed: Option<String>,
    /// The game's launch toggles by id (manifest `launch_toggle`), when the
    /// player has changed them.
    pub toggles: std::collections::BTreeMap<String, bool>,
}

impl LauncherState {
    /// The toggle's state: the player's choice, else its default.
    pub fn toggle(&self, toggle: &platform_sdk::manifest::LaunchToggle) -> bool {
        self.toggles.get(&toggle.id).copied().unwrap_or(toggle.default)
    }
}

pub const STATE_FILE: &str = "launcher-state.json";

pub fn load_state(data_root: &Path) -> LauncherState {
    read_json_bounded(&data_root.join(STATE_FILE), 64 * 1024).unwrap_or_default()
}

pub fn save_state(data_root: &Path, state: &LauncherState) -> std::io::Result<()> {
    fs::create_dir_all(data_root)?;
    let temporary = data_root.join(format!("{STATE_FILE}.tmp"));
    fs::write(&temporary, serde_json::to_vec_pretty(state)?)?;
    fs::rename(&temporary, data_root.join(STATE_FILE))
}

/// Where the game keeps its virtual memory card (`roses-data/memory-card`).
pub fn memory_card_dir(data_root: &Path) -> PathBuf {
    data_root.join("memory-card")
}

/// The game's backdrop (its manifest's `art.backdrop`) in the player's own
/// import, shown by the launcher after an import. Only a path inside the
/// given import is returned; nothing ships with the launcher.
pub fn backdrop_candidates(marker: &Path, backdrop: Option<&str>) -> Vec<PathBuf> {
    match (marker.parent(), backdrop) {
        (Some(import), Some(relative)) if platform_sdk::manifest::safe_relative(relative) => vec![import.join(relative)],
        _ => Vec::new(),
    }
}

/// The UTC date (`YYYY-MM-DD`) a game import finished.
pub fn imported_on(game_json: &Path) -> Option<String> {
    let modified = game_json.metadata().ok()?.modified().ok()?;
    Some(utc_timestamp(modified)[..10].to_owned())
}

/// Decode a PNG to straight RGBA (bounded: 16 MiB file, 4096 px per side).
pub fn load_png_rgba(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let file = File::open(path).map_err(|error| error.to_string())?;
    if file.metadata().map_err(|error| error.to_string())?.len() > 16 * 1024 * 1024 {
        return Err("image too large".into());
    }
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let (width, height) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if width == 0 || height == 0 || width > 4096 || height > 4096 {
        return Err("unsupported image size".into());
    }
    let mut buffer = vec![0; reader.output_buffer_size().ok_or("image too large")?];
    let frame = reader
        .next_frame(&mut buffer)
        .map_err(|error| error.to_string())?;
    let pixels = (width * height) as usize;
    let data = match frame.color_type {
        png::ColorType::Rgba => buffer[..pixels * 4].to_vec(),
        png::ColorType::Rgb => buffer[..pixels * 3]
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => buffer[..pixels * 2]
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => buffer[..pixels]
            .iter()
            .flat_map(|&v| [v, v, v, 255])
            .collect(),
        png::ColorType::Indexed => return Err("unexpanded palette image".into()),
    };
    Ok((width, height, data))
}

/// Open a file or folder with its default program.
pub fn open_path(path: &Path) -> Result<(), String> {
    open_folder(path)
}

pub fn open_folder(directory: &Path) -> Result<(), String> {
    let mut command = Command::new("explorer.exe");
    command.arg(directory);
    hide_console(&mut command);
    command
        .spawn()
        .map(|_| ())
        .map_err(|error| error.to_string())
}

fn hide_console(command: &mut Command) {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    #[cfg(not(windows))]
    let _ = command;
}

fn drain_output(mut reader: impl Read, events: &Sender<Event>) -> std::io::Result<String> {
    let mut retained = Vec::new();
    let mut buffer = [0u8; 4096];
    let mut truncated = false;
    loop {
        let count = match reader.read(&mut buffer) {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            break;
        }
        let accepted = count.min(MAX_OUTPUT_BYTES - retained.len());
        if accepted != 0 {
            retained.extend_from_slice(&buffer[..accepted]);
            let _ = events.send(Event::Log(display_text(&buffer[..accepted])));
        }
        if accepted < count && !truncated {
            truncated = true;
            let _ = events.send(Event::Log(
                "\n[Further importer output omitted; the process is still being drained.]\n".into(),
            ));
        }
    }
    Ok(display_text(&retained))
}

fn display_text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .filter(|character| !character.is_control() || matches!(character, '\n' | '\r' | '\t'))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    use std::sync::mpsc;

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let path =
                fresh_import_path(&std::env::temp_dir().join("roses-launcher-tests")).unwrap();
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            // This path is a fresh directory created by Fixture::new, never a supplied path.
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn output_retention_is_bounded_but_the_entire_stream_is_drained() {
        let input = vec![b'x'; MAX_OUTPUT_BYTES * 4];
        let mut cursor = Cursor::new(&input);
        let (sender, receiver) = mpsc::channel();
        let output = drain_output(&mut cursor, &sender).unwrap();
        assert_eq!(output.len(), MAX_OUTPUT_BYTES);
        assert_eq!(cursor.position(), input.len() as u64);
        let logs: Vec<_> = receiver.try_iter().collect();
        assert!(logs.len() <= MAX_OUTPUT_BYTES / 4096 + 1);
    }

    #[test]
    fn utc_timestamps_and_logs_rotate() {
        assert_eq!(utc_timestamp(UNIX_EPOCH), "1970-01-01 00:00:00");
        let leap = UNIX_EPOCH + std::time::Duration::from_secs(951_782_400 + 3_723);
        assert_eq!(utc_timestamp(leap), "2000-02-29 01:02:03");
        let fixture = Fixture::new();
        drop(start_log(&fixture.0, GAME_LOG).unwrap());
        drop(start_log(&fixture.0, GAME_LOG).unwrap());
        assert!(logs_dir(&fixture.0).join("game.log").is_file());
        assert!(logs_dir(&fixture.0).join("game-previous.log").is_file());
    }

    #[test]
    fn importer_step_lines_and_play_time_are_parsed() {
        assert_eq!(
            parse_step("[4/12] Campaign map\r"),
            Some((4, 12, "Campaign map".to_owned()))
        );
        assert_eq!(parse_step("[0/12] x"), None);
        assert_eq!(parse_step("[13/12] x"), None);
        assert_eq!(parse_step("Import destination: C:\\x"), None);
        assert_eq!(parse_step("[a/b] x"), None);
        assert_eq!(friendly_step("Opening (intro movie: x)"), "Opening and intro movie");
        assert_eq!(friendly_step("Battle arenas"), "Battle arenas");
        assert_eq!(friendly_step("Deck choice, Seto, side choice (x)"), "Story events and ending movies");
        assert!(friendly_step(&"x".repeat(80)).chars().count() <= 48);
        assert_eq!(format_play_time(0), "00:00:00");
        assert_eq!(format_play_time(3 * 3600 + 25 * 60 + 7), "03:25:07");
    }

    #[test]
    fn launcher_state_round_trips_and_tolerates_missing_or_bad_files() {
        let fixture = Fixture::new();
        assert_eq!(load_state(&fixture.0), LauncherState::default());
        let mut state = LauncherState {
            played_seconds: 4242,
            banner_dismissed: Some("v1".into()),
            ..LauncherState::default()
        };
        state.toggles.insert("extras".into(), false);
        save_state(&fixture.0, &state).unwrap();
        assert_eq!(load_state(&fixture.0), state);
        fs::write(fixture.0.join(STATE_FILE), b"{oops").unwrap();
        assert_eq!(load_state(&fixture.0), LauncherState::default());
        fs::write(fixture.0.join(STATE_FILE), br#"{"played_seconds": 9}"#).unwrap();
        assert_eq!(load_state(&fixture.0).played_seconds, 9);
        let toggle = platform_sdk::manifest::LaunchToggle { id: "extras".into(), label: "Extras".into(), default: true, args_on: vec![], args_off: vec!["--faithful".into()] };
        assert!(load_state(&fixture.0).toggle(&toggle), "unchanged: the default");
        save_state(&fixture.0, &state).unwrap();
        assert!(!load_state(&fixture.0).toggle(&toggle), "the player's choice is remembered");
    }

    #[test]
    fn generated_import_paths_are_distinct_and_stay_under_root() {
        let root = std::env::temp_dir().join("roses-launcher-path-test");
        let first = fresh_import_path(&root).unwrap();
        let second = fresh_import_path(&root).unwrap();
        assert_ne!(first, second);
        assert_eq!(first.parent(), Some(root.as_path()));
        assert!(!first.exists());
    }

    #[test]
    fn rejects_duplicate_and_missing_launcher_options() {
        let exe = std::env::current_exe().unwrap();
        for arguments in [
            vec!["--iso"],
            vec!["--data-root", "--iso"],
            vec!["--iso", "a.iso", "--iso", "b.iso"],
            vec!["--unknown", "x"],
            // The external movie converters are gone (the built-in decoder).
            vec!["--ffmpeg", "a.exe"],
            vec!["--blender", "b.exe"],
        ] {
            assert!(Options::parse(arguments.into_iter().map(OsString::from), &exe).is_err());
        }
        let options = Options::parse(Vec::<OsString>::new(), &exe).unwrap();
        assert_eq!(options.launcher_dir, exe.parent().unwrap());
        assert!(options.data_root.is_absolute());
        let options = Options::parse(["--research-root", "work"].map(OsString::from), &exe).unwrap();
        assert!(options.research_root.as_deref().is_some_and(Path::is_absolute));
    }

    #[test]
    fn development_options_go_only_to_games_that_take_them() {
        let exe = std::env::current_exe().unwrap();
        let none = Options::parse(Vec::<OsString>::new(), &exe).unwrap();
        let all = ["--research-root".to_string()];
        assert!(dev_args(&none, &all).is_empty(), "no development option was given");
        let options = Options::parse(
            ["--research-root", "C:/research root"].map(OsString::from),
            &exe,
        )
        .unwrap();
        assert!(dev_args(&options, &["--other".into()]).is_empty());
        assert_eq!(
            dev_args(&options, &["--research-root".into()]),
            ["--research-root", "C:/research root"].map(OsString::from)
        );
        assert!(dev_args(&options, &[]).is_empty());
    }

    #[test]
    fn backdrops_come_only_from_the_given_import() {
        let game = Path::new("C:/data/game-1/game.json");
        let candidates = backdrop_candidates(game, Some("story/textures/texetc-00.png"));
        assert_eq!(candidates, [Path::new("C:/data/game-1/story/textures/texetc-00.png")]);
        assert!(backdrop_candidates(game, Some("../elsewhere.png")).is_empty());
        assert!(backdrop_candidates(game, None).is_empty());
    }
}
