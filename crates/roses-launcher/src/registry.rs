//! The game registry: every game the launcher can import and start, each
//! described by a `platform_sdk` game manifest, plus the platform's own
//! settings (its name, which is a placeholder until the brand is chosen,
//! and its web links).
//!
//! Where manifests come from, in order (the first of an id wins, at most
//! [`MAX_GAMES`]):
//! 1. `games/<id>/game.json` beside the launcher: a game installed in its own
//!    folder (its programs are in that folder);
//! 2. `games/<id>.json` beside the launcher: a game whose programs sit beside
//!    the launcher;
//! 3. the built-in Roses manifest (`games/roses.json` in this crate), so a
//!    download from before the registry keeps working unchanged.
//!
//! Roses is imported, checked and started through the same manifest path as
//! every other game: its importer, its import check (`importer.verify` after
//! an import, `importer.check` at start, both `roses verify-import`) and its
//! executable are its own programs, named in its manifest.

use crate::{drain_output, fresh_import_path, hide_console, start_log, Event, Verified, GAME_LOG};
use platform_sdk::contract::{self, ImportReport};
use platform_sdk::manifest::{expand_args, valid_link, CommandSpec, DiscInput, GameManifest};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc::Sender;
use std::thread;

/// The Roses manifest compiled in.
pub const BUILTIN_ROSES: &str = include_str!("../games/roses.json");
/// The launcher's own settings beside the executable.
pub const PLATFORM_FILE: &str = "platform.json";
pub const GAMES_DIR: &str = "games";
/// Games shown in the sidebar at most.
pub const MAX_GAMES: usize = 6;
/// The platform's name (the oldrusty brand) when `platform.json` names none.
pub const PLATFORM_NAME_PLACEHOLDER: &str = "oldrusty";

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct PlatformFile {
    name: Option<String>,
    /// The launcher's own release page (newest download).
    launcher_releases: Option<String>,
    /// Where to report a problem with the launcher itself.
    launcher_issues: Option<String>,
    website: Option<String>,
}

/// The platform's name and links (`platform.json`; every field optional).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Platform {
    pub name: String,
    pub launcher_releases: Option<String>,
    pub launcher_issues: Option<String>,
    pub website: Option<String>,
}

impl Default for Platform {
    fn default() -> Self {
        Self { name: PLATFORM_NAME_PLACEHOLDER.into(), launcher_releases: None, launcher_issues: None, website: None }
    }
}

impl Platform {
    /// `platform.json` in `launcher_dir`; problems are returned as warnings
    /// and the placeholders used.
    pub fn load(launcher_dir: &Path) -> (Self, Vec<String>) {
        let path = launcher_dir.join(PLATFORM_FILE);
        let mut warnings = Vec::new();
        let file: PlatformFile = match fs::read_to_string(&path) {
            Ok(text) if text.len() <= 64 * 1024 => serde_json::from_str(&text).unwrap_or_else(|e| {
                warnings.push(format!("{} is not usable ({e}); using the defaults.", path.display()));
                PlatformFile::default()
            }),
            Ok(_) => {
                warnings.push(format!("{} is too large; using the defaults.", path.display()));
                PlatformFile::default()
            }
            Err(_) => PlatformFile::default(),
        };
        let mut link = |what: &str, value: Option<String>| {
            value.filter(|url| {
                let ok = valid_link(url);
                if !ok {
                    warnings.push(format!("platform.json: {what} must be an https:// address; ignored."));
                }
                ok
            })
        };
        let platform = Self {
            name: file
                .name
                .map(|n| n.trim().chars().take(40).collect::<String>())
                .filter(|n| !n.is_empty())
                .unwrap_or_else(|| PLATFORM_NAME_PLACEHOLDER.into()),
            launcher_releases: link("launcher_releases", file.launcher_releases),
            launcher_issues: link("launcher_issues", file.launcher_issues),
            website: link("website", file.website),
        };
        (platform, warnings)
    }
}

/// One registered game.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Game {
    pub manifest: GameManifest,
    /// Where its programs are.
    pub install_dir: PathBuf,
    /// Its imports, saves and logs.
    pub data_dir: PathBuf,
    /// Where the manifest came from (for the log).
    pub source: String,
}

