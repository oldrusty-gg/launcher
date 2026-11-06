//! The swap: a running exe cannot be replaced, so a copy of the launcher
//! ([`start_helper`], `--apply-update`) does it after the launcher has closed
//! and then starts the launcher again.
//!
//! Layout of the staging folder: `new/` the unpacked release, `backup/` the
//! replaced files, `undone/` files moved out by a rollback, `helper/` the
//! copy of the launcher, `swap.journal` every file of the swap, marked `=`
//! (replaces a file) or `+` (added; written before the first move),
//! `swap.lock` held while the helper works,
//! `result.json` what the next launcher tells the player.
//!
//! Each file moves old -> `backup/`, then `new/` -> place, all renames on one
//! disk; the launcher goes last. Renames alone would not notice a program
//! that is running (Windows lets those be renamed), so every target is first
//! opened exclusively ([`in_use`]). A swap that fails, or a helper killed
//! half way (the next launcher finds the journal), is undone file by file.

use super::{DATA_DIR, LAUNCHER_EXE, STAGING_DIR};
use serde::{Deserialize, Serialize};
use std::ffi::OsString;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread::sleep;
use std::time::{Duration, Instant};

/// The helper's first argument.
pub const APPLY_ARG: &str = "--apply-update";
const HELPER_EXE: &str = "roses-update-helper.exe";
/// How long the helper waits for the launcher to close and files to free up.
const WAIT_FOR_FILES: Duration = Duration::from_secs(30);

/// The staging folder of an install.
#[derive(Clone, Debug)]
pub struct Staging {
    pub install: PathBuf,
    pub root: PathBuf,
}

/// What became of an update; written by the helper, read by the next launcher.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    pub ok: bool,
    pub message: String,
}

impl Staging {
    pub fn of(install: &Path) -> Self {
        Self { install: install.to_path_buf(), root: install.join(STAGING_DIR) }
    }
    pub fn new_dir(&self) -> PathBuf {
        self.root.join("new")
    }
    fn backup_dir(&self) -> PathBuf {
        self.root.join("backup")
    }
    fn undone_dir(&self) -> PathBuf {
        self.root.join("undone")
    }
    fn journal(&self) -> PathBuf {
        self.root.join("swap.journal")
    }
    fn result(&self) -> PathBuf {
        self.root.join("result.json")
    }
    fn lock(&self) -> PathBuf {
        self.root.join("swap.lock")
    }

    /// Whether a helper is at work (it holds `swap.lock` open).
    fn busy(&self) -> bool {
        matches!(fs::remove_file(self.lock()), Err(e) if e.kind() != io::ErrorKind::NotFound)
    }

    /// Delete the staging folder, unless a helper is at work or a rollback is
    /// still owed (the backups are the old version then).
    pub fn remove(&self) {
        if !self.root.exists() || self.busy() || self.journal().exists() {
            return;
        }
        // The helper that started this launcher may be closing just now.
        for _ in 0..10 {
            if fs::remove_dir_all(&self.root).is_ok() || !self.root.exists() {
                return;
            }
            sleep(Duration::from_millis(300));
        }
    }

    fn write_result(&self, outcome: &Outcome) {
        if let Ok(text) = serde_json::to_string(outcome) {
            let _ = fs::write(self.result(), text);
        }
    }
}

/// Open `path` so nobody else can have it: fails for a running program and
/// for a file another program holds without sharing.
#[cfg(windows)]
fn open_alone(path: &Path, create: bool) -> io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    OpenOptions::new().read(true).write(true).create(create).truncate(false).share_mode(0).open(path)
}

#[cfg(not(windows))]
fn open_alone(path: &Path, create: bool) -> io::Result<File> {
    OpenOptions::new().read(true).write(true).create(create).truncate(false).open(path)
}

