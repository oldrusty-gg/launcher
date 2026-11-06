# Roses launcher

A native Windows launcher. **Import ISO** runs `roses import-game`, which
imports everything playable so far from a supported, user-provided PAL ISO
into one new local folder: the card library (854 English card names and
statistics, 26,540 static fusions, 46 terrain maps), the opening and story to
the campaign map, the campaign map, and the first duel's rules data, board and
monster models. **Play** starts the game (`roses-bevy-scene.exe --game
<game.json>`) at the title screen with the newest completed import. Later
campaign duels, most opponents, the slot machine, deck editing, saving and
online play are unfinished; the game shows explicit boundaries there.

The game's movies are copied from the ISO as they are and decoded by the
game's built-in Rust decoder; nothing else needs installing. Nothing is
downloaded or installed by the launcher.

Build the executables together, using cached dependencies if available:

```powershell
cargo build --offline -p roses-cli -p roses-launcher
cargo +stable build --offline --manifest-path crates/roses-game/Cargo.toml --features renderer --bin roses-bevy-scene
```

Keep `roses-launcher.exe`, `roses.exe` and `roses-bevy-scene.exe` together. Run
the launcher and select **Import ISO**, which asks for an uncompressed `.iso`
(or uses `--iso`). The CLI checks the complete supported-disc fingerprint before
reading any part. No ISO, original game assets, download service,
or installer is included in the launcher.

Until Simon's scene and the history clouds are derived from the disc, the game
can use private research captures from a development workspace:
`--research-root <workspace>` is passed on to the game. Without it those two
parts are missing and the game says so.

By default, imports are stored in `roses-data` beside the executables. The folder
must be writable. Each import creates a separate timestamped `game-*` directory;
earlier imports and the source ISO are not changed. A game import counts as
complete only once its `game.json` exists; older `import-*` library folders are
still loaded. The latest valid completed import
under this folder is loaded on startup (the launcher asks the game: `roses.exe
verify-import --data-root <roses-data>`, and after an import `roses.exe
verify-import --import <game-*>`; it reads the one report line, see
`platform_sdk::contract::ImportReport`); invalid newer imports are skipped so an
older valid library remains accessible. When none validate, the activity log
shows bounded diagnostic examples. A failed or interrupted import without a
manifest is not offered as a completed library; partial files remain available
for inspection.

The following optional arguments select a portable data folder and prefill the
ISO field. Prefilling does not start an import:

```powershell
.\target\debug\roses-launcher.exe --data-root work\launcher-data --iso 'work\disc\Yu-Gi-Oh! The Duelists of the Roses (Europe).iso'
```

Importing runs in a worker thread through the sibling CLI, with separate process
arguments and no command shell. The window remains responsive and displays
elapsed time and activity. A second import is disabled until completion; closing
the window during a running operation asks the user to wait. There is no cancel
button or implied cancellation. The child process runs without a console window.
Stdout and stderr are drained completely while retaining at most 64 KiB each.
Metadata files are capped at 2 MiB (8 MiB for the larger rules file). Recorded
source hashes, schemas, profiles and language must match the supported importer;
card IDs/counts, rule counts, and required output files are checked before display.
These cache checks detect incomplete or mixed imports. They do not rehash the ISO
or authenticate the contents of edited JSON or preview files.

## Window