/// Every registered game (Roses always among them) and warnings about
/// manifests that were skipped.
pub fn load_registry(launcher_dir: &Path, data_root: &Path) -> (Vec<Game>, Vec<String>) {
    let mut games: Vec<Game> = Vec::new();
    let mut warnings = Vec::new();
    let mut add = |manifest: GameManifest, install_dir: PathBuf, source: String, warnings: &mut Vec<String>| {
        if games.iter().any(|g| g.manifest.id == manifest.id) {
            warnings.push(format!("{source}: a game with id {} is registered already; skipped.", manifest.id));
            return;
        }
        if games.len() >= MAX_GAMES {
            warnings.push(format!("{source}: at most {MAX_GAMES} games are shown; skipped."));
            return;
        }
        let data_dir = match manifest.data_dir() {
            "." => data_root.to_path_buf(),
            dir => data_root.join(dir),
        };
        games.push(Game { manifest, install_dir, data_dir, source });
    };
    let dir = launcher_dir.join(GAMES_DIR);
    let mut entries: Vec<PathBuf> = fs::read_dir(&dir).into_iter().flatten().flatten().map(|e| e.path()).collect();
    entries.sort();
    for path in entries.into_iter().take(64) {
        let (file, install_dir) = if path.is_dir() {
            (path.join("game.json"), path.clone())
        } else if path.extension().is_some_and(|e| e == "json") {
            (path.clone(), launcher_dir.to_path_buf())
        } else {
            continue;
        };
        let Ok(text) = fs::read_to_string(&file) else { continue };
        if text.len() > 256 * 1024 {
            warnings.push(format!("{}: too large; skipped.", file.display()));
            continue;
        }
        match GameManifest::parse(&text) {
            Ok(m) => add(m, install_dir, file.display().to_string(), &mut warnings),
            Err(e) => warnings.push(format!("{}: {e}; skipped.", file.display())),
        }
    }
    let builtin = GameManifest::parse(BUILTIN_ROSES).expect("the built-in Roses manifest is valid");
    if !games.iter().any(|g| g.manifest.id == builtin.id) {
        // Roses first: the launcher's first entry, as before the registry.
        let data_dir = data_root.to_path_buf();
        games.insert(0, Game { manifest: builtin, install_dir: launcher_dir.to_path_buf(), data_dir, source: "built in".into() });
        games.truncate(MAX_GAMES);
    }
    (games, warnings)
}

impl Game {
    pub fn id(&self) -> &str {
        &self.manifest.id
    }

    /// The game checks its imports (`importer.verify` / `importer.check`):
    /// an import counts once its check says so, and the check's summary is
    /// shown.
    pub fn checks_imports(&self) -> bool {
        self.manifest.importer.verify.is_some() || self.manifest.importer.check.is_some()
    }

    /// The sidebar tile's text before an import (`art.initials`, else the
    /// name's initials).
    pub fn initials(&self) -> String {
        match &self.manifest.art.initials {
            Some(text) => text.clone(),
            None => crate::initials(&self.manifest.name),
        }
    }

    fn helper_program(&self, spec: &CommandSpec) -> PathBuf {
        self.install_dir.join(spec.program.as_deref().unwrap_or(&self.manifest.importer.program))
    }

    /// Run an import check and read its report (blocking).
    fn run_check(&self, spec: &CommandSpec, values: &BTreeMap<&str, OsString>) -> Result<ImportReport, String> {
        let program = self.helper_program(spec);
        if !program.is_file() {
            return Err(format!(
                "Cannot find {}. Keep the game's programs beside the launcher.",
                program.display()
            ));
        }
        let args = expand_args(&spec.args, values).map_err(|e| e.to_string())?;
        let mut command = Command::new(&program);
        command.args(args).current_dir(&self.install_dir).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
        self.environment(&mut command, None);
        hide_console(&mut command);
        let output = command.output().map_err(|error| format!("Could not start the import check: {error}"))?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        ImportReport::find(&stdout).ok_or_else(|| {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let why: String = stderr.trim().chars().take(2048).collect();
            format!("The import check gave no answer ({}). {why}", output.status)
        })
    }

    /// The newest usable import, by the game's `check` (None: not imported
    /// yet). Games without one: Err (use [`Game::latest_marker`]).
    pub fn check_imports(&self) -> Result<Option<Verified>, String> {
        let Some(spec) = &self.manifest.importer.check else {
            return Err("This game has no import check.".into());
        };
        let values = BTreeMap::from([("data_dir", self.data_dir.as_os_str().to_owned())]);
        let report = self.run_check(spec, &values)?;
        if !report.ok {
            return Err(report.error.unwrap_or_else(|| "The newest import does not check out.".into()));
        }
        Ok(report.import_dir.map(|dir| Verified { import_dir: Some(PathBuf::from(dir)), summary: report.summary }))
    }

    /// Check the import just made in `import_dir` (its `verify`; without one
    /// it counts as made).
    pub fn verify_import(&self, import_dir: &Path, marker: &Path) -> Result<Verified, String> {
        let Some(spec) = &self.manifest.importer.verify else {
            return Ok(Verified { import_dir: Some(import_dir.to_path_buf()), summary: None });
        };
        let values = BTreeMap::from([
            ("import_dir", import_dir.as_os_str().to_owned()),
            ("marker", marker.as_os_str().to_owned()),
            ("data_dir", self.data_dir.as_os_str().to_owned()),
        ]);
        let report = self.run_check(spec, &values)?;
        if !report.ok {
            return Err(report.error.unwrap_or_else(|| "The import does not check out.".into()));
        }
        Ok(Verified { import_dir: Some(import_dir.to_path_buf()), summary: report.summary })
    }

    pub fn importer_program(&self) -> PathBuf {
        self.install_dir.join(&self.manifest.importer.program)
    }

    pub fn game_program(&self) -> PathBuf {
        self.install_dir.join(&self.manifest.executable.program)
    }