/// Every file under `root` as relative paths; the launcher last, so a swap
/// that stops early leaves the old launcher in place.
pub fn files_under(root: &Path) -> io::Result<Vec<PathBuf>> {
    fn walk(root: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
        for entry in fs::read_dir(root.join(rel))? {
            let entry = entry?;
            let path = rel.join(entry.file_name());
            if entry.file_type()?.is_dir() {
                walk(root, &path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    walk(root, Path::new(""), &mut out)?;
    out.sort_by_key(|path| (path.as_os_str().eq_ignore_ascii_case(LAUNCHER_EXE), path.clone()));
    Ok(out)
}

/// The first of `files` (relative to `install`) that exists and cannot be
/// opened alone.
pub fn in_use(install: &Path, files: &[PathBuf]) -> Option<PathBuf> {
    files.iter().find(|rel| match open_alone(&install.join(rel), false) {
        Ok(_) => false,
        Err(e) => e.kind() != io::ErrorKind::NotFound,
    }).cloned()
}

/// The first of the install's programs in use, leaving out `running` (this
/// launcher): a game or importer running, from this launcher or any other.
pub fn program_in_use(install: &Path, running: &Path) -> Option<PathBuf> {
    fn programs(install: &Path, rel: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = fs::read_dir(install.join(rel)) else { return };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let path = rel.join(&name);
            if entry.file_type().is_ok_and(|t| t.is_dir()) {
                if name != DATA_DIR && name != STAGING_DIR {
                    programs(install, &path, out);
                }
            } else if path.extension().is_some_and(|e| e.eq_ignore_ascii_case("exe")) {
                out.push(path);
            }
        }
    }
    let mut files = Vec::new();
    programs(install, Path::new(""), &mut files);
    files.retain(|rel| install.join(rel) != running);
    in_use(install, &files)
}

/// A rename, tried a few times: virus scanners hold new files for a moment.
fn move_file(from: &Path, to: &Path) -> io::Result<()> {
    if let Some(parent) = to.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut result = fs::rename(from, to);
    for _ in 0..5 {
        if result.is_ok() {
            break;
        }
        sleep(Duration::from_millis(200));
        result = fs::rename(from, to);
    }
    result
}

/// Move `files` from `new/` into place. `fail_after`: the test seam's failure
/// before that file. On an error the caller rolls back.
pub fn swap(staging: &Staging, files: &[PathBuf], fail_after: Option<usize>) -> Result<(), String> {
    let list: Vec<String> = files
        .iter()
        .map(|rel| format!("{}{}", if staging.install.join(rel).exists() { '=' } else { '+' }, rel.display()))
        .collect();
    fs::write(staging.journal(), list.join("\n")).map_err(|e| format!("Could not start the swap ({e})."))?;
    for (index, rel) in files.iter().enumerate() {
        if fail_after == Some(index) {
            return Err("The test seam stopped the swap.".into());
        }
        let target = staging.install.join(rel);
        let failed = |e: io::Error| format!("Could not replace {} ({e}).", rel.display());
        if target.exists() {
            move_file(&target, &staging.backup_dir().join(rel)).map_err(failed)?;
        }
        move_file(&staging.new_dir().join(rel), &target).map_err(failed)?;
    }
    Ok(())
}

/// Undo a swap from its journal, however far it came, and safe to repeat: a
/// replaced file with a backup gets it back (no backup: not reached, or back
/// already); an added file whose new copy has left `new/` goes. The journal
/// stays until every file is back, so a later launcher can try again.
pub fn rollback(staging: &Staging) -> Result<(), String> {
    let list = fs::read_to_string(staging.journal()).map_err(|e| format!("The list of swapped files could not be read ({e})."))?;
    let mut failed = Vec::new();
    for line in list.lines().rev() {
        let (added, rel) = (line.starts_with('+'), Path::new(line.get(1..).unwrap_or_default()));
        let target = staging.install.join(rel);
        let backup = staging.backup_dir().join(rel);
        let undone = staging.undone_dir().join(rel);
        let result = if !added && backup.exists() {
            let clear = if target.exists() { move_file(&target, &undone) } else { Ok(()) };
            clear.and_then(|()| move_file(&backup, &target))
        } else if added && !staging.new_dir().join(rel).exists() && target.exists() {
            let moved = move_file(&target, &undone);
            // A folder only this update made.
            let mut parent = rel.parent();
            while let Some(folder) = parent.filter(|p| !p.as_os_str().is_empty()) {
                if fs::remove_dir(staging.install.join(folder)).is_err() {
                    break;
                }
                parent = folder.parent();
            }
            moved
        } else {
            Ok(())
        };
        if let Err(e) = result {
            failed.push(format!("{} ({e})", rel.display()));
        }
    }
    if failed.is_empty() {
        let _ = fs::remove_file(staging.journal());
        Ok(())
    } else {
        Err(failed.join(", "))
    }
}

/// The result of a failed swap, after rolling it back.
fn undo(staging: &Staging, why: &str) -> Outcome {
    match rollback(staging) {
        Ok(()) => Outcome { ok: false, message: format!("The update did not finish, so the old version was put back. {why}") },
        Err(files) => Outcome {
            ok: false,
            message: format!(
                "The update failed and these files could not be put back yet: {files}. Close other programs and start the launcher again to retry; the old files are in {}. {why}",
                staging.root.join("backup").display()
            ),
        },
    }
}

/// The helper's work, without the restart.
pub fn apply(staging: &Staging, version: &str, wait: Duration, fail_after: Option<usize>) -> Outcome {
    let files = match files_under(&staging.new_dir()) {
        Ok(files) if !files.is_empty() => files,
        _ => return Outcome { ok: false, message: "The update's files are missing. Nothing was changed.".into() },
    };
    let deadline = Instant::now() + wait;
    while let Some(busy) = in_use(&staging.install, &files) {
        if Instant::now() >= deadline {
            return Outcome {
                ok: false,
                message: format!("{} is in use by another program, so the update was not installed. Nothing was changed. Close it and try again.", busy.display()),
            };
        }
        sleep(Duration::from_millis(250));
    }
    match swap(staging, &files, fail_after) {
        Ok(()) => Outcome { ok: true, message: format!("Updated to {version}.") },
        Err(why) => undo(staging, &why),
    }
}

/// Copy the running launcher into the staging folder and start it as the
/// helper. The caller then closes.
pub fn start_helper(install: &Path, version: &str, data_root: &Path) -> Result<(), String> {
    let staging = Staging::of(install);
    let helper = staging.root.join("helper").join(HELPER_EXE);
    let start = || -> io::Result<()> {
        fs::create_dir_all(staging.root.join("helper"))?;
        fs::copy(std::env::current_exe()?, &helper)?;
        Command::new(&helper).arg(APPLY_ARG).arg(install).arg(version).arg(data_root).current_dir(install).spawn()?;
        Ok(())
    };
    start().map_err(|e| format!("Could not start the update helper ({e})."))
}

/// `--apply-update <install> <version> <data root>` of this process, if any.
pub fn helper_args() -> Option<(PathBuf, String, OsString)> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != APPLY_ARG {
        return None;
    }
    let install = PathBuf::from(args.next()?);
    let version = args.next()?.to_string_lossy().into_owned();
    Some((install, version, args.next()?))
}

/// The helper process: swap, leave the result, start the launcher (the new
/// one, or the old one after a failure). An error means no launcher started.
pub fn run_helper(install: &Path, version: &str, data_root: &OsString) -> Result<(), String> {
    let staging = Staging::of(install);
    // With the test seam, ROSES_UPDATE_TEST_FAIL_AFTER breaks the swap there.
    let fail_after = if cfg!(feature = "update-test-seam") {
        std::env::var("ROSES_UPDATE_TEST_FAIL_AFTER").ok().and_then(|n| n.parse().ok())
    } else {
        None
    };
    let lock = open_alone(&staging.lock(), true);
    let outcome = match &lock {
        Ok(_) => apply(&staging, version, WAIT_FOR_FILES, fail_after),
        Err(_) => Outcome { ok: false, message: "Another update is running. Nothing was changed.".into() },
    };
    staging.write_result(&outcome);
    if outcome.ok {
        let _ = fs::remove_file(staging.journal());
    }
    drop(lock);
    Command::new(install.join(LAUNCHER_EXE))
        .arg("--data-root")
        .arg(data_root)
        .current_dir(install)
        .spawn()
        .map(drop)
        .map_err(|e| format!("{} Could not start the launcher again ({e}); start it yourself.", outcome.message))
}

/// At launcher start: what became of the last update, if anything is to say.
/// A swap cut off half way (the journal without a result) is rolled back now.
pub fn finish_previous(install: &Path) -> Option<Outcome> {
    let staging = Staging::of(install);
    if !staging.root.exists() || staging.busy() {
        return None;
    }
    if let Ok(text) = fs::read_to_string(staging.result()) {
        let _ = fs::remove_file(staging.result());
        if let Ok(outcome) = serde_json::from_str::<Outcome>(&text) {
            if outcome.ok {
                let _ = fs::remove_file(staging.journal());
            }
            return Some(outcome);
        }
    }
    staging.journal().exists().then(|| undo(&staging, "It was interrupted."))
}

#[cfg(test)]
mod tests {
    use super::super::test_support::folder;
    use super::*;

    /// An install with old files, data and a staged new version.
    fn install() -> Staging {
        let dir = folder("swap");
        for (name, body) in [
            ("roses-launcher.exe", "old launcher"),
            ("roses.exe", "old importer"),
            ("README.txt", "old readme"),
            ("gone-in-new.txt", "stays"),
            ("assets/a.wgsl", "old shader"),
            ("roses-data/memory-card/card.bin", "save"),
        ] {
            let path = dir.join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        let staging = Staging::of(&dir);
        for (name, body) in [
            ("roses-launcher.exe", "new launcher"),
            ("roses.exe", "new importer"),
            ("README.txt", "new readme"),
            ("assets/a.wgsl", "new shader"),
            ("assets/deep/added.wgsl", "added"),
        ] {
            let path = staging.new_dir().join(name);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, body).unwrap();
        }
        staging
    }

    fn read(staging: &Staging, name: &str) -> String {
        fs::read_to_string(staging.install.join(name)).unwrap_or_else(|e| format!("{name}: {e}"))
    }

    fn is_old(staging: &Staging) {
        assert_eq!(read(staging, "roses-launcher.exe"), "old launcher");
        assert_eq!(read(staging, "roses.exe"), "old importer");
        assert_eq!(read(staging, "README.txt"), "old readme");
        assert_eq!(read(staging, "assets/a.wgsl"), "old shader");
        assert!(!staging.install.join("assets/deep").exists(), "the added folder is gone");
        assert_eq!(read(staging, "roses-data/memory-card/card.bin"), "save");
    }

    #[test]
    fn the_launcher_is_swapped_last() {
        let staging = install();
        let files = files_under(&staging.new_dir()).unwrap();
        assert_eq!(files.len(), 5);
        assert_eq!(files.last().unwrap(), Path::new("roses-launcher.exe"));
    }

    #[test]
    fn a_swap_replaces_the_programs_and_nothing_else() {
        let staging = install();
        let outcome = apply(&staging, "2026-11-07", Duration::ZERO, None);
        assert!(outcome.ok, "{outcome:?}");
        assert_eq!(read(&staging, "roses-launcher.exe"), "new launcher");
        assert_eq!(read(&staging, "assets/a.wgsl"), "new shader");
        assert_eq!(read(&staging, "assets/deep/added.wgsl"), "added");
        assert_eq!(read(&staging, "gone-in-new.txt"), "stays");
        assert_eq!(read(&staging, "roses-data/memory-card/card.bin"), "save");
    }

    #[test]
    fn a_swap_stopped_at_any_file_is_rolled_back() {
        for stop in 0..5 {
            let staging = install();
            let outcome = apply(&staging, "2026-11-07", Duration::ZERO, Some(stop));
            assert!(!outcome.ok);
            assert!(outcome.message.contains("put back"), "{}", outcome.message);
            is_old(&staging);
            assert!(!staging.journal().exists());
        }
    }

    #[test]
    fn a_locked_file_stops_the_update_before_anything_moves() {
        let staging = install();
        let held = open_alone(&staging.install.join("roses.exe"), false).unwrap();
        let outcome = apply(&staging, "2026-11-07", Duration::ZERO, None);
        assert!(!outcome.ok);
        assert!(outcome.message.contains("roses.exe is in use"), "{}", outcome.message);
        assert!(outcome.message.contains("Nothing was changed"));
        assert!(!staging.journal().exists(), "the swap did not start");
        drop(held);
        is_old(&staging);
    }

    #[test]
    fn a_file_locked_during_the_swap_rolls_back() {
        let staging = install();
        let files = files_under(&staging.new_dir()).unwrap();
        // After the check for locks: straight to the swap.
        let held = open_alone(&staging.install.join("roses.exe"), false).unwrap();
        let why = swap(&staging, &files, None).unwrap_err();
        assert!(why.contains("roses.exe"), "{why}");
        drop(held);
        rollback(&staging).unwrap();
        is_old(&staging);
    }

    #[test]
    fn a_rollback_that_fails_keeps_its_journal_and_can_retry() {
        let staging = install();
        let files = files_under(&staging.new_dir()).unwrap();
        swap(&staging, &files, Some(3)).unwrap_err();
        // A swapped-in file held by something: it cannot be moved out.
        let held = open_alone(&staging.install.join("README.txt"), false).unwrap();
        let outcome = undo(&staging, "why");
        assert!(outcome.message.contains("README.txt"), "{}", outcome.message);
        assert!(staging.journal().exists());
        staging.remove();
        assert!(staging.root.exists(), "the backups are kept while a rollback is owed");
        drop(held);
        let retried = finish_previous(&staging.install).unwrap();
        assert!(retried.message.contains("put back"), "{}", retried.message);
        is_old(&staging);
    }

    #[test]
    fn a_helper_killed_half_way_is_undone_at_the_next_start() {
        let staging = install();
        let files = files_under(&staging.new_dir()).unwrap();
        swap(&staging, &files, Some(2)).unwrap_err();
        let outcome = finish_previous(&staging.install).unwrap();
        assert!(outcome.message.contains("interrupted"), "{}", outcome.message);
        is_old(&staging);
    }

    #[test]
    fn a_working_helper_is_left_alone() {
        let staging = install();
        let lock = open_alone(&staging.lock(), true).unwrap();
        assert_eq!(finish_previous(&staging.install), None);
        staging.remove();
        assert!(staging.new_dir().exists());
        drop(lock);
        staging.remove();
        assert!(!staging.root.exists());
    }

    #[test]
    fn the_result_is_told_once() {
        let staging = install();
        let outcome = Outcome { ok: true, message: "Updated to 2026-11-07.".into() };
        staging.write_result(&outcome);
        assert_eq!(finish_previous(&staging.install), Some(outcome));
        assert_eq!(finish_previous(&staging.install), None);
    }

    #[test]
    fn programs_in_use_are_found_except_this_launcher() {
        let staging = install();
        let launcher = staging.install.join("roses-launcher.exe");
        assert_eq!(program_in_use(&staging.install, &launcher), None);
        let held = open_alone(&launcher, false).unwrap();
        assert_eq!(program_in_use(&staging.install, &launcher), None);
        let game = open_alone(&staging.install.join("roses.exe"), false).unwrap();
        assert_eq!(program_in_use(&staging.install, &launcher), Some(PathBuf::from("roses.exe")));
        drop((held, game));
    }
}