A fixed-size, borderless window (per-monitor DPI aware v2, scaled down when the
work area is small) in the layout of the OpenGOAL launcher, drawn entirely by
the launcher itself: a title bar with the oldrusty mark and `[oldrusty]` wordmark, version, help, minimise
and close; a sidebar (game, settings); a dismissable beta banner with Known
issues (opens `KNOWN-ISSUES.txt` beside the exe) and Report a problem; and a
bottom-right stack with the title, "Played For hh:mm:ss" (summed game run time,
kept in `roses-data/launcher-state.json`), the primary button (Import ISO with
inline step progress parsed from the importer's `[n/12] step` lines, then Play),
Saves, Report a problem and Settings. Beside Play, a game's manifest may
declare a launch toggle (`launch_toggle`; Roses: "Online Play and extras", on
by default; off starts the game with `--faithful`, the original only); its
state is kept per game in `launcher-state.json` (`toggles`) and flips with a
click, Space or Enter (Tab reaches it after Play). The settings page shows the import, the
data/saves/logs folders (each with a button), Re-import and the controls.

No game asset ships with the launcher. Before an import the page is a plain
near-black; afterwards it shows the original title art from the player's own
import (`story/textures/texetc-00.png`, the logo part), which then stands in
for the drawn title text. The only drawing of our own is the flat oldrusty
mark (`src/mark.rs`: rust-red `[ ]` brackets around a gold "o" on a dark
rounded square, the website's colours) in the title bar and the exe icon;
`build.rs` renders it into a hand-written `.res` linked into the exe (MSVC
only). Before an import a game's sidebar tile shows its initials
(`art.initials`, Roses "DotR"), afterwards the logo from the player's import.

Every button is an owner-drawn BUTTON and every text an owner-drawn STATIC, so
Tab/Shift+Tab, Enter/Space, Escape (leave settings), BM_CLICK automation
(`scripts/release/first-run.ps1`) and screen readers work. Screenshots of each
state: `scripts/release/capture-window.ps1` (PrintWindow), or headless with
`ROSES_LAUNCHER_CAPTURE=<file.png>`: the window is never shown, the page is
drawn off-screen once the game is ready, and the launcher exits.

Video clips (a tool, `src/record.rs`): `ROSES_LAUNCHER_RECORD=<new.mp4>`
works like the capture, then draws a short clip frame by frame at 1800x1080
with a drawn mouse cursor and writes it as H.264/AAC (silent) 1920x1080 at
60 fps through Windows Media Foundation (`crates/platform-video`, the games'
segment format). `ROSES_LAUNCHER_RECORD_SCRIPT` scripts it, e.g. the
default `wait:1.2,move:game:roses:1.1,click,wait:0.5,move:play:0.9,wait:0.2,click,wait:1.3`
(steps `wait:S`, `move:game:<id>|play|import|toggle|X;Y:S`, `click`,
`pad:<dir>`). A click on a game's sidebar entry shows that game; a click on
Play is only drawn pressed: nothing is started.

## Games, platform name and links

The launcher is game-neutral (`src/registry.rs`): every game is a
`platform_sdk` manifest (`games/roses.json` is built in; others are read
from `games/<id>/game.json` or `games/<id>.json` beside the exe, at most
six). Each gets its own sidebar entry, data folder, import, Play, settings,
notice and play time; Roses keeps `roses-data` itself as its folder, so
existing installs are unchanged. Import and Play run the programs and
arguments the manifest names (Roses: `roses.exe import-game ...` and
`roses-bevy-scene.exe --game <game.json>`), with the `PLATFORM_*`
environment of the launcher-game contract.

The launcher depends on no game crate: only `platform-sdk` (the manifest and
the launcher-game contract) and `platform-net` (the Profile page's account
connection), both from the platform-sdk repository. Everything game-specific
comes from the manifest: the import check (`importer.verify` /
`importer.check`), the development options a game takes (`dev_options`), its
art (`art`: the sidebar mark, the backdrop and logo inside the import) and
the fields its server wants in an account-only hello
(`online.account_hello`, Roses: its protocol `compat`).

A manifest may name the kind of disc file its importer takes
(`"disc": {"extensions": ["cue", "bin"]}`; default `["iso"]`): the file
picker's filter, the import's file check and the texts ("Import disc
image") follow it. Yu-Gi-Oh! Forbidden Memories (repository
`forbidden2026`) is the second game this way: its own
`games/forbidden-memories.json` plus `fm.exe`, `fm-game.exe` and
`assets/present.wgsl` beside the launcher (its
`scripts/stage-launcher.ps1` stages exactly that), data in
`roses-data/forbidden-memories`. Nothing of it is built into the launcher;
`scripts/build-release.ps1 -ForbiddenMemories <stage>` packages it, off by
default. `ROSES_LAUNCHER_GAME=<id>` shows that game first (for
`ROSES_LAUNCHER_CAPTURE` screenshots of its page).

`platform.json` beside the exe (optional) sets the platform's name (a
brand, "oldrusty" when unset; the header draws it as the wordmark
`[oldrusty]`) and its links:
`{"name": "...", "launcher_releases": "https://...", "launcher_update_api": "https://api.github.com/...", "launcher_issues": "https://...", "website": "https://..."}`.
The title bar shows the launcher's and the shown game's versions and a
"Get the latest launcher" link, which becomes an Update button when
`launcher_update_api`'s latest release is newer than `ROSES_RELEASE` and
carries `latest.json` (`src/updater.rs` has the whole flow; a development
build never checks). Known issues and Report a problem use the
game's manifest `links` (https only, opened in the system browser) and
fall back to `KNOWN-ISSUES.txt` and the logs folder until they are set.

When the server sends email (its welcome says so), the Profile page shows
the account's address state, takes an address and the mailed code, and
offers "Forgot your password?" (a reset code by email). Its jobs are
`platform_net::profile`; the end-to-end tests against a real hub live with the
server (`crates/roses-server/tests/launcher_*.rs`). See
`work/research/platform/PLATFORM-PLAN.md`.

## Verification

```powershell
cargo test --offline -p roses-launcher
cargo clippy --offline -p roses-launcher --all-targets -- -D warnings
```

Synthetic tests exercise path argument isolation, output bounds, option parsing,
development options per game, import-check reports (scripts standing in for a
game's check), and unique output paths. The card-library checks themselves
(metadata bounds, card-name ID joins, duplicate/missing names, mixed metadata,
required outputs, recovery from a damaged latest import) are the game's:
`roses_assets::library`. An ignored test runs the built-in Roses manifest's
check with a real `roses.exe` and import:

```powershell
$env:ROSES_LAUNCHER_TEST_INSTALL = (Resolve-Path 'target\debug').Path   # holds roses.exe
$env:ROSES_LAUNCHER_TEST_DATA = (Resolve-Path 'work\play
oses-data').Path
cargo test --offline -p roses-launcher roses_checks_a_real_import -- --ignored
```

Interactive QA on 27 September 2026 verified the visible title and initial sizing,
empty ISO validation, Browse/cancel, a real supported PAL import, card selection,
width resizing and startup library reload. The import finished before the busy
controls could be inspected. Duplicate-import prevention, progress responsiveness,
close during import, and recovery from a damaged cache therefore remain code/test
checks rather than claimed interactive checks. No gameplay is implemented here.