    /// The newest completed import's marker file (`<prefix>*/<marker>`).
    pub fn latest_marker(&self) -> Option<PathBuf> {
        let importer = &self.manifest.importer;
        let mut found: Vec<_> = fs::read_dir(&self.data_dir)
            .into_iter()
            .flatten()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().starts_with(&importer.import_prefix))
            .map(|e| e.path().join(&importer.complete_marker))
            .filter_map(|p| Some((p.metadata().ok()?.modified().ok()?, p)))
            .take(1024)
            .collect();
        found.sort();
        found.pop().map(|(_, p)| p)
    }

    fn environment(&self, command: &mut Command, session_file: Option<&Path>) {
        for (key, value) in contract::environment(self.id(), &self.data_dir, &crate::version_label(), session_file) {
            command.env(key, value);
        }
    }

    /// The importer for `iso` into `import_dir` (`extra`: launcher options
    /// passed on that its manifest's `dev_options` take).
    pub fn import_command(&self, iso: &Path, import_dir: &Path, extra: &[OsString]) -> Result<Command, String> {
        let values = BTreeMap::from([
            ("iso", iso.as_os_str().to_owned()),
            ("import_dir", import_dir.as_os_str().to_owned()),
            ("data_dir", self.data_dir.as_os_str().to_owned()),
        ]);
        let args = expand_args(&self.manifest.importer.args, &values).map_err(|e| e.to_string())?;
        let mut command = Command::new(self.importer_program());
        command.args(args).args(extra).current_dir(&self.install_dir).stdin(Stdio::null());
        self.environment(&mut command, None);
        hide_console(&mut command);
        Ok(command)
    }

    /// The game with the newest import (`extra`: development options passed
    /// on; `toggle_on`: the state of the manifest's launch toggle, whose
    /// arguments follow the executable's). Its output goes to
    /// `logs/game.log` in its data folder.
    pub fn launch_command(&self, toggle_on: bool, extra: &[OsString], session_file: Option<&Path>) -> Result<Command, String> {
        let marker = self
            .latest_marker()
            .ok_or_else(|| format!("There is no completed game import yet. Choose your {0} and click Import {0} first.", self.manifest.disc.noun()))?;
        let program = self.game_program();
        if !program.is_file() {
            return Err(format!(
                "The game is missing. Keep {} beside the launcher. Expected: {}",
                self.manifest.executable.program,
                program.display()
            ));
        }
        let import_dir = marker.parent().map(Path::to_path_buf).unwrap_or_default();
        let values = BTreeMap::from([
            ("marker", marker.as_os_str().to_owned()),
            ("import_dir", import_dir.into_os_string()),
            ("data_dir", self.data_dir.as_os_str().to_owned()),
        ]);
        let mut args = expand_args(&self.manifest.executable.args, &values).map_err(|e| e.to_string())?;
        if let Some(toggle) = &self.manifest.launch_toggle {
            args.extend(expand_args(toggle.args(toggle_on), &values).map_err(|e| e.to_string())?);
        }
        let mut command = Command::new(&program);
        command.args(args).args(extra).current_dir(&self.install_dir).stdin(Stdio::null());
        self.environment(&mut command, session_file);
        let log = start_log(&self.data_dir, GAME_LOG).and_then(|file| {
            let copy = file.try_clone()?;
            Ok((file, copy))
        });
        match log {
            Ok((out, err)) => command.stdout(out).stderr(err),
            Err(_) => command.stdout(Stdio::null()).stderr(Stdio::null()),
        };
        hide_console(&mut command);
        Ok(command)
    }

    pub fn launch(&self, toggle_on: bool, extra: &[OsString], session_file: Option<&Path>) -> Result<Child, String> {
        self.launch_command(toggle_on, extra, session_file)?.spawn().map_err(|error| format!("Could not start the game: {error}"))
    }
}

/// A finished import: its marker file, and what its check said (games
/// that check their imports).
#[derive(Debug)]
pub struct Imported {
    pub marker: PathBuf,
    pub verified: Option<Verified>,
}

/// Import `iso` for `game` into a new folder (blocking; progress and output
/// go to `events` as [`Event::Log`]).
pub fn import(game: &Game, iso: &Path, extra: &[OsString], events: &Sender<Event>) -> Result<Imported, String> {
    let program = game.importer_program();
    if !program.is_file() {
        return Err(format!(
            "Cannot find {}. Put {} beside the launcher, or build both workspace binaries.",
            program.display(),
            game.manifest.importer.program
        ));
    }
    let disc = &game.manifest.disc;
    if !iso.is_file() {
        return Err(format!("Choose an existing {} file before importing.", disc.noun()));
    }
    if !disc.accepts(iso) {
        return Err(format!("Choose an uncompressed {} file. Archive files must be extracted first.", disc_kinds(disc)));
    }
    if game.data_dir.exists() && !game.data_dir.is_dir() {
        return Err("The selected data root is not a directory.".into());
    }
    fs::create_dir_all(&game.data_dir).map_err(|e| format!("{}: {e}", game.data_dir.display()))?;
    let output = fresh_import_path(&game.data_dir)?;
    let output = output.with_file_name(
        output.file_name().map_or_else(OsString::new, |n| {
            let n = n.to_string_lossy();
            OsString::from(format!("{}{}", game.manifest.importer.import_prefix, n.strip_prefix("game-").unwrap_or(&n)))
        }),
    );
    let _ = events.send(Event::Log(format!(
        "Import destination: {}\nThe full {} fingerprint is checked before each part is converted.\n",
        output.display(),
        disc.noun()
    )));
    let mut command = game.import_command(iso, &output, extra)?;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| format!("Could not start the importer: {error}"))?;
    let stdout = child.stdout.take().ok_or("Importer stdout pipe was unavailable.")?;
    let stderr = child.stderr.take().ok_or("Importer stderr pipe was unavailable.")?;
    let (out_events, err_events) = (events.clone(), events.clone());
    let out_reader = thread::spawn(move || drain_output(stdout, &out_events));
    let err_reader = thread::spawn(move || drain_output(stderr, &err_events));
    let status = child.wait().map_err(|error| format!("Could not wait for the importer: {error}"));
    let out_result = out_reader.join().map_err(|_| "Importer output reader failed.")?;
    let err_result = err_reader.join().map_err(|_| "Importer error reader failed.")?;
    out_result.map_err(|error| format!("Could not read importer output: {error}"))?;
    let stderr = err_result.map_err(|error| format!("Could not read importer errors: {error}"))?;
    let status = status?;
    if !status.success() {
        let code = status.code().map_or_else(|| "process terminated".into(), |code| format!("exit {code}"));
        return Err(import_failure(&code, &stderr, output.exists().then_some(output.as_path()), disc.noun()));
    }
    let marker = output.join(&game.manifest.importer.complete_marker);
    if !marker.is_file() {
        return Err(format!("The importer finished but wrote no {}; the import is incomplete.", game.manifest.importer.complete_marker));
    }
    let verified = if game.checks_imports() {
        Some(game.verify_import(&output, &marker).map_err(|error| {
            format!("Importer exited successfully, but the library could not be verified: {error}")
        })?)
    } else {
        None
    };
    Ok(Imported { marker, verified })
}

/// What the player reads when the importer fails. A disc it refuses gets its
/// one plain line (nothing else in the log means anything to a player); the
/// partial-output note only appears when there is a folder to speak of.
fn import_failure(code: &str, stderr: &str, partial: Option<&Path>, noun: &str) -> String {
    let refused = stderr.lines().find_map(|line| {
        let line = line.trim();
        line.ends_with("can't be imported.").then(|| line.strip_prefix("roses: ").unwrap_or(line))
    });
    let unchanged = format!("Your source {noun} was not modified.");
    match (refused, partial) {
        (Some(reason), None) => format!("{reason}\n{unchanged}"),
        (Some(reason), Some(dir)) => format!("{reason}\nAny partial output remains at {}. {unchanged}", dir.display()),
        (None, None) => format!("Import failed ({code}).\n{}\n{unchanged}", stderr.trim()),
        (None, Some(dir)) => format!("Import failed ({code}).\n{}\nAny partial output remains at {}. {unchanged}", stderr.trim(), dir.display()),
    }
}

/// The accepted disc files for texts: ".iso", ".cue or .bin".
pub fn disc_kinds(disc: &DiscInput) -> String {
    let list: Vec<String> = disc.extensions().iter().map(|e| format!(".{e}")).collect();
    match list.split_last() {
        Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
        _ => list.concat(),
    }
}

/// The file picker's filter for a game's disc files (Windows
/// `lpstrFilter` pairs, NUL-separated, ending in two NULs):
/// "Disc images (*.cue;*.bin)" then "All files".
pub fn disc_filter(disc: &DiscInput) -> String {
    let patterns: Vec<String> = disc.extensions().iter().map(|e| format!("*.{e}")).collect();
    let patterns = patterns.join(";");
    format!("Disc images ({patterns})\0{patterns}\0All files (*.*)\0*.*\0")
}

/// Open an https link in the system browser (anything else is refused).
pub fn open_link(url: &str) -> Result<(), String> {
    if !valid_link(url) {
        return Err(format!("not an https:// address: {url}"));
    }
    let mut command = Command::new("explorer.exe");
    command.arg(url);
    hide_console(&mut command);
    command.spawn().map(|_| ()).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("roses-registry-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn example(id: &str) -> String {
        format!(
            r#"{{"contract": 1, "id": "{id}", "name": "Example {id}", "version": "0.2.0",
               "discs": [{{"label": "NTSC-U", "serial": "SLUS-00000"}}],
               "importer": {{"program": "{id}-cli.exe", "args": ["import", "{{iso}}", "{{import_dir}}"], "import_prefix": "import-", "complete_marker": "done.json"}},
               "executable": {{"program": "{id}.exe", "args": ["--import", "{{import_dir}}"]}},
               "links": {{"issues": "https://example.org/{id}/issues"}}}}"#
        )
    }

    #[test]
    fn roses_is_built_in_and_keeps_the_old_data_folder() {
        let dir = temp("builtin");
        let (games, warnings) = load_registry(&dir, &dir.join("roses-data"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(games.len(), 1);
        let roses = &games[0];
        assert_eq!((roses.id(), roses.checks_imports(), roses.initials()), ("roses", true, "DotR".to_string()));
        let toggle = roses.manifest.launch_toggle.as_ref().expect("Roses' launch toggle");
        assert_eq!((toggle.label.as_str(), toggle.default, toggle.args(false)), ("Online Play and extras", true, &["--faithful".to_string()][..]));
        assert_eq!(roses.data_dir, dir.join("roses-data"));
        assert_eq!(roses.importer_program(), dir.join("roses.exe"));
        assert_eq!(roses.game_program(), dir.join("roses-bevy-scene.exe"));
        assert_eq!(roses.manifest.version, env!("CARGO_PKG_VERSION"), "keep games/roses.json's version in step");
        // Roses' own programs check its imports; the launcher reads no game file.
        let check = roses.manifest.importer.check.as_ref().unwrap();
        assert_eq!(check.args, ["verify-import", "--data-root", "{data_dir}"]);
        let verify = roses.manifest.importer.verify.as_ref().unwrap();
        assert_eq!(verify.args, ["verify-import", "--import", "{import_dir}"]);
        assert_eq!(roses.manifest.executable.dev_options, ["--research-root"]);
        assert!(roses.manifest.importer.dev_options.is_empty(), "the importer takes no development option");
        let hello = roses.manifest.online.as_ref().and_then(|o| o.account_hello.as_ref()).expect("Roses' account hello");
        assert_eq!(hello["compat"]["protocol"], 4, "the Roses server's protocol (roses-net PROTOCOL_VERSION)");
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn other_games_come_from_manifest_files_in_their_own_folders() {
        let dir = temp("files");
        fs::create_dir_all(dir.join("games/beta")).unwrap();
        fs::write(dir.join("games/beta/game.json"), example("beta")).unwrap();
        fs::write(dir.join("games/alpha.json"), example("alpha")).unwrap();
        fs::write(dir.join("games/broken.json"), "{").unwrap();
        fs::write(dir.join("games/dupe.json"), example("alpha")).unwrap();
        let (games, warnings) = load_registry(&dir, &dir.join("data"));
        let ids: Vec<_> = games.iter().map(Game::id).collect();
        assert_eq!(ids, ["roses", "alpha", "beta"]);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let beta = &games[2];
        assert_eq!(beta.install_dir, dir.join("games/beta"));
        assert_eq!(beta.data_dir, dir.join("data/beta"));
        assert_eq!(games[1].install_dir, dir);
        assert_eq!(beta.manifest.links.issues.as_deref(), Some("https://example.org/beta/issues"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn commands_come_from_the_manifest_with_paths_as_single_arguments() {
        let dir = temp("commands");
        let game = Game {
            manifest: GameManifest::parse(&example("alpha")).unwrap(),
            install_dir: dir.join("install & co"),
            data_dir: dir.join("my data"),
            source: "test".into(),
        };
        let iso = Path::new("C:\\My Games\\a & b.iso");
        let out = dir.join("my data").join("import-1");
        let command = game.import_command(iso, &out, &["--extra".into()]).unwrap();
        let args: Vec<OsString> = command.get_args().map(OsString::from).collect();
        assert_eq!(args, [OsString::from("import"), iso.into(), out.clone().into(), "--extra".into()]);
        assert_eq!(command.get_program(), dir.join("install & co").join("alpha-cli.exe"));
        let env: Vec<_> = command.get_envs().map(|(k, v)| (k.to_owned(), v.map(OsString::from))).collect();
        assert!(env.contains(&(contract::ENV_GAME_ID.into(), Some("alpha".into()))));
        // No import yet: Play explains; with one, the newest marker is used.
        assert!(game.launch_command(true, &[], None).unwrap_err().contains("no completed game import"));
        fs::create_dir_all(&out).unwrap();
        fs::write(out.join("done.json"), "{}").unwrap();
        assert_eq!(game.latest_marker(), Some(out.join("done.json")));
        assert!(game.launch_command(true, &[], None).unwrap_err().contains("The game is missing"));
        fs::create_dir_all(&game.install_dir).unwrap();
        fs::write(game.game_program(), b"").unwrap();
        let command = game.launch_command(true, &[], Some(Path::new("C:/s.json"))).unwrap();
        let args: Vec<OsString> = command.get_args().map(OsString::from).collect();
        assert_eq!(args, [OsString::from("--import"), out.clone().into()]);
        // A launch toggle adds its arguments for its state.
        let mut toggled = game.clone();
        toggled.manifest.launch_toggle = Some(platform_sdk::manifest::LaunchToggle {
            id: "extras".into(),
            label: "Extras".into(),
            default: true,
            args_on: vec![],
            args_off: vec!["--faithful".into()],
        });
        let args = |on: bool| toggled.launch_command(on, &["--dev".into()], None).unwrap().get_args().map(OsString::from).collect::<Vec<_>>();
        assert_eq!(args(true), [OsString::from("--import"), out.clone().into(), "--dev".into()]);
        assert_eq!(args(false), [OsString::from("--import"), out.into(), "--faithful".into(), "--dev".into()]);
        assert!(command.get_envs().any(|(k, _)| k == contract::ENV_SESSION_FILE));
        assert!(game.data_dir.join("logs").join(GAME_LOG).is_file());
        let _ = fs::remove_dir_all(&dir);
    }

    /// A game whose import checks are batch files: the launcher reads their
    /// report line and nothing else.
    #[cfg(windows)]
    #[test]
    fn import_checks_are_programs_whose_report_is_read() {
        let dir = temp("checks");
        let script = |name: &str, lines: &[String]| {
            let body: String = lines.iter().map(|l| format!("@echo {l}\r\n")).collect();
            fs::write(dir.join(name), body).unwrap();
            CommandSpec { program: Some(name.into()), args: vec![] }
        };
        let ok = ImportReport { ok: true, import_dir: Some("D:\\data\\import-7".into()), summary: Some("12 levels".into()), error: None };
        let bad = ImportReport { ok: false, error: Some("level 3 is damaged".into()), ..ImportReport::default() };
        let mut manifest = GameManifest::parse(&example("alpha")).unwrap();
        let game = |m: &GameManifest| Game { manifest: m.clone(), install_dir: dir.clone(), data_dir: dir.join("data"), source: "test".into() };
        assert!(game(&manifest).check_imports().is_err(), "no check: the marker counts");
        manifest.importer.check = Some(script("check.cmd", &["noise".into(), ok.line()]));
        manifest.importer.verify = Some(script("verify.cmd", &[bad.line()]));
        let g = game(&manifest);
        assert!(g.checks_imports());
        let found = g.check_imports().unwrap().unwrap();
        assert_eq!(found.summary.as_deref(), Some("12 levels"));
        assert_eq!(found.import_dir.as_deref(), Some(Path::new("D:\\data\\import-7")));
        assert_eq!(g.verify_import(&dir, &dir.join("done.json")).unwrap_err(), "level 3 is damaged");
        manifest.importer.check = Some(script("silent.cmd", &["nothing".into()]));
        assert!(game(&manifest).check_imports().unwrap_err().contains("no answer"));
        manifest.importer.check = Some(script("none.cmd", &[ImportReport { ok: true, ..ImportReport::default() }.line()]));
        assert_eq!(game(&manifest).check_imports(), Ok(None), "nothing imported yet");
        manifest.importer.check = Some(CommandSpec { program: Some("missing.exe".into()), args: vec![] });
        assert!(game(&manifest).check_imports().unwrap_err().contains("Cannot find"));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The Forbidden Memories manifest from its own repository (the file a
    /// release copies to `games/forbidden-memories.json` beside the
    /// launcher): `FM_GAME_MANIFEST`, else the sibling `forbidden2026`
    /// checkout. None (the test says so and passes) when neither exists.
    fn fm_manifest_text() -> Option<String> {
        let path = std::env::var_os("FM_GAME_MANIFEST").map(PathBuf::from).unwrap_or_else(|| {
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../forbidden2026/games/forbidden-memories.json")
        });
        let text = fs::read_to_string(&path).ok();
        if text.is_none() {
            eprintln!("skipped: no Forbidden Memories manifest at {} (set FM_GAME_MANIFEST)", path.display());
        }
        text
    }

    #[test]
    fn the_forbidden_memories_manifest_validates() {
        let Some(text) = fm_manifest_text() else { return };
        let m = GameManifest::parse(&text).unwrap();
        assert_eq!((m.id.as_str(), m.data_dir(), m.art.initials.as_deref()), ("forbidden-memories", "forbidden-memories", Some("FM")));
        assert_eq!(m.disc.extensions(), ["cue", "bin"], "a PS1 dump: a .cue with its .bin, or the .bin alone");
        assert_eq!((m.importer.program.as_str(), m.executable.program.as_str()), ("fm.exe", "fm-game.exe"));
        assert_eq!(m.importer.complete_marker, "game.json");
        assert!(m.importer.verify.is_some() && m.importer.check.is_some());
        assert_eq!(m.launch_toggle, None, "Faithful mode does not apply to FM yet");
        assert_eq!(m.discs[0].serial, "SLES-03947");
    }

    #[test]
    fn roses_and_forbidden_memories_load_side_by_side() {
        let Some(text) = fm_manifest_text() else { return };
        let dir = temp("two-games");
        fs::create_dir_all(dir.join(GAMES_DIR)).unwrap();
        fs::write(dir.join(GAMES_DIR).join("forbidden-memories.json"), text).unwrap();
        let data = dir.join("roses-data");
        let (games, warnings) = load_registry(&dir, &data);
        assert!(warnings.is_empty(), "{warnings:?}");
        let ids: Vec<_> = games.iter().map(Game::id).collect();
        assert_eq!(ids, ["roses", "forbidden-memories"], "Roses first");
        let (roses, fm) = (&games[0], &games[1]);
        assert_eq!(roses.data_dir, data, "Roses keeps roses-data itself");
        assert_eq!(fm.data_dir, data.join("forbidden-memories"), "FM gets its own folder");
        assert_eq!(fm.install_dir, dir, "its programs sit beside the launcher");
        assert_eq!((fm.importer_program(), fm.game_program()), (dir.join("fm.exe"), dir.join("fm-game.exe")));
        assert_eq!((roses.initials(), fm.initials()), ("DotR".to_string(), "FM".to_string()));
        assert_eq!(roses.manifest.disc.extensions(), ["iso"], "Roses still asks for an .iso");
        assert_eq!(disc_kinds(&fm.manifest.disc), ".cue or .bin");
        assert_eq!(disc_filter(&fm.manifest.disc), "Disc images (*.cue;*.bin)\0*.cue;*.bin\0All files (*.*)\0*.*\0");
        assert_eq!(disc_filter(&roses.manifest.disc), "Disc images (*.iso)\0*.iso\0All files (*.*)\0*.*\0");
        // Neither is imported, and each game's state stays in its own folder.
        assert_eq!((roses.latest_marker(), fm.latest_marker()), (None, None));
        let _ = fs::remove_dir_all(&dir);
    }

    /// A game that takes .cue/.bin discs, imported and checked through the
    /// contract by batch files standing in for its importer and its
    /// `verify-import`: the picked file, the import folder and the
    /// `PLATFORM_*` environment reach them; the marker completes the import;
    /// the report line is what counts; Play passes the import folder.
    #[cfg(windows)]
    #[test]
    fn a_cue_bin_game_imports_and_verifies_through_the_contract() {
        let dir = temp("fake-importer");
        let manifest = format!(
            r#"{{"contract": 1, "id": "fake-fm", "name": "Fake FM",
               "discs": [{{"label": "PAL (Europe)", "serial": "SLES-00000"}}],
               "disc": {{"extensions": ["cue", "bin"]}},
               "importer": {{"program": "fake-import.cmd", "args": ["import-game", "--disc", "{{iso}}", "--out", "{{import_dir}}"],
                            "import_prefix": "game-", "complete_marker": "game.json", "progress": "steps",
                            "verify": {{"program": "fake-verify.cmd", "args": ["verify-import", "--import", "{{import_dir}}"]}},
                            "check": {{"program": "fake-verify.cmd", "args": ["verify-import", "--data-root", "{{data_dir}}"]}}}},
               "executable": {{"program": "fake-game.exe", "args": ["--data", "{{import_dir}}"]}}}}"#
        );
        fs::create_dir_all(dir.join(GAMES_DIR)).unwrap();
        fs::write(dir.join(GAMES_DIR).join("fake-fm.json"), manifest).unwrap();
        // The importer: %3 = the picked disc, %5 = the import folder.
        fs::write(
            dir.join("fake-import.cmd"),
            "@echo off\r\n\
             if not \"%1\"==\"import-game\" exit /b 3\r\n\
             if not exist \"%~3\" exit /b 4\r\n\
             if not \"%PLATFORM_GAME_ID%\"==\"fake-fm\" exit /b 5\r\n\
             echo [1/2] Checking the disc\r\n\
             mkdir \"%~5\"\r\n\
             echo {\"disc\": \"%~nx3\"}> \"%~5\\game.json\"\r\n\
             echo [2/2] Done\r\n",
        )
        .unwrap();
        let ok = ImportReport { ok: true, import_dir: None, summary: Some("722 cards".into()), error: None };
        fs::write(
            dir.join("fake-verify.cmd"),
            format!(
                "@echo off\r\n\
                 if \"%2\"==\"--data-root\" goto check\r\n\
                 if not exist \"%~3\\game.json\" (echo {}& exit /b 0)\r\n\
                 echo noise\r\n\
                 echo {}\r\n\
                 exit /b 0\r\n\
                 :check\r\n\
                 set \"p=%~3\"\r\n\
                 set \"p=%p:\\=/%\"\r\n\
                 echo platform-import-report: {{\"ok\":true,\"import_dir\":\"%p%/game-newest\",\"summary\":\"722 cards\"}}\r\n",
                ImportReport { ok: false, error: Some("no game.json".into()), ..ImportReport::default() }.line(),
                ok.line()
            ),
        )
        .unwrap();
        let (games, warnings) = load_registry(&dir, &dir.join("data"));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(games.iter().map(Game::id).collect::<Vec<_>>(), ["roses", "fake-fm"]);
        let game = &games[1];
        let (events, received) = std::sync::mpsc::channel();
        // Only the manifest's kinds of disc file are taken.
        let iso = dir.join("disc.iso");
        fs::write(&iso, b"x").unwrap();
        assert!(import(game, &iso, &[], &events).unwrap_err().contains(".cue or .bin"));
        let cue = dir.join("My Disc (Europe).cue");
        fs::write(&cue, "FILE \"My Disc (Europe).bin\" BINARY\r\n").unwrap();
        let imported = import(game, &cue, &[], &events).unwrap();
        assert!(imported.marker.starts_with(dir.join("data").join("fake-fm")), "{}", imported.marker.display());
        assert!(imported.marker.parent().unwrap().file_name().unwrap().to_string_lossy().starts_with("game-"));
        assert!(fs::read_to_string(&imported.marker).unwrap().contains("My Disc (Europe).cue"));
        assert_eq!(imported.verified.unwrap().summary.as_deref(), Some("722 cards"));
        let log: String = received.try_iter().filter_map(|e| if let Event::Log(t) = e { Some(t) } else { None }).collect();
        assert!(log.contains("[1/2] Checking the disc") && log.contains("[2/2] Done"), "{log}");
        // A single .bin is taken too.
        let bin = dir.join("My Disc (Europe).bin");
        fs::write(&bin, b"raw").unwrap();
        assert!(import(game, &bin, &[], &events).is_ok());
        // An import folder without its marker does not verify.
        let empty = dir.join("data").join("fake-fm").join("game-empty");
        fs::create_dir_all(&empty).unwrap();
        assert_eq!(game.verify_import(&empty, &empty.join("game.json")).unwrap_err(), "no game.json");
        // The start-up check finds the newest usable import.
        let found = game.check_imports().unwrap().unwrap();
        assert_eq!(found.summary.as_deref(), Some("722 cards"));
        assert!(found.import_dir.unwrap().ends_with("fake-fm/game-newest"), "the check names the import it found");
        // Play starts the game with the newest import's folder.
        fs::write(game.game_program(), b"").unwrap();
        let newest = game.latest_marker().unwrap();
        let command = game.launch_command(true, &[], None).unwrap();
        let args: Vec<OsString> = command.get_args().map(OsString::from).collect();
        assert_eq!(args, [OsString::from("--data"), newest.parent().unwrap().into()]);
        let env: Vec<_> = command.get_envs().map(|(k, v)| (k.to_owned(), v.map(OsString::from))).collect();
        assert!(env.contains(&(contract::ENV_DATA_DIR.into(), Some(dir.join("data").join("fake-fm").into()))));
        let _ = fs::remove_dir_all(&dir);
    }

    /// The built-in Roses manifest against a real install and import: the
    /// game's own `roses.exe verify-import` answers.
    #[test]
    #[ignore = "needs ROSES_LAUNCHER_TEST_INSTALL (a folder with roses.exe) and ROSES_LAUNCHER_TEST_DATA (roses-data with an import)"]
    fn roses_checks_a_real_import_with_its_own_program() {
        let install = PathBuf::from(std::env::var_os("ROSES_LAUNCHER_TEST_INSTALL").expect("ROSES_LAUNCHER_TEST_INSTALL"));
        let data = PathBuf::from(std::env::var_os("ROSES_LAUNCHER_TEST_DATA").expect("ROSES_LAUNCHER_TEST_DATA"));
        let (games, _) = load_registry(&install, &data);
        let roses = games.into_iter().find(|g| g.id() == "roses").unwrap();
        let found = roses.check_imports().unwrap().expect("an import");
        assert!(found.summary.as_deref().unwrap().starts_with("854 cards, 26540 fusions"), "{found:?}");
        let import = found.import_dir.unwrap();
        let verified = roses.verify_import(&import, &import.join("game.json")).unwrap();
        assert_eq!(verified.summary, found.summary);
        assert!(roses.verify_import(&data, &data.join("game.json")).is_err(), "the data folder itself is no import");
    }

    #[test]
    fn the_platform_name_and_links_come_from_platform_json() {
        let dir = temp("platform");
        let (p, w) = Platform::load(&dir);
        assert_eq!((p.name.as_str(), w.len()), (PLATFORM_NAME_PLACEHOLDER, 0));
        assert_eq!(p.launcher_releases, None);
        fs::write(
            dir.join(PLATFORM_FILE),
            r#"{"name": "  Brand  ", "launcher_releases": "https://example.org/releases", "launcher_issues": "http://insecure.example"}"#,
        )
        .unwrap();
        let (p, w) = Platform::load(&dir);
        assert_eq!(p.name, "Brand");
        assert_eq!(p.launcher_releases.as_deref(), Some("https://example.org/releases"));
        assert_eq!(p.launcher_issues, None);
        assert_eq!(w.len(), 1);
        fs::write(dir.join(PLATFORM_FILE), "{nope").unwrap();
        assert_eq!(Platform::load(&dir).0, Platform::default());
        assert!(open_link("file:///C:/Windows/notepad.exe").is_err());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_refused_disc_reads_as_one_plain_line() {
        let stderr = "Verifying the complete PAL ISO fingerprint…\nroses: This disc image doesn't match the supported PAL release (SLES_515.79). Modified or other-region images can't be imported.\n";
        assert_eq!(
            import_failure("exit 2", stderr, None, "disc image"),
            "This disc image doesn't match the supported PAL release (SLES_515.79). Modified or other-region images can't be imported.\nYour source disc image was not modified."
        );
        // Any other failure keeps the importer's words and the partial folder.
        let other = import_failure("exit 2", "roses: out of disk space\n", Some(Path::new("data")), "disc image");
        assert!(other.starts_with("Import failed (exit 2).\nroses: out of disk space\n"), "{other}");
        assert!(other.contains("Any partial output remains at data."), "{other}");
    }
}
