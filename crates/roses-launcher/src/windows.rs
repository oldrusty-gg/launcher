//! The launcher window: borderless, fixed size, custom drawn. The UI thread owns
//! every HWND and all display state; worker threads only send bounded text and
//! completed library data through the channel.
//!
//! Every clickable element is a real BUTTON child (owner-drawn) and every text a
//! real STATIC child (owner-drawn), so Tab/Enter/Space, BM_CLICK automation and
//! screen readers keep working; the window behind them is painted from a cached
//! backdrop (our own drawing, or art from the player's own import).

use crate::canvas::{self, mix, rgb, Rgba, Surface};
use crate::pad::{self, Action, Dir, Target};
use roses_launcher::registry::{Game, Platform};
use roses_launcher::updater::{self, swap, Available, Check, UpdateEvent};
use roses_launcher::{Event, LauncherState, Options, Verified};
use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Instant;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_ROUND,
};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Controls::Dialogs::*;
use windows_sys::Win32::UI::Controls::{
    DRAWITEMSTRUCT, EM_LIMITTEXT, EM_SETMARGINS, ODS_DISABLED, ODS_FOCUS, ODS_NOFOCUSRECT, ODS_SELECTED, WM_MOUSELEAVE,
};
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForWindow, GetSystemMetricsForDpi, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, GetFocus, IsWindowEnabled, SetFocus, TrackMouseEvent, TME_LEAVE, TRACKMOUSEEVENT,
};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

// Buttons. IMPORT and PLAY keep their ids and captions: scripts/release/
// first-run.ps1 finds them by caption and clicks them with BM_CLICK.
const IMPORT: usize = 102;
const PLAY: usize = 105;
const REPORT: usize = 106;
const SAVES: usize = 107;
const GEAR: usize = 108;
/// The first game's sidebar entry; game `i` > 0 is `NAV_GAMES + i`.
const NAV_GAME: usize = 110;
const NAV_GAMES: usize = 300;
const NAV_SETTINGS: usize = 111;
const HELP: usize = 112;
const MINIMIZE: usize = 113;
const CLOSE: usize = 114;
const KNOWN_ISSUES: usize = 115;
const BANNER_REPORT: usize = 116;
const BANNER_CLOSE: usize = 117;
const S_REIMPORT: usize = 120;
const S_DATA: usize = 121;
const S_SAVES: usize = 122;
const S_LOGS: usize = 123;
// The Profile page (friends-online account).
const NAV_PROFILE: usize = 118;
/// "Signed in as ..." in the bottom-right stack (opens the Profile page).
const ACCOUNT: usize = 119;
const P_MODE_LOGIN: usize = 130;
const P_MODE_SIGNUP: usize = 131;
const P_SUBMIT: usize = 132;
const P_LOGOUT: usize = 133;
const P_CHANGE: usize = 134;
const P_CANCEL: usize = 135;
const P_RETRY: usize = 136;
// Text fields (EDIT) and their captions (E_* + FIELD_CAPTION).
const E_NAME: usize = 140;
const E_PASSWORD: usize = 141;
const E_NEW: usize = 142;
const E_CONFIRM: usize = 143;
const E_INVITE: usize = 144;
/// The account's email address (also "name or address" when resetting).
const E_EMAIL: usize = 145;
/// A code from an email.
const E_CODE: usize = 146;
const P_EMAIL_SEND: usize = 137;
const P_EMAIL_VERIFY: usize = 138;
const P_EMAIL_LATER: usize = 139;
/// "Forgot your password?" under the log-in form.
const P_FORGOT: usize = 170;
/// The header's "latest launcher" link.
const UPDATE: usize = 171;
/// The game's launch toggle beside Play (manifest `launch_toggle`).
const TOGGLE: usize = 172;
const L_P_EMAIL_SECTION: usize = 167;
const L_P_EMAIL: usize = 168;
const FIELD_CAPTION: usize = 10;
const L_P_HEADING: usize = 160;
const L_P_SERVER_SECTION: usize = 161;
const L_P_SERVER: usize = 162;
const L_P_ACCOUNT_SECTION: usize = 163;
const L_P_ACCOUNT: usize = 164;
const L_P_MESSAGE: usize = 165;
const L_P_NOTE: usize = 166;
// Labels.
const L_BRAND: usize = 200;
const L_VERSION: usize = 201;
const L_BANNER: usize = 202;
const L_SEPARATOR: usize = 203;
const L_HERO: usize = 204;
const L_SUBTITLE: usize = 205;
const L_PROGRESS: usize = 206;
const L_STATUS: usize = 207;
const L_HEADING: usize = 210;
const L_SECTION: usize = 211; // ..=215
const L_VALUE: usize = 216; // ..=220

const TIMER: usize = 1;
/// Game pads are polled this often (ms); never in a capture.
const PAD_TIMER: usize = 2;
const PAD_POLL_MS: u32 = 30;
/// SS_OWNERDRAW (System_SystemServices, not worth another feature).
const SS_OWNERDRAW: u32 = 13;
/// The header's update link.
const UPDATE_TEXT: &str = "Get the latest launcher";

// Logical layout (96 DPI pixels); everything is scaled by App::scale.
const WIDTH: i32 = 1000;
const HEIGHT: i32 = 600;
const TITLE: i32 = 36;
const SIDEBAR: i32 = 64;
const BANNER: i32 = 34;
const STACK_RIGHT: i32 = WIDTH - 44;
/// The logo part of the imported title art (texetc-00, 640x448): rows
/// 16..316 (the copyright line below is left out), shown this wide.
const TITLE_ART_TOP: i32 = 16;
const TITLE_ART_HEIGHT: i32 = 300;
const TITLE_ART_WIDTH: i32 = 580;
/// The sidebar's game entry shows the game's logo (its manifest's
/// `art.logo` part of the backdrop) this wide (logical px) in the 64 px tile.
const GAME_ICON_WIDTH: i32 = 56;
const STACK_WIDTH: i32 = 320;
const STACK_LEFT: i32 = STACK_RIGHT - STACK_WIDTH;

// Palette.
const ACCENT: u32 = rgb(214, 58, 85);
const GOLD: u32 = rgb(229, 170, 108);
const TEXT: u32 = rgb(236, 236, 240);
const MUTED: u32 = rgb(156, 156, 166);
const ERROR_TEXT: u32 = rgb(255, 140, 140);
const FOCUS: u32 = rgb(240, 192, 136);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum View {
    Game,
    Settings,
    Profile,
}

/// Which form the Profile page shows.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Form {
    LogIn,
    SignUp,
    /// Signed in: Log out / Change password.
    Account,
    Change,
    /// Forgot the password: name or address, then "Send reset code".
    Reset,
    /// The code from the reset email and a new password.
    ResetCode,
}

/// The Profile page's state (UI thread only; jobs run on worker threads).
struct Profile {
    paths: roses_launcher::profile::Paths,
    config: Result<platform_net::config::OnlineConfig, String>,
    /// Signed in as (from the session file, then the server's answer).
    user: Option<String>,
    server_name: Option<String>,
    invite_required: Option<bool>,
    /// The server sends email (verification, password reset).
    email_offered: bool,
    /// The account's email state (servers that send email).
    email: Option<platform_net::account::AccountStatus>,
    /// The label of the last job sent (`Job::label`).
    last_job: &'static str,
    form: Form,
    busy: bool,
    /// The last check reached the server.
    reached: bool,
    message: String,
    error: bool,
    sender: Sender<Result<roses_launcher::profile::Outcome, String>>,
    receiver: Receiver<Result<roses_launcher::profile::Outcome, String>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Font {
    Body,
    Small,
    Brand,
    Mono,
    Hero,
    Subtitle,
    Primary,
    Heading,
    Section,
    Link,
    LinkHover,
    Glyph,
    /// The header's `[oldrusty]` wordmark (monospace, as on the website).
    Wordmark,
}
const FONT_COUNT: usize = 13;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Primary,
    Secondary,
    Gear,
    Title,
    Nav,
    Link,
    BannerClose,
    /// A Profile page form switch (Log in / Sign up).
    Tab,
    /// A text field (EDIT): its box is drawn on the background.
    Edit,
    /// An on/off switch (the launch toggle): a box with a tick and its
    /// caption; Space, Enter or a click flips it.
    Check,
    Label {
        font: Font,
        color: u32,
        flags: DRAW_TEXT_FORMAT,
        spacing: i32,
    },
}

struct Control {
    hwnd: HWND,
    id: usize,
    kind: Kind,
    rect: RECT,
}

/// A top-down 32-bit DIB section selected into its own memory DC.
struct Dib {
    dc: HDC,
    bitmap: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u32,
    width: i32,
    height: i32,
}

impl Dib {
    unsafe fn new(width: i32, height: i32) -> Option<Self> {
        let (width, height) = (width.max(1), height.max(1));
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: width,
                biHeight: -height,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB,
                ..Default::default()
            },
            ..Default::default()
        };
        let dc = CreateCompatibleDC(null_mut());
        if dc.is_null() {
            return None;
        }
        let mut bits = null_mut();
        let bitmap = CreateDIBSection(dc, &info, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
        if bitmap.is_null() || bits.is_null() {
            DeleteDC(dc);
            return None;
        }
        let old = SelectObject(dc, bitmap);
        Some(Self {
            dc,
            bitmap,
            old,
            bits: bits.cast(),
            width,
            height,
        })
    }

    fn pixels(&mut self) -> &mut [u32] {
        unsafe {
            GdiFlush();
            std::slice::from_raw_parts_mut(self.bits, (self.width * self.height) as usize)
        }
    }

    fn surface(&mut self) -> Surface<'_> {
        let (width, height) = (self.width, self.height);
        Surface {
            width,
            height,
            pixels: self.pixels(),
        }
    }
}

impl Drop for Dib {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.dc, self.old);
            DeleteObject(self.bitmap);
            DeleteDC(self.dc);
        }
    }
}

struct Fonts([HFONT; FONT_COUNT]);

/// Windows has the font `face` installed (the website's first monospace
/// choice, Cascadia Code, ships with Windows 11; else Consolas).
unsafe fn mono_face(face: &str) -> bool {
    unsafe extern "system" fn found(_: *const LOGFONTW, _: *const TEXTMETRICW, _: u32, hit: LPARAM) -> i32 {
        *(hit as *mut bool) = true;
        0
    }
    let dc = CreateCompatibleDC(null_mut());
    let mut logfont = LOGFONTW { lfCharSet: DEFAULT_CHARSET as u8, ..Default::default() };
    for (slot, unit) in logfont.lfFaceName.iter_mut().zip(face.encode_utf16().take(31)) {
        *slot = unit;
    }
    let mut hit = false;
    EnumFontFamiliesExW(dc, &logfont, Some(found), &mut hit as *mut bool as LPARAM, 0);
    DeleteDC(dc);
    hit
}

impl Fonts {
    unsafe fn new(scale: f32) -> Self {
        let make = |size: f32, weight: i32, face: &str, underline: bool| {
            CreateFontW(
                -((size * scale).round() as i32),
                0,
                0,
                0,
                weight,
                0,
                u32::from(underline),
                0,
                DEFAULT_CHARSET as u32,
                OUT_TT_PRECIS as u32,
                CLIP_DEFAULT_PRECIS as u32,
                CLEARTYPE_QUALITY as u32,
                (DEFAULT_PITCH | FF_SWISS) as u32,
                wide(face).as_ptr(),
            )
        };
        Self([
            make(14.0, 400, "Segoe UI", false),
            make(12.5, 400, "Segoe UI", false),
            make(15.0, 700, "Segoe UI", false),
            make(12.5, 400, "Consolas", false),
            make(40.0, 900, "Segoe UI Black", false),
            make(18.0, 600, "Segoe UI Semibold", false),
            make(20.0, 600, "Segoe UI Semibold", false),
            make(30.0, 300, "Segoe UI Light", false),
            make(11.5, 700, "Segoe UI", false),
            make(14.0, 600, "Segoe UI Semibold", false),
            make(14.0, 600, "Segoe UI Semibold", true),
            make(15.0, 600, "Segoe UI Semibold", false),
            make(15.0, 700, if mono_face("Cascadia Code") { "Cascadia Code" } else { "Consolas" }, false),
        ])
    }

    fn get(&self, font: Font) -> HFONT {
        self.0[font as usize]
    }
}

impl Drop for Fonts {
    fn drop(&mut self) {
        for font in self.0 {
            if !font.is_null() {
                unsafe { DeleteObject(font) };
            }
        }
    }
}

/// One registered game's live state: the shown game's is `App::slot`, the
/// others wait in `App::parked` (their imports and games keep running).
struct Slot {
    sender: Sender<Event>,
    receiver: Receiver<Event>,
    /// What the game's import check said of the newest import (games that
    /// check their imports).
    verified: Option<Verified>,
    /// The newest completed import's marker file (`game.json` for Roses).
    game_json: Option<PathBuf>,
    busy: bool,
    importing: bool,
    started: Instant,
    step: Option<(usize, usize, String)>,
    step_started: Instant,
    line_buffer: String,
    game_child: Option<std::process::Child>,
    game_started: Option<Instant>,
    iso: Option<PathBuf>,
    status: String,
    status_error: bool,
    /// Play time and the dismissed notice, in the game's data folder.
    state: LauncherState,
    banner: bool,
    backdrop: Option<Rgba>,
    /// The sidebar's game icon cut from `backdrop`, cached for its size.
    game_icon: Option<Rgba>,
}

impl Slot {
    /// A game's state before its imports are looked at; starts the loader.
    fn new(game: &Game, iso: Option<PathBuf>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let state = roses_launcher::load_state(&game.data_dir);
        let banner = game.manifest.notice.is_some()
            && state.banner_dismissed.as_deref() != Some(&roses_launcher::version_label());
        let events = sender.clone();
        let loader = game.clone();
        std::thread::spawn(move || {
            if loader.manifest.importer.check.is_some() {
                let result = std::panic::catch_unwind(|| loader.check_imports())
                    .unwrap_or_else(|_| Err("The library loader stopped unexpectedly.".into()));
                let _ = events.send(Event::Complete(result));
            } else {
                let _ = events.send(Event::Imported(Ok(loader.latest_marker())));
            }
        });
        Self {
            sender,
            receiver,
            verified: None,
            game_json: None,
            busy: true,
            importing: false,
            started: Instant::now(),
            step: None,
            step_started: Instant::now(),
            line_buffer: String::new(),
            game_child: None,
            game_started: None,
            iso,
            status: "Checking for your imported game…".into(),
            status_error: false,
            state,
            banner,
            backdrop: None,
            game_icon: None,
        }
    }
}

struct App {
    options: Options,
    /// The platform's name and links (`platform.json`).
    platform: Platform,
    games: Vec<Game>,
    /// The game shown (index into `games`).
    current: usize,
    slot: Slot,
    /// The other games' states (`None` at `current`).
    parked: Vec<Option<Slot>>,
    /// The game whose sidebar entry the tooltip names.
    tooltip_game: usize,
    /// logs/launcher.log: everything the launcher reports.
    log_file: Option<std::fs::File>,
    view: View,
    hwnd: HWND,
    tooltip: HWND,
    hover: HWND,
    scale: f32,
    dpi: u32,
    fonts: Option<Fonts>,
    controls: Vec<Control>,
    background: Vec<u32>,
    background_dirty: bool,
    canvas: Option<Dib>,
    canvas_dirty: bool,
    shown_second: u64,
    profile: Profile,
    /// The text fields' background brush (WM_CTLCOLOREDIT).
    edit_brush: HBRUSH,
    /// `ROSES_LAUNCHER_CAPTURE=<file.png>` (screenshots, tests): the window
    /// is never shown; once the shown game is ready its page is drawn
    /// off-screen into this PNG and the launcher closes.
    capture: Option<(PathBuf, Instant)>,
    /// `ROSES_LAUNCHER_RECORD=<out.mp4>` (videos; a tool): as the capture,
    /// then a short clip with a scripted cursor (`crate::record`).
    record: Option<(PathBuf, Instant)>,
    /// Game pads (`crate::pad`; not in a capture).
    pads: Option<pad::Pads>,
    pad_input: pad::Input,
    /// The control the pad moved the focus to last (a capture draws it
    /// focused: its window is never shown, so it has no keyboard focus).
    pad_focus: Option<usize>,
    pad_clock: Instant,
    update: Update,
    update_sender: Sender<UpdateEvent>,
    update_receiver: Receiver<UpdateEvent>,
    /// What the updater last said (and whether it is an error): shown in
    /// place of the game's status until the next Play or Import.
    update_notice: Option<(String, bool)>,
}

/// The header's update control ([`UPDATE`]).
enum Update {
    /// Today's link to the release page: still checking, nothing newer, a
    /// release without `latest.json`, or the check failed.
    Link,
    /// The Update button.
    Offered(Available),
    /// Downloading or unpacking; `percent` of the download.
    Working { available: Available, percent: u32 },
}

impl Update {
    fn working(&self) -> bool {
        matches!(self, Update::Working { .. })
    }
}

/// The capture variable ([`App::capture`]).
const ENV_CAPTURE: &str = "ROSES_LAUNCHER_CAPTURE";
/// The id of the game shown at start (default: the first, Roses).
const ENV_FIRST_GAME: &str = "ROSES_LAUNCHER_GAME";
/// With [`ENV_CAPTURE`]: game-pad moves made before the capture, e.g.
/// `left,left,down` (the focus they reach is drawn focused).
const ENV_PAD_MOVES: &str = "ROSES_LAUNCHER_PAD_MOVES";
/// The recording variable ([`App::record`]) and its script.
const ENV_RECORD: &str = "ROSES_LAUNCHER_RECORD";
const ENV_RECORD_SCRIPT: &str = "ROSES_LAUNCHER_RECORD_SCRIPT";

const FIELD_FILL: u32 = rgb(20, 20, 25);

fn wide(value: impl AsRef<OsStr>) -> Vec<u16> {
    value.as_ref().encode_wide().chain(Some(0)).collect()
}

fn colorref(color: u32) -> COLORREF {
    ((color >> 16) & 255) | (color & 0xff00) | ((color & 255) << 16)
}

pub fn show_error(error: &str) {
    unsafe {
        MessageBoxW(
            null_mut(),
            wide(error).as_ptr(),
            wide("Roses launcher").as_ptr(),
            MB_OK | MB_ICONERROR,
        );
    }
}

pub fn run() -> Result<(), String> {
    let executable = std::env::current_exe().map_err(|error| error.to_string())?;
    let options = Options::parse(std::env::args_os().skip(1), &executable)?;
    let launcher_dir = executable.parent().map(Path::to_path_buf).unwrap_or_default();
    let (platform, mut notes) = Platform::load(&launcher_dir);
    let (games, game_notes) = roses_launcher::registry::load_registry(&launcher_dir, &options.data_root);
    notes.extend(game_notes);
    // The game shown first: the first one (Roses), or `ROSES_LAUNCHER_GAME=<id>`
    // (screenshots of another game's page with ROSES_LAUNCHER_CAPTURE).
    let first = std::env::var(ENV_FIRST_GAME).ok().and_then(|id| games.iter().position(|g| g.id() == id)).unwrap_or(0);
    let slot = Slot::new(&games[first], options.iso.clone());
    let parked = (0..games.len()).map(|i| (i != first).then(|| Slot::new(&games[i], None))).collect();
    let paths = roses_launcher::profile::paths_for(&options.data_root, &games);
    let config = paths.load_config();
    let user = config.as_ref().ok().and_then(|c| paths.saved(c)).map(|s| s.username);
    let (profile_sender, profile_receiver) = mpsc::channel();
    let (update_sender, update_receiver) = mpsc::channel();
    let mut app = Box::new(App {
        options,
        platform,
        games,
        current: first,
        slot,
        parked,
        tooltip_game: first,
        log_file: None,
        view: View::Game,
        hwnd: null_mut(),
        tooltip: null_mut(),
        hover: null_mut(),
        scale: 1.0,
        dpi: 96,
        fonts: None,
        controls: Vec::new(),
        background: Vec::new(),
        background_dirty: true,
        canvas: None,
        canvas_dirty: true,
        shown_second: u64::MAX,
        profile: Profile {
            paths,
            config,
            form: if user.is_some() { Form::Account } else { Form::SignUp },
            user,
            server_name: None,
            invite_required: None,
            email_offered: false,
            email: None,
            last_job: "",
            busy: false,
            reached: false,
            message: String::new(),
            error: false,
            sender: profile_sender,
            receiver: profile_receiver,
        },
        edit_brush: unsafe { CreateSolidBrush(colorref(FIELD_FILL)) },
        capture: std::env::var_os(ENV_CAPTURE).filter(|p| !p.is_empty()).map(|p| (PathBuf::from(p), Instant::now())),
        record: std::env::var_os(ENV_RECORD).filter(|p| !p.is_empty()).map(|p| (PathBuf::from(p), Instant::now())),
        pads: None,
        pad_input: pad::Input::default(),
        pad_focus: None,
        pad_clock: Instant::now(),
        update: Update::Link,
        update_sender,
        update_receiver,
        update_notice: None,
    });
    app.log_file =
        roses_launcher::start_log(&app.options.data_root, roses_launcher::LAUNCHER_LOG).ok();
    // SAFETY: append_log only writes to the log file (no window calls).
    for game in app.games.clone() {
        unsafe { append_log(&mut app, &format!("Game: {} {} ({}, data in {})\n", game.manifest.name, game.manifest.version, game.source, game.data_dir.display())) };
    }
    for note in notes {
        unsafe { append_log(&mut app, &format!("{note}\n")) };
    }
    // A panic inside a window callback aborts the process; leave the reason in
    // launcher.log so a report contains it.
    let log_path = roses_launcher::logs_dir(&app.options.data_root).join(roses_launcher::LAUNCHER_LOG);
    std::panic::set_hook(Box::new(move |info| {
        if let Ok(mut file) = std::fs::OpenOptions::new().append(true).open(&log_path) {
            use std::io::Write;
            let _ = writeln!(file, "The launcher stopped: {info}");
        }
    }));
    unsafe {
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let instance = GetModuleHandleW(null());
        let class_name = wide("RosesLauncherWindow");
        let class = WNDCLASSW {
            style: CS_DROPSHADOW,
            lpfnWndProc: Some(window_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            hIcon: LoadIconW(instance, 1 as _),
            lpszClassName: class_name.as_ptr(),
            ..Default::default()
        };
        if RegisterClassW(&class) == 0 {
            return Err(std::io::Error::last_os_error().to_string());
        }
        let tip_class = wide("RosesLauncherTooltip");
        RegisterClassW(&WNDCLASSW {
            lpfnWndProc: Some(tooltip_proc),
            hInstance: instance,
            hCursor: LoadCursorW(null_mut(), IDC_ARROW),
            lpszClassName: tip_class.as_ptr(),
            ..Default::default()
        });
        // The boxed state outlives the complete message loop. No callback frees it.
        let hwnd = CreateWindowExW(
            WS_EX_APPWINDOW | WS_EX_CONTROLPARENT,
            class_name.as_ptr(),
            wide(format!(
                "{} — {}",
                app.platform.name,
                roses_launcher::version_label()
            ))
            .as_ptr(),
            WS_POPUP | WS_SYSMENU | WS_MINIMIZEBOX | WS_CLIPCHILDREN,
            0,
            0,
            WIDTH,
            HEIGHT,
            null_mut(),
            null_mut(),
            instance,
            (&mut *app as *mut App).cast(),
        );
        if hwnd.is_null() {
            return Err(format!(
                "Could not create the launcher window: {}",
                std::io::Error::last_os_error()
            ));
        }
        app.tooltip = CreateWindowExW(
            WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE,
            tip_class.as_ptr(),
            wide(&app.games[0].manifest.name).as_ptr(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            hwnd,
            null_mut(),
            instance,
            null(),
        );
        SetWindowLongPtrW(app.tooltip, GWLP_USERDATA, &mut *app as *mut App as isize);
        let corner = DWMWCP_ROUND;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE as u32,
            (&corner as *const i32).cast(),
            4,
        );
        place_window(&mut app, None);
        set_icons(&app);
        if SetTimer(hwnd, TIMER, 100, None) == 0 {
            DestroyWindow(hwnd);
            return Err("Could not start the launcher UI timer.".into());
        }
        refresh(&mut app);
        // Check the saved sign-in with the server (quietly).
        if app.profile.config.is_ok() {
            profile_job(&mut app, roses_launcher::profile::Job::Check);
        }
        start_update_check(&mut app);
        // Hide focus rectangles until the keyboard is used (Tab shows them).
        SendMessageW(hwnd, WM_CHANGEUISTATE, (UIS_SET as usize) | (((UISF_HIDEFOCUS | UISF_HIDEACCEL) as usize) << 16), 0);
        if app.capture.is_none() && app.record.is_none() && !updater::test_hidden() {
            app.pads = Some(pad::Pads::start());
            SetTimer(hwnd, PAD_TIMER, PAD_POLL_MS, None);
            ShowWindow(hwnd, SW_SHOW);
            focus_first(&app);
        }
        let mut message = MSG::default();
        loop {
            let result = GetMessageW(&mut message, null_mut(), 0, 0);
            if result == 0 {
                break;
            }
            if result == -1 {
                let error = std::io::Error::last_os_error().to_string();
                DestroyWindow(hwnd);
                return Err(error);
            }
            if IsDialogMessageW(hwnd, &message) == 0 {
                TranslateMessage(&message);
                DispatchMessageW(&message);
            }
        }
    }
    Ok(())
}

unsafe fn set_icons(app: &App) {
    let instance = GetModuleHandleW(null());
    for (kind, metric) in [(ICON_BIG, SM_CXICON), (ICON_SMALL, SM_CXSMICON)] {
        let size = GetSystemMetricsForDpi(metric, app.dpi);
        let icon = LoadImageW(instance, 1 as _, IMAGE_ICON, size, size, LR_DEFAULTCOLOR);
        if !icon.is_null() {
            SendMessageW(app.hwnd, WM_SETICON, kind as usize, icon as isize);
        }
    }
}

/// The scale for a window of WIDTH x HEIGHT logical pixels at `dpi`, shrunk
/// when the monitor's work area is too small to show it whole.
fn window_scale(dpi: u32, work: RECT) -> f32 {
    let wanted = dpi.max(96) as f32 / 96.0;
    let fit_w = (work.right - work.left - 16).max(320) as f32 / WIDTH as f32;
    let fit_h = (work.bottom - work.top - 16).max(200) as f32 / HEIGHT as f32;
    wanted.min(fit_w).min(fit_h).max(0.5)
}

unsafe fn monitor_work_area(monitor: HMONITOR) -> Option<RECT> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    (GetMonitorInfoW(monitor, &mut info) != 0
        && info.rcWork.right > info.rcWork.left
        && info.rcWork.bottom > info.rcWork.top)
        .then_some(info.rcWork)
}

/// Size and centre the window on the monitor under the cursor (or, after a
/// DPI change, inside the rectangle Windows suggests).
unsafe fn place_window(app: &mut App, suggested: Option<RECT>) {
    let hwnd = app.hwnd;
    if suggested.is_none() {
        let mut cursor = POINT::default();
        GetCursorPos(&mut cursor);
        let monitor = MonitorFromPoint(cursor, MONITOR_DEFAULTTOPRIMARY);
        if let Some(work) = monitor_work_area(monitor) {
            // Move onto that monitor first so GetDpiForWindow reports its DPI.
            SetWindowPos(
                hwnd,
                null_mut(),
                work.left,
                work.top,
                1,
                1,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
        }
    }
    app.dpi = GetDpiForWindow(hwnd).max(96);
    let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
    let work = monitor_work_area(monitor).unwrap_or(RECT {
        left: 0,
        top: 0,
        right: 1920,
        bottom: 1080,
    });
    app.scale = window_scale(app.dpi, work);
    let (w, h) = (app.sc(WIDTH), app.sc(HEIGHT));
    let (x, y) = match suggested {
        Some(r) => (r.left, r.top),
        None => (
            work.left + (work.right - work.left - w) / 2,
            work.top + (work.bottom - work.top - h) / 2,
        ),
    };
    app.fonts = Some(Fonts::new(app.scale));
    SetWindowPos(hwnd, null_mut(), x, y, w, h, SWP_NOZORDER | SWP_NOACTIVATE);
    layout(app);
    app.background_dirty = true;
    app.canvas_dirty = true;
}

impl App {
    fn sc(&self, value: i32) -> i32 {
        (value as f32 * self.scale).round() as i32
    }

    fn control(&self, id: usize) -> Option<&Control> {
        self.controls.iter().find(|c| c.id == id)
    }

    fn hwnd_of(&self, id: usize) -> HWND {
        self.control(id).map_or(null_mut(), |c| c.hwnd)
    }

    fn played_seconds(&self) -> u64 {
        self.slot.state.played_seconds + self.slot.game_started.map_or(0, |t| t.elapsed().as_secs())
    }

    /// Share of the import done: finished steps plus a creeping share of the
    /// running one (the importer reports only step starts).
    fn progress(&self) -> f32 {
        match &self.slot.step {
            Some((step, total, _)) => {
                let t = self.slot.step_started.elapsed().as_secs_f32();
                let within = 0.85 * (1.0 - (-t / 45.0).exp());
                ((*step as f32 - 1.0 + within) / *total as f32).clamp(0.0, 1.0)
            }
            None => 0.02,
        }
    }

    /// Game `index`'s state (shown or parked).
    fn slot_of(&self, index: usize) -> Option<&Slot> {
        if index == self.current { Some(&self.slot) } else { self.parked.get(index)?.as_ref() }
    }

    fn slot_of_mut(&mut self, index: usize) -> Option<&mut Slot> {
        if index == self.current { Some(&mut self.slot) } else { self.parked.get_mut(index)?.as_mut() }
    }

    /// The game shown.
    fn game(&self) -> &Game {
        &self.games[self.current]
    }

    /// The shown game has a completed import (games with an import check:
    /// one that checks out).
    fn imported(&self) -> bool {
        if self.game().manifest.importer.check.is_some() {
            self.slot.verified.is_some()
        } else {
            self.slot.game_json.is_some()
        }
    }

    /// Any game is being imported (closing the window waits for it).
    fn any_importing(&self) -> bool {
        self.slot.importing || self.parked.iter().flatten().any(|s| s.importing)
    }
}

/// The sidebar entry of game `index`.
fn nav_id(index: usize) -> usize {
    if index == 0 { NAV_GAME } else { NAV_GAMES + index }
}

/// The game a sidebar entry stands for.
fn nav_game(id: usize) -> Option<usize> {
    match id {
        NAV_GAME => Some(0),
        id if (NAV_GAMES + 1..NAV_GAMES + roses_launcher::registry::MAX_GAMES).contains(&id) => Some(id - NAV_GAMES),
        _ => None,
    }
}

/// Show game `index` (its state was parked; the shown one is parked).
unsafe fn select_game(app: &mut App, index: usize) {
    if index == app.current || index >= app.games.len() {
        return set_view(app, View::Game);
    }
    let Some(next) = app.parked[index].take() else { return };
    let previous = std::mem::replace(&mut app.slot, next);
    app.parked[app.current] = Some(previous);
    app.current = index;
    app.view = View::Game;
    append_log(app, &format!("Showing {}\n", app.game().manifest.name));
    if app.slot.backdrop.is_none() && app.slot.game_json.is_some() {
        load_backdrop(app);
    }
    layout(app);
    app.background_dirty = true;
    refresh(app);
    focus_first(app);
}

/// The parked games: count play time of a game that closed, take finished
/// imports (their messages show when the game is shown again).
unsafe fn poll_parked(app: &mut App) {
    let games = app.games.clone();
    let mut lines = Vec::new();
    for (index, slot) in app.parked.iter_mut().enumerate() {
        let Some(slot) = slot.as_mut() else { continue };
        let game = &games[index];
        if let Some(child) = slot.game_child.as_mut() {
            if !matches!(child.try_wait(), Ok(None)) {
                slot.game_child = None;
                if let Some(started) = slot.game_started.take() {
                    slot.state.played_seconds += started.elapsed().as_secs();
                    let _ = roses_launcher::save_state(&game.data_dir, &slot.state);
                }
                slot.status = game.manifest.texts.ready.clone().unwrap_or_else(|| "The game closed.".into());
                slot.status_error = false;
                lines.push(format!("{} closed.\n", game.manifest.name));
            }
        }
        for _ in 0..64 {
            let Ok(event) = slot.receiver.try_recv() else { break };
            match event {
                Event::Log(text) => lines.push(text),
                Event::Complete(result) => {
                    slot.busy = false;
                    slot.importing = false;
                    match result {
                        Ok(Some(verified)) => {
                            slot.verified = Some(verified);
                            slot.game_json = game.latest_marker();
                            slot.status = "Import complete. Click Play to start from the title screen.".into();
                        }
                        Ok(None) => slot.status = not_imported_status(game, slot.iso.as_deref()),
                        Err(error) => {
                            slot.status = format!("Import failed: {} Report a problem opens the logs.", first_line(&error));
                            slot.status_error = true;
                        }
                    }
                }
                Event::Imported(result) => {
                    slot.busy = false;
                    slot.importing = false;
                    match result {
                        Ok(Some(marker)) => {
                            slot.game_json = Some(marker);
                            slot.status = "Ready. Click Play.".into();
                        }
                        Ok(None) => slot.status = not_imported_status(game, slot.iso.as_deref()),
                        Err(error) => {
                            slot.status = format!("Import failed: {}", first_line(&error));
                            slot.status_error = true;
                        }
                    }
                }
            }
        }
    }
    for line in lines {
        append_log(app, &line);
    }
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_NCCREATE {
        let create = &*(lparam as *const CREATESTRUCTW);
        SetWindowLongPtrW(hwnd, GWLP_USERDATA, create.lpCreateParams as isize);
        (*(create.lpCreateParams as *mut App)).hwnd = hwnd;
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut App;
    if state.is_null() {
        return DefWindowProcW(hwnd, message, wparam, lparam);
    }
    match message {
        WM_CREATE => {
            if create_controls(&mut *state).is_err() {
                return -1;
            }
            0
        }
        WM_NCHITTEST => {
            let mut point = POINT {
                x: (lparam & 0xffff) as i16 as i32,
                y: ((lparam >> 16) & 0xffff) as i16 as i32,
            };
            ScreenToClient(hwnd, &mut point);
            if point.y < (*state).sc(TITLE) {
                HTCAPTION as isize
            } else {
                HTCLIENT as isize
            }
        }
        WM_ERASEBKGND => 1,
        // Like a dialog: the window itself never keeps the keyboard focus.
        WM_SETFOCUS => {
            focus_first(&*state);
            0
        }
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let app = &mut *state;
            ensure_canvas(app);
            if let Some(canvas) = &app.canvas {
                BitBlt(dc, 0, 0, canvas.width, canvas.height, canvas.dc, 0, 0, SRCCOPY);
            }
            EndPaint(hwnd, &paint);
            0
        }
        WM_DRAWITEM => {
            draw_item(&mut *state, &*(lparam as *const DRAWITEMSTRUCT));
            1
        }
        WM_DPICHANGED => {
            let app = &mut *state;
            let suggested = *(lparam as *const RECT);
            place_window(app, Some(suggested));
            set_icons(app);
            refresh(app);
            0
        }
        WM_COMMAND => {
            let id = wparam & 0xffff;
            let notice = (wparam >> 16) as u32;
            let edit = (*state).control(id).is_some_and(|c| c.kind == Kind::Edit);
            if edit {
                // A text field gained or lost the focus: its box outline.
                if notice == EN_SETFOCUS || notice == EN_KILLFOCUS {
                    let app = &mut *state;
                    app.background_dirty = true;
                    app.canvas_dirty = true;
                    RedrawWindow(hwnd, null(), null_mut(), RDW_INVALIDATE | RDW_ALLCHILDREN);
                }
            } else if notice == BN_CLICKED {
                command(hwnd, state, id);
            }
            0
        }
        WM_CTLCOLOREDIT => {
            let dc = wparam as HDC;
            SetTextColor(dc, colorref(TEXT));
            SetBkColor(dc, colorref(FIELD_FILL));
            (*state).edit_brush as isize
        }
        WM_TIMER if wparam == PAD_TIMER => {
            poll_pads(hwnd, state);
            0
        }
        WM_TIMER => {
            tick(&mut *state);
            0
        }
        WM_ACTIVATEAPP => {
            hide_tooltip(&mut *state);
            DefWindowProcW(hwnd, message, wparam, lparam)
        }
        WM_CLOSE => {
            // Do not hold a Rust reference to App across this modal message loop.
            if (*state).any_importing() {
                MessageBoxW(hwnd, wide("The game is still being imported. Please wait for it to finish before closing this window.").as_ptr(),
                    wide("Import in progress").as_ptr(), MB_OK | MB_ICONINFORMATION);
            } else {
                DestroyWindow(hwnd);
            }
            0
        }
        WM_DESTROY => {
            // Count the play time so far if the game is still running.
            let app = &mut *state;
            if let Some(started) = app.slot.game_started.take() {
                app.slot.state.played_seconds += started.elapsed().as_secs();
                let _ = roses_launcher::save_state(&app.game().data_dir, &app.slot.state);
            }
            for (index, slot) in app.parked.iter_mut().enumerate() {
                if let Some(started) = slot.as_mut().and_then(|s| s.game_started.take()) {
                    let slot = slot.as_mut().expect("present");
                    slot.state.played_seconds += started.elapsed().as_secs();
                    let _ = roses_launcher::save_state(&app.games[index].data_dir, &slot.state);
                }
            }
            KillTimer(hwnd, TIMER);
            KillTimer(hwnd, PAD_TIMER);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

unsafe fn command(hwnd: HWND, state: *mut App, id: usize) {
    match id {
        // Enter: click the focused button, else the visible primary one.
        1 => {
            let focus = GetFocus();
            let target = if (*state).controls.iter().any(|c| {
                c.hwnd == focus && !matches!(c.kind, Kind::Label { .. } | Kind::Edit)
            }) {
                focus
            } else {
                visible_primary(&*state).unwrap_or(null_mut())
            };
            if !target.is_null() && IsWindowEnabled(target) != 0 && IsWindowVisible(target) != 0 {
                SendMessageW(target, BM_CLICK, 0, 0);
            }
        }
        // Escape leaves the settings page (and the password forms).
        2 if (*state).view == View::Profile && (*state).profile.form == Form::Change => {
            set_form(&mut *state, Form::Account)
        }
        2 if (*state).view == View::Profile && matches!((*state).profile.form, Form::Reset | Form::ResetCode) => {
            set_form(&mut *state, Form::LogIn)
        }
        2 => set_view(&mut *state, View::Game),
        // Nothing may start a program while its file is about to be replaced.
        IMPORT | S_REIMPORT | PLAY if (*state).update.working() => {
            update_notice(&mut *state, "Please wait for the update to finish.", false);
        }
        IMPORT if !(*state).slot.busy => {
            (*state).update_notice = None;
            let iso = match (*state).slot.iso.clone() {
                Some(iso) => Some(iso),
                None => choose_iso(hwnd, (*state).game()),
            };
            if let Some(iso) = iso {
                start_import(&mut *state, iso);
            }
        }
        S_REIMPORT if !(*state).slot.busy => {
            (*state).update_notice = None;
            if let Some(iso) = choose_iso(hwnd, (*state).game()) {
                set_view(&mut *state, View::Game);
                start_import(&mut *state, iso);
            }
        }
        PLAY if !(*state).slot.busy && (*state).slot.game_child.is_none() => {
            (*state).update_notice = None;
            play_game(&mut *state)
        }
        TOGGLE if (*state).slot.game_child.is_none() => flip_toggle(&mut *state),
        REPORT | BANNER_REPORT | S_LOGS => report_problem(&mut *state),
        SAVES | S_SAVES => {
            let app = &mut *state;
            let saves = roses_launcher::memory_card_dir(&app.game().data_dir);
            let _ = std::fs::create_dir_all(&saves);
            if let Err(error) = roses_launcher::open_folder(&saves) {
                set_status(app, &format!("Could not open the saves folder: {error}"), true);
            }
        }
        S_DATA => {
            let app = &mut *state;
            let data = app.game().data_dir.clone();
            let _ = std::fs::create_dir_all(&data);
            if let Err(error) = roses_launcher::open_folder(&data) {
                set_status(app, &format!("Could not open the data folder: {error}"), true);
            }
        }
        GEAR | NAV_SETTINGS => set_view(&mut *state, View::Settings),
        id if nav_game(id).is_some() => select_game(&mut *state, nav_game(id).expect("checked")),
        NAV_PROFILE | ACCOUNT => {
            let app = &mut *state;
            set_view(app, View::Profile);
            if !app.profile.busy && !app.profile.reached {
                profile_job(app, roses_launcher::profile::Job::Check);
            }
        }
        P_RETRY if !(*state).profile.busy => profile_job(&mut *state, roses_launcher::profile::Job::Check),
        P_MODE_LOGIN => set_form(&mut *state, Form::LogIn),
        P_MODE_SIGNUP => set_form(&mut *state, Form::SignUp),
        P_CHANGE => set_form(&mut *state, Form::Change),
        P_CANCEL if matches!((*state).profile.form, Form::Reset | Form::ResetCode) => set_form(&mut *state, Form::LogIn),
        P_CANCEL => set_form(&mut *state, Form::Account),
        P_FORGOT => set_form(&mut *state, Form::Reset),
        P_EMAIL_SEND if !(*state).profile.busy => submit_email(&mut *state),
        P_EMAIL_VERIFY if !(*state).profile.busy => submit_code(&mut *state),
        P_EMAIL_LATER if !(*state).profile.busy => profile_job(&mut *state, roses_launcher::profile::Job::DismissEmailPrompt),
        UPDATE => update_clicked(hwnd, state),
        P_LOGOUT if !(*state).profile.busy => profile_job(&mut *state, roses_launcher::profile::Job::LogOut),
        P_SUBMIT if !(*state).profile.busy => submit_profile(&mut *state),
        HELP => open_beside_launcher(&mut *state, &["README.txt"]),
        KNOWN_ISSUES => {
            let app = &mut *state;
            match app.game().manifest.links.known_issues.clone() {
                Some(url) => open_configured(app, Some(&url), ""),
                None => open_beside_launcher(app, &["KNOWN-ISSUES.txt", "README.txt"]),
            }
        }
        BANNER_CLOSE => {
            let app = &mut *state;
            app.slot.banner = false;
            app.background_dirty = true;
            app.slot.state.banner_dismissed = Some(roses_launcher::version_label());
            if let Err(error) = roses_launcher::save_state(&app.game().data_dir, &app.slot.state) {
                append_log(app, &format!("Could not save launcher settings: {error}\n"));
            }
            layout(app);
            refresh(app);
        }
        MINIMIZE => {
            ShowWindow(hwnd, SW_MINIMIZE);
        }
        CLOSE => {
            PostMessageW(hwnd, WM_CLOSE, 0, 0);
        }
        _ => {}
    }
}

/// The updater's message: logged and shown on the game page.
unsafe fn update_notice(app: &mut App, text: &str, error: bool) {
    append_log(app, &format!("Update: {text}\n"));
    app.update_notice = Some((text.to_owned(), error));
    if app.view != View::Game {
        set_view(app, View::Game);
    }
    update_texts(app);
}

/// Show `text` on the update control.
unsafe fn update_caption(app: &App, text: &str) {
    set_text(app, UPDATE, text);
    InvalidateRect(app.hwnd_of(UPDATE), null(), 0);
}

/// Report the last update, then ask GitHub in the background. Release builds
/// only (a development build has no version to compare), and no network from
/// screenshots and recordings.
unsafe fn start_update_check(app: &mut App) {
    let tool = app.capture.is_some() || app.record.is_some();
    if tool && !cfg!(feature = "update-test-seam") {
        return;
    }
    let Some(current) = roses_launcher::RELEASE else { return };
    let install = app.options.launcher_dir.clone();
    if let Some(outcome) = swap::finish_previous(&install) {
        update_notice(app, &outcome.message, !outcome.ok);
    }
    let Some(url) = app.platform.launcher_update_api.clone() else {
        swap::Staging::of(&install).remove();
        return;
    };
    let sender = app.update_sender.clone();
    std::thread::spawn(move || {
        swap::Staging::of(&install).remove();
        let result = std::panic::catch_unwind(|| updater::check(&url, current, &updater::Policy::for_build()))
            .unwrap_or_else(|_| Err("The update check stopped unexpectedly.".into()));
        let _ = sender.send(UpdateEvent::Checked(result));
    });
}

/// Why an update cannot start now, in the player's words.
fn update_blocked(app: &App) -> Option<String> {
    if app.slot.game_child.is_some() || app.parked.iter().flatten().any(|s| s.game_child.is_some()) {
        return Some("Close the game first, then click Update.".into());
    }
    if app.any_importing() {
        return Some("Wait for the import to finish, then click Update.".into());
    }
    let running = std::env::current_exe().ok()?;
    swap::program_in_use(&app.options.launcher_dir, &running)
        .map(|file| format!("{} is running. Close the game first, then click Update.", file.display()))
}

/// The header's update control: the release page, or the update itself.
/// Holds no Rust reference to App across the question's message loop.
unsafe fn update_clicked(hwnd: HWND, state: *mut App) {
    let available = match &(*state).update {
        Update::Link => {
            let app = &mut *state;
            let url = app.platform.launcher_releases.clone();
            open_configured(app, url.as_deref(), "The launcher's download page is not set up yet (platform.json: launcher_releases).");
            return;
        }
        Update::Working { .. } => return,
        Update::Offered(available) => available.clone(),
    };
    if let Some(reason) = update_blocked(&*state) {
        update_notice(&mut *state, &reason, true);
        return;
    }
    let question = format!(
        "Update to {}?\n\nThe launcher downloads about {} MB, closes, replaces its program files and starts again. Your imports, saves and logs are not touched.",
        available.version,
        available.size.div_ceil(1024 * 1024)
    );
    if !updater::test_confirmed()
        && MessageBoxW(hwnd, wide(question).as_ptr(), wide("Update").as_ptr(), MB_YESNO | MB_ICONQUESTION) != IDYES
    {
        return;
    }
    // The game could have been started from the pad meanwhile.
    if let Some(reason) = update_blocked(&*state) {
        update_notice(&mut *state, &reason, true);
        return;
    }
    let app = &mut *state;
    update_notice(app, &format!("Downloading {}…", available.version), false);
    update_caption(app, "Downloading 0%");
    app.update = Update::Working { available: available.clone(), percent: 0 };
    let install = app.options.launcher_dir.clone();
    let sender = app.update_sender.clone();
    std::thread::spawn(move || {
        let progress = sender.clone();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            updater::prepare(&available, &install, &updater::Policy::for_build(), &mut |done, total| {
                let _ = progress.send(UpdateEvent::Progress(done, total));
            })
        }))
        .unwrap_or_else(|_| Err("The update stopped unexpectedly.".into()));
        let _ = sender.send(UpdateEvent::Prepared(result));
    });
}

/// The update failed before the swap: the Update button again, and why.
unsafe fn update_failed(app: &mut App, available: Available, why: &str) {
    update_caption(app, &format!("Update to {}", available.version));
    app.update = Update::Offered(available);
    swap::Staging::of(&app.options.launcher_dir).remove();
    update_notice(app, &format!("The update did not finish. Nothing was changed; this version still works. {why}"), true);
}

/// Everything is staged: hand over to the helper and close.
unsafe fn update_ready(app: &mut App, available: Available) {
    if let Some(reason) = update_blocked(app) {
        update_failed(app, available, &reason);
        return;
    }
    let install = app.options.launcher_dir.clone();
    let data_root = app.options.data_root.clone();
    match swap::start_helper(&install, &available.version, &data_root) {
        Ok(()) => {
            append_log(app, &format!("Update: installing {}; the launcher closes and starts again.\n", available.version));
            DestroyWindow(app.hwnd);
        }
        Err(why) => update_failed(app, available, &why),
    }
}

/// What the update jobs sent.
unsafe fn receive_update(app: &mut App) {
    while let Ok(event) = app.update_receiver.try_recv() {
        match event {
            UpdateEvent::Checked(Ok(Check::Available(available))) => {
                append_log(app, &format!("Update: {} is available.\n", available.version));
                update_caption(app, &format!("Update to {}", available.version));
                app.update = Update::Offered(available);
            }
            UpdateEvent::Checked(Ok(Check::UpToDate)) => append_log(app, "Update: this is the latest version.\n"),
            UpdateEvent::Checked(Ok(Check::NoUpdateFile { tag, why })) => {
                append_log(app, &format!("Update: {tag} cannot be installed from here ({why}); the link opens its page.\n"));
            }
            UpdateEvent::Checked(Err(why)) => append_log(app, &format!("Update check failed: {why}\n")),
            UpdateEvent::Progress(done, total) => {
                let now = (done * 100 / total.max(1)) as u32;
                if let Update::Working { percent, .. } = &mut app.update {
                    if *percent != now {
                        *percent = now;
                        let caption = if done >= total { "Unpacking…".to_owned() } else { format!("Downloading {now}%") };
                        update_caption(app, &caption);
                    }
                }
            }
            UpdateEvent::Prepared(result) => {
                let Update::Working { available, .. } = std::mem::replace(&mut app.update, Update::Link) else { continue };
                match result {
                    Ok(()) => update_ready(app, available),
                    Err(why) => update_failed(app, available, &why),
                }
                return;
            }
        }
    }
}

/// Open a configured web page in the system browser, or say it is not set
/// up yet.
unsafe fn open_configured(app: &mut App, url: Option<&str>, missing: &str) {
    match url {
        Some(url) => {
            append_log(app, &format!("Opening {url}\n"));
            if let Err(error) = roses_launcher::registry::open_link(url) {
                set_status(app, &format!("Could not open the page: {error}"), true);
            }
        }
        None => set_status(app, missing, false),
    }
}

/// Open a text file shipped beside the launcher (first that exists).
unsafe fn open_beside_launcher(app: &mut App, names: &[&str]) {
    let directory = Some(app.options.launcher_dir.clone());
    let found = directory.and_then(|directory| {
        names
            .iter()
            .map(|name| directory.join(name))
            .find(|path| path.is_file())
    });
    match found {
        Some(path) => {
            if let Err(error) = roses_launcher::open_path(&path) {
                set_status(app, &format!("Could not open {}: {error}", path.display()), true);
            }
        }
        None => set_view(app, View::Settings),
    }
}

unsafe fn set_view(app: &mut App, view: View) {
    if app.view != view {
        app.view = view;
        app.background_dirty = true;
        refresh(app);
        focus_first(app);
    }
}

/// Give the keyboard focus to the primary button, or else to the first
/// visible, enabled tab stop.
unsafe fn focus_first(app: &App) {
    let usable = |hwnd: HWND| {
        !hwnd.is_null() && IsWindowVisible(hwnd) != 0 && IsWindowEnabled(hwnd) != 0
    };
    // The Profile page starts in its first empty text field.
    if app.view == View::Profile {
        let field = profile_fields(app).into_iter().map(|id| app.hwnd_of(id)).find(|&h| usable(h) && GetWindowTextLengthW(h) == 0);
        if let Some(hwnd) = field {
            SetFocus(hwnd);
            return;
        }
    }
    let target = visible_primary(app).filter(|&hwnd| usable(hwnd)).or_else(|| {
        app.controls
            .iter()
            .filter(|c| !matches!(c.kind, Kind::Label { .. }))
            .map(|c| c.hwnd)
            .find(|&hwnd| usable(hwnd))
    });
    if let Some(hwnd) = target {
        SetFocus(hwnd);
    }
}

fn visible_primary(app: &App) -> Option<HWND> {
    if app.view == View::Profile {
        return [P_SUBMIT, P_EMAIL_VERIFY, P_EMAIL_SEND].into_iter().find(|&id| is_visible(app, id)).map(|id| app.hwnd_of(id));
    }
    if app.view != View::Game {
        return Some(app.hwnd_of(S_REIMPORT));
    }
    let id = if app.imported() && !app.slot.importing {
        PLAY
    } else {
        IMPORT
    };
    Some(app.hwnd_of(id))
}

/// Button subclass: hover tracking, the hand cursor on links, no erase.
unsafe extern "system" fn control_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let original: WNDPROC = std::mem::transmute(GetPropW(hwnd, PROP.as_ptr()));
    let parent = GetParent(hwnd);
    let state = GetWindowLongPtrW(parent, GWLP_USERDATA) as *mut App;
    match message {
        WM_ERASEBKGND => return 1,
        WM_MOUSEMOVE if !state.is_null() && (*state).hover != hwnd => {
            let app = &mut *state;
            let previous = app.hover;
            app.hover = hwnd;
            let mut track = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: hwnd,
                dwHoverTime: 0,
            };
            TrackMouseEvent(&mut track);
            if !previous.is_null() {
                InvalidateRect(previous, null(), 0);
            }
            InvalidateRect(hwnd, null(), 0);
            if let Some(index) = (0..app.games.len()).find(|&i| hwnd == app.hwnd_of(nav_id(i))) {
                app.tooltip_game = index;
                show_tooltip(app);
            }
        }
        WM_MOUSELEAVE if !state.is_null() && (*state).hover == hwnd => {
            let app = &mut *state;
            app.hover = null_mut();
            InvalidateRect(hwnd, null(), 0);
            hide_tooltip(app);
        }
        WM_SETCURSOR if !state.is_null() => {
            let link = (*state)
                .controls
                .iter()
                .any(|c| c.hwnd == hwnd && c.kind == Kind::Link);
            if link {
                SetCursor(LoadCursorW(null_mut(), IDC_HAND));
                return 1;
            }
        }
        WM_NCDESTROY => {
            RemovePropW(hwnd, PROP.as_ptr());
        }
        _ => {}
    }
    CallWindowProcW(original, hwnd, message, wparam, lparam)
}

static PROP: [u16; 10] = {
    let text = *b"RosesProc\0";
    let mut out = [0u16; 10];
    let mut i = 0;
    while i < 10 {
        out[i] = text[i] as u16;
        i += 1;
    }
    out
};

unsafe fn add_control(
    app: &mut App,
    class: &str,
    text: &str,
    style: u32,
    id: usize,
    kind: Kind,
) -> Result<(), ()> {
    let hwnd = CreateWindowExW(
        0,
        wide(class).as_ptr(),
        wide(text).as_ptr(),
        WS_CHILD | style,
        0,
        0,
        1,
        1,
        app.hwnd,
        id as HMENU,
        GetModuleHandleW(null()),
        null(),
    );
    if hwnd.is_null() {
        return Err(());
    }
    // Record the class procedure before subclassing: control_proc needs it
    // from the very first message.
    let original = GetWindowLongPtrW(hwnd, GWLP_WNDPROC);
    SetPropW(hwnd, PROP.as_ptr(), original as HANDLE);
    SetWindowLongPtrW(hwnd, GWLP_WNDPROC, control_proc as *const () as isize);
    app.controls.push(Control {
        hwnd,
        id,
        kind,
        rect: RECT::default(),
    });
    Ok(())
}

unsafe fn create_controls(app: &mut App) -> Result<(), ()> {
    let button = |app: &mut App, text: &str, id: usize, kind: Kind, tab: bool| {
        let style = BS_OWNERDRAW as u32 | if tab { WS_TABSTOP } else { 0 };
        add_control(app, "BUTTON", text, style, id, kind)
    };
    // Creation order is the Tab order.
    button(app, "Import ISO", IMPORT, Kind::Primary, true)?;
    button(app, "Play", PLAY, Kind::Primary, true)?;
    let toggle = app.game().manifest.launch_toggle.as_ref().map_or(String::new(), |t| t.label.clone());
    button(app, &toggle, TOGGLE, Kind::Check, true)?;
    button(app, "Saves", SAVES, Kind::Secondary, true)?;
    button(app, "Report a problem", REPORT, Kind::Secondary, true)?;
    button(app, "Settings", GEAR, Kind::Gear, true)?;
    button(app, "Profile", ACCOUNT, Kind::Link, true)?;
    button(app, UPDATE_TEXT, UPDATE, Kind::Link, true)?;
    // The Profile page, in Tab order.
    button(app, "Log in", P_MODE_LOGIN, Kind::Tab, true)?;
    button(app, "Sign up", P_MODE_SIGNUP, Kind::Tab, true)?;
    for (id, password, limit) in [
        (E_NAME, false, platform_net::account::USERNAME_MAX),
        (E_PASSWORD, true, platform_net::account::PASSWORD_MAX),
        (E_NEW, true, platform_net::account::PASSWORD_MAX),
        (E_CONFIRM, true, platform_net::account::PASSWORD_MAX),
        (E_INVITE, false, 9),
        (E_EMAIL, false, 254),
        (E_CODE, false, 9),
    ] {
        let style = WS_TABSTOP | ES_AUTOHSCROLL as u32 | if password { ES_PASSWORD as u32 } else { 0 };
        add_control(app, "EDIT", "", style, id, Kind::Edit)?;
        let hwnd = app.hwnd_of(id);
        SendMessageW(hwnd, EM_LIMITTEXT, limit, 0);
        if password {
            // EM_SETPASSWORDCHAR: a round dot instead of the classic '*'.
            SendMessageW(hwnd, 0x00CC, 0x25CF, 0);
        }
    }
    button(app, "Sign up", P_SUBMIT, Kind::Primary, true)?;
    button(app, "Forgot your password?", P_FORGOT, Kind::Link, true)?;
    button(app, "Send code", P_EMAIL_SEND, Kind::Secondary, true)?;
    button(app, "Verify", P_EMAIL_VERIFY, Kind::Secondary, true)?;
    button(app, "Not now", P_EMAIL_LATER, Kind::Link, true)?;
    button(app, "Change password", P_CHANGE, Kind::Secondary, true)?;
    button(app, "Log out", P_LOGOUT, Kind::Secondary, true)?;
    button(app, "Cancel", P_CANCEL, Kind::Secondary, true)?;
    button(app, "Try again", P_RETRY, Kind::Secondary, true)?;
    button(app, "Re-import ISO…", S_REIMPORT, Kind::Secondary, true)?;
    button(app, "Open data folder", S_DATA, Kind::Secondary, true)?;
    button(app, "Open saves folder", S_SAVES, Kind::Secondary, true)?;
    button(app, "Open logs folder", S_LOGS, Kind::Secondary, true)?;
    button(app, "Known issues", KNOWN_ISSUES, Kind::Link, true)?;
    button(app, "Report a problem…", BANNER_REPORT, Kind::Link, true)?;
    button(app, "Dismiss beta notice", BANNER_CLOSE, Kind::BannerClose, true)?;
    let names: Vec<String> = app.games.iter().map(|g| g.manifest.name.clone()).collect();
    for (index, name) in names.iter().enumerate() {
        button(app, name, nav_id(index), Kind::Nav, true)?;
    }
    button(app, "Profile", NAV_PROFILE, Kind::Nav, true)?;
    button(app, "Settings", NAV_SETTINGS, Kind::Nav, true)?;
    button(app, "Help", HELP, Kind::Title, true)?;
    button(app, "Minimize", MINIMIZE, Kind::Title, false)?;
    button(app, "Close", CLOSE, Kind::Title, false)?;
    let label = |app: &mut App, text: &str, id: usize, font: Font, color: u32, flags: DRAW_TEXT_FORMAT, spacing: i32| {
        add_control(
            app,
            "STATIC",
            text,
            SS_OWNERDRAW as u32,
            id,
            Kind::Label {
                font,
                color,
                flags,
                spacing,
            },
        )
    };
    let line = DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS;
    let right = line | DT_RIGHT;
    let wrap = DT_WORDBREAK | DT_NOPREFIX | DT_END_ELLIPSIS;
    let brand = app.platform.name.clone();
    label(app, &brand, L_BRAND, Font::Brand, TEXT, line, 0)?;
    label(
        app,
        &format!("Launcher {}", roses_launcher::short_version()),
        L_VERSION,
        Font::Mono,
        rgb(124, 124, 134),
        line,
        0,
    )?;
    label(app, "", L_BANNER, Font::Body, rgb(244, 226, 230), line, 0)?;
    label(app, "·", L_SEPARATOR, Font::Body, rgb(200, 160, 168), line | DT_CENTER, 0)?;
    label(app, "", L_HERO, Font::Hero, rgb(255, 255, 255), right, 0)?;
    label(app, "", L_SUBTITLE, Font::Subtitle, GOLD, right, 0)?;
    label(app, "", L_PROGRESS, Font::Small, MUTED, right, 0)?;
    label(app, "", L_STATUS, Font::Small, MUTED, DT_RIGHT | wrap, 0)?;
    label(app, "Settings", L_HEADING, Font::Heading, TEXT, line, 0)?;
    for (index, name) in ["GAME IMPORT", "DATA FOLDER", "SAVES", "LOGS", "CONTROLLERS"]
        .into_iter()
        .enumerate()
    {
        label(app, name, L_SECTION + index, Font::Section, GOLD, line, 1)?;
        let flags = if index == 1 {
            DT_SINGLELINE | DT_NOPREFIX | DT_PATH_ELLIPSIS
        } else {
            wrap
        };
        label(app, "", L_VALUE + index, Font::Body, rgb(206, 206, 214), flags, 0)?;
    }
    label(app, "Profile", L_P_HEADING, Font::Heading, TEXT, line, 0)?;
    label(app, "SERVER", L_P_SERVER_SECTION, Font::Section, GOLD, line, 1)?;
    label(app, "", L_P_SERVER, Font::Body, rgb(206, 206, 214), wrap, 0)?;
    label(app, "ACCOUNT", L_P_ACCOUNT_SECTION, Font::Section, GOLD, line, 1)?;
    label(app, "", L_P_ACCOUNT, Font::Body, rgb(206, 206, 214), wrap, 0)?;
    label(app, PROFILE_NOTE, L_P_NOTE, Font::Small, MUTED, wrap, 0)?;
    label(app, "", L_P_MESSAGE, Font::Body, MUTED, wrap, 0)?;
    label(app, "EMAIL", L_P_EMAIL_SECTION, Font::Section, GOLD, line, 1)?;
    label(app, "", L_P_EMAIL, Font::Body, rgb(206, 206, 214), wrap, 0)?;
    for id in EDITS {
        label(app, "", id + FIELD_CAPTION, Font::Section, rgb(176, 176, 186), line, 1)?;
    }
    Ok(())
}

const PROFILE_NOTE: &str = "Online Play in the game uses this account: sign in here once and the game signs in by itself. Your password is sent only to this server, encrypted, and is never stored on this PC.";

/// What the Profile page's email part shows (Account form only).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct EmailArea {
    /// The server sends email: the section is shown.
    offered: bool,
}

/// Logical rectangles (x, y, w, h) of every control for the current view.
/// `measured` gives text widths in logical pixels: brand, banner text and
/// the two banner links. `games`: sidebar entries.
/// `fields`: the Profile page's visible form fields in order; `form` its form.
fn logical_layout(banner: bool, measured: [i32; 4], fields: &[usize], form: Form, games: usize, email: EmailArea) -> Vec<(usize, [i32; 4])> {
    let [brand, banner_text, link1, link2] = measured;
    let update_x = WIDTH - 138 - 176;
    let version_x = 38 + brand + 10;
    let mut out = vec![
        (L_BRAND, [38, 0, brand + 2, TITLE]),
        (L_VERSION, [version_x, 0, (update_x - version_x - 8).max(40), TITLE]),
        (UPDATE, [update_x, 6, 170, TITLE - 12]),
        (HELP, [WIDTH - 138, 0, 46, TITLE]),
        (MINIMIZE, [WIDTH - 92, 0, 46, TITLE]),
        (CLOSE, [WIDTH - 46, 0, 46, TITLE]),
    ];
    let games = games.clamp(1, roses_launcher::registry::MAX_GAMES);
    for index in 0..games {
        out.push((nav_id(index), [0, TITLE + 10 + 62 * index as i32, SIDEBAR, 56]));
    }
    out.push((NAV_PROFILE, [0, TITLE + 10 + 62 * games as i32, SIDEBAR, 56]));
    out.push((NAV_SETTINGS, [0, HEIGHT - 66, SIDEBAR, 56]));
    let left = SIDEBAR + 20;
    let banner_text = banner_text.min(WIDTH - left - 300);
    let link1_x = left + banner_text + 8;
    let separator_x = link1_x + link1 + 10;
    let link2_x = separator_x + 12;
    out.extend([
        (L_BANNER, [left, TITLE, banner_text + 2, BANNER]),
        (KNOWN_ISSUES, [link1_x, TITLE + 5, link1 + 10, BANNER - 10]),
        (L_SEPARATOR, [separator_x, TITLE, 12, BANNER]),
        (BANNER_REPORT, [link2_x, TITLE + 5, link2 + 10, BANNER - 10]),
        (BANNER_CLOSE, [WIDTH - 40, TITLE + 3, 30, BANNER - 6]),
    ]);
    let wide_text = STACK_RIGHT - left;
    out.extend([
        (L_HERO, [left, 304, wide_text, 54]),
        (L_SUBTITLE, [left, 358, wide_text, 26]),
        (L_PROGRESS, [left, 388, wide_text, 22]),
        (IMPORT, [STACK_LEFT, 420, STACK_WIDTH, 60]),
        (PLAY, [STACK_LEFT, 420, STACK_WIDTH, 60]),
        (TOGGLE, [STACK_LEFT - 256, 438, 244, 24]),
        (SAVES, [STACK_LEFT, 488, 112, 40]),
        (REPORT, [STACK_LEFT + 116, 488, 156, 40]),
        (GEAR, [STACK_LEFT + 276, 488, 44, 40]),
        (ACCOUNT, [STACK_LEFT, 532, STACK_WIDTH, 22]),
        (L_STATUS, [STACK_RIGHT - 560, 556, 560, 38]),
    ]);
    let top = TITLE + if banner { BANNER } else { 0 };
    let x = SIDEBAR + 48;
    let button_x = WIDTH - 48 - 210;
    out.push((L_HEADING, [x, top + 26, 500, 46]));
    for (row, id) in [S_REIMPORT, S_DATA, S_SAVES, S_LOGS].into_iter().enumerate() {
        let y = top + 96 + 74 * row as i32;
        out.push((L_SECTION + row, [x, y, 560, 18]));
        out.push((L_VALUE + row, [x, y + 20, button_x - x - 32, 42]));
        out.push((id, [button_x, y + 6, 210, 38]));
    }
    let y = top + 96 + 74 * 4;
    out.push((L_SECTION + 4, [x, y, 560, 18]));
    out.push((L_VALUE + 4, [x, y + 20, WIDTH - x - 48, 64]));
    out.extend(profile_layout(top, fields, form, email));
    out
}

/// A text field: its caption at `y` and its EDIT inside a box `w` wide.
fn field_rects(id: usize, x: i32, y: i32, w: i32) -> [(usize, [i32; 4]); 2] {
    [(id + FIELD_CAPTION, [x, y, w, 18]), (id, [x + EDIT_PAD_X, y + 20 + EDIT_PAD_Y, w - 2 * EDIT_PAD_X, 20])]
}

/// The Profile page: what it is about on the left, the form on the right.
fn profile_layout(top: i32, fields: &[usize], form: Form, email: EmailArea) -> Vec<(usize, [i32; 4])> {
    let x = SIDEBAR + 48;
    let column = 360;
    let (fx, fw) = (WIDTH - 48 - 412, 412);
    let mut out = vec![
        (L_P_HEADING, [x, top + 26, column, 46]),
        (L_P_SERVER_SECTION, [x, top + 96, column, 18]),
        (L_P_SERVER, [x, top + 116, column, 62]),
        (L_P_ACCOUNT_SECTION, [x, top + 190, column, 18]),
        (L_P_ACCOUNT, [x, top + 210, column, 42]),
        (L_P_NOTE, [x, top + 262, column, 62]),
        (P_RETRY, [x, top + 334, 160, 38]),
        (P_MODE_LOGIN, [fx, top + 34, 202, 36]),
        (P_MODE_SIGNUP, [fx + 210, top + 34, 202, 36]),
        (P_CHANGE, [fx, top + 96, 202, 42]),
        (P_LOGOUT, [fx + 210, top + 96, 202, 42]),
    ];
    let mut y = top + 96;
    for &id in fields {
        out.extend(field_rects(id, fx, y, fw));
        y += 64;
    }
    match form {
        Form::Account => {
            out.push((L_P_MESSAGE, [fx, top + 152, fw, 46]));
            if email.offered {
                let bw = 134;
                out.push((L_P_EMAIL_SECTION, [fx, top + 206, fw, 18]));
                out.push((L_P_EMAIL, [fx, top + 226, fw, 40]));
                out.extend(field_rects(E_EMAIL, fx, top + 272, fw - bw - 8));
                out.push((P_EMAIL_SEND, [fx + fw - bw, top + 292, bw, 34]));
                out.extend(field_rects(E_CODE, fx, top + 336, fw - bw - 8));
                out.push((P_EMAIL_VERIFY, [fx + fw - bw, top + 356, bw, 34]));
                out.push((P_EMAIL_LATER, [fx, top + 398, 120, 22]));
            }
        }
        Form::Change | Form::Reset | Form::ResetCode => {
            out.push((P_SUBMIT, [fx, y + 4, 260, 46]));
            out.push((P_CANCEL, [fx + 268, y + 4, fw - 268, 46]));
            out.push((L_P_MESSAGE, [fx, y + 60, fw, 46]));
        }
        Form::LogIn | Form::SignUp => {
            out.push((P_SUBMIT, [fx, y + 4, fw, 46]));
            out.push((L_P_MESSAGE, [fx, y + 60, fw, 46]));
            out.push((P_FORGOT, [fx, y + 110, 200, 22]));
        }
    }
    out
}

/// Every text field.
const EDITS: [usize; 7] = [E_NAME, E_PASSWORD, E_NEW, E_CONFIRM, E_INVITE, E_EMAIL, E_CODE];

/// A text field's box around its EDIT (logical px).
const EDIT_PAD_X: i32 = 12;
const EDIT_PAD_Y: i32 = 7;

unsafe fn text_width(font: HFONT, text: &str) -> i32 {
    let dc = CreateCompatibleDC(null_mut());
    let old = SelectObject(dc, font);
    let mut size = SIZE::default();
    let text = wide(text);
    GetTextExtentPoint32W(dc, text.as_ptr(), text.len() as i32 - 1, &mut size);
    SelectObject(dc, old);
    DeleteDC(dc);
    size.cx
}

unsafe fn layout(app: &mut App) {
    let Some(fonts) = &app.fonts else {
        return;
    };
    let logical = |px: i32| (px as f32 / app.scale).ceil() as i32;
    let notice = app.game().manifest.notice.clone().unwrap_or_default();
    let measured = [
        logical(text_width(fonts.get(Font::Wordmark), &format!("[{}]", app.platform.name)) + 3),
        logical(text_width(fonts.get(Font::Body), &notice)),
        logical(text_width(fonts.get(Font::Link), "Known issues")),
        logical(text_width(fonts.get(Font::Link), "Report a problem…")),
    ];
    let fields = form_fields(app);
    for id in EDITS {
        SendMessageW(app.hwnd_of(id), WM_SETFONT, fonts.get(Font::Body) as usize, 0);
        SendMessageW(app.hwnd_of(id), EM_SETMARGINS, (EC_LEFTMARGIN | EC_RIGHTMARGIN) as usize, 0);
    }
    let email = EmailArea { offered: app.profile.email_offered };
    for (id, [x, y, w, h]) in logical_layout(app.slot.banner, measured, &fields, app.profile.form, app.games.len(), email) {
        let rect = RECT {
            left: app.sc(x),
            top: app.sc(y),
            right: app.sc(x + w),
            bottom: app.sc(y + h),
        };
        if let Some(control) = app.controls.iter_mut().find(|c| c.id == id) {
            control.rect = rect;
            MoveWindow(
                control.hwnd,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                0,
            );
        }
    }
    app.canvas_dirty = true;
}

/// The Profile page's form fields shown now, in order (the email part's
/// fields are not among them).
fn form_fields(app: &App) -> Vec<usize> {
    if app.profile.config.is_err() {
        return Vec::new();
    }
    match app.profile.form {
        Form::LogIn => vec![E_NAME, E_PASSWORD],
        Form::SignUp if app.profile.invite_required == Some(true) => vec![E_NAME, E_PASSWORD, E_CONFIRM, E_INVITE],
        Form::SignUp => vec![E_NAME, E_PASSWORD, E_CONFIRM],
        Form::Account => Vec::new(),
        Form::Change => vec![E_PASSWORD, E_NEW, E_CONFIRM],
        Form::Reset => vec![E_EMAIL],
        Form::ResetCode => vec![E_EMAIL, E_CODE, E_NEW, E_CONFIRM],
    }
}

/// Every text field shown now, in order (the form's, then the email part's).
fn profile_fields(app: &App) -> Vec<usize> {
    let mut fields = form_fields(app);
    if email_part(app) {
        fields.push(E_EMAIL);
        if email_pending(app) {
            fields.push(E_CODE);
        }
    }
    fields
}

/// The Account form's email part is shown.
fn email_part(app: &App) -> bool {
    app.profile.config.is_ok() && app.profile.form == Form::Account && app.profile.email_offered && app.profile.user.is_some()
}

/// An address waits for its code.
fn email_pending(app: &App) -> bool {
    app.profile.email.as_ref().is_some_and(|s| s.pending_email.is_some())
}

fn is_visible(app: &App, id: usize) -> bool {
    let game = app.view == View::Game;
    let settings = app.view == View::Settings;
    let profile = app.view == View::Profile;
    let p = &app.profile;
    let configured = p.config.is_ok();
    let fields = || profile_fields(app);
    let email = profile && email_part(app);
    match id {
        ACCOUNT => game && configured,
        L_P_HEADING | L_P_SERVER_SECTION | L_P_SERVER | L_P_ACCOUNT_SECTION | L_P_ACCOUNT | L_P_NOTE | L_P_MESSAGE => profile,
        L_P_EMAIL_SECTION | L_P_EMAIL | P_EMAIL_SEND => email,
        P_EMAIL_VERIFY => email && email_pending(app),
        P_EMAIL_LATER => email && p.email.as_ref().is_some_and(|s| s.prompt_email),
        P_FORGOT => profile && configured && p.email_offered && p.form == Form::LogIn,
        P_RETRY => profile && configured && !p.reached && p.error && !p.busy,
        P_MODE_LOGIN | P_MODE_SIGNUP => profile && configured && matches!(p.form, Form::LogIn | Form::SignUp),
        P_SUBMIT => profile && configured && !matches!(p.form, Form::Account),
        P_CHANGE | P_LOGOUT => profile && configured && p.form == Form::Account,
        P_CANCEL => profile && configured && matches!(p.form, Form::Change | Form::Reset | Form::ResetCode),
        id if EDITS.contains(&id) => profile && fields().contains(&id),
        id if EDITS.iter().any(|e| e + FIELD_CAPTION == id) => profile && fields().contains(&(id - FIELD_CAPTION)),
        PLAY => game && app.imported() && !app.slot.importing,
        TOGGLE => game && app.imported() && !app.slot.importing && app.game().manifest.launch_toggle.is_some(),
        IMPORT => game && (!app.imported() || app.slot.importing),
        // With the imported title art showing, its logo is the title.
        L_HERO => game && app.slot.backdrop.is_none(),
        SAVES | REPORT | GEAR | L_SUBTITLE | L_PROGRESS | L_STATUS => game,
        KNOWN_ISSUES | BANNER_REPORT | BANNER_CLOSE | L_BANNER | L_SEPARATOR => app.slot.banner,
        S_REIMPORT | S_DATA | S_SAVES | S_LOGS | L_HEADING => settings,
        id if (L_SECTION..L_VALUE + 5).contains(&id) => settings,
        _ => true,
    }
}

fn is_enabled(app: &App, id: usize) -> bool {
    match id {
        IMPORT | S_REIMPORT => !app.slot.busy,
        PLAY | TOGGLE => !app.slot.busy && app.slot.game_child.is_none(),
        P_SUBMIT | P_LOGOUT | P_CHANGE | P_MODE_LOGIN | P_MODE_SIGNUP | P_RETRY | P_FORGOT | P_EMAIL_SEND | P_EMAIL_VERIFY
        | P_EMAIL_LATER => !app.profile.busy,
        id if EDITS.contains(&id) => !app.profile.busy,
        _ => true,
    }
}

unsafe fn set_text(app: &App, id: usize, text: &str) {
    let hwnd = app.hwnd_of(id);
    if hwnd.is_null() {
        return;
    }
    let length = GetWindowTextLengthW(hwnd);
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
    if OsString::from_wide(&buffer[..copied.max(0) as usize]) != OsStr::new(text) {
        SetWindowTextW(hwnd, wide(text).as_ptr());
        InvalidateRect(hwnd, null(), 0);
    }
}

/// Bring every control's text, visibility and enabled state up to date and
/// repaint.
unsafe fn refresh(app: &mut App) {
    update_texts(app);
    let focus = GetFocus();
    let mut lost_focus = false;
    for control in &app.controls {
        let visible = is_visible(app, control.id);
        let enabled = is_enabled(app, control.id);
        if !visible && control.hwnd == focus {
            lost_focus = true;
        }
        EnableWindow(control.hwnd, i32::from(enabled));
        ShowWindow(control.hwnd, if visible { SW_SHOWNA } else { SW_HIDE });
    }
    if lost_focus || focus == app.hwnd || (!focus.is_null() && IsWindowEnabled(focus) == 0) {
        focus_first(app);
    }
    app.canvas_dirty = true;
    RedrawWindow(
        app.hwnd,
        null(),
        null_mut(),
        RDW_INVALIDATE | RDW_ALLCHILDREN,
    );
}

unsafe fn update_texts(app: &mut App) {
    let progress = if app.slot.importing {
        match &app.slot.step {
            Some((step, total, name)) => format!(
                "Step {step} of {total} · {} · {}",
                roses_launcher::friendly_step(name),
                elapsed(app.slot.started)
            ),
            None => format!("Starting the import · {}", elapsed(app.slot.started)),
        }
    } else if app.imported() {
        format!(
            "Played For {}",
            roses_launcher::format_play_time(app.played_seconds())
        )
    } else if app.slot.busy {
        String::new()
    } else {
        "Not imported yet".into()
    };
    set_text(app, L_PROGRESS, &progress);
    app.shown_second = if app.slot.importing {
        app.slot.started.elapsed().as_secs()
    } else {
        app.played_seconds()
    };
    let (status, status_error) = app.update_notice.clone().unwrap_or((app.slot.status.clone(), app.slot.status_error));
    set_text(app, L_STATUS, &status);
    let color = if status_error { ERROR_TEXT } else { MUTED };
    if let Some(control) = app.controls.iter_mut().find(|c| c.id == L_STATUS) {
        if let Kind::Label { color: c, .. } = &mut control.kind {
            if *c != color {
                *c = color;
                InvalidateRect(control.hwnd, null(), 0);
            }
        }
    }
    let manifest = app.game().manifest.clone();
    let noun = manifest.disc.noun();
    let disc = manifest.discs.first().map(|d| format!("{} {noun} ({})", d.label, d.serial)).unwrap_or_else(|| noun.into());
    let date = app.slot.game_json.as_deref().and_then(roses_launcher::imported_on).map_or(String::new(), |date| format!(" {date}"));
    let import = match app.slot.verified.as_ref().and_then(|v| v.summary.as_deref()) {
        _ if app.slot.importing => format!("Importing now. Your {noun} is only read, never changed."),
        Some(summary) => format!("Imported{date} from your {disc}: {summary}."),
        None if app.imported() => format!("Imported{date} from your {disc}."),
        None => manifest.texts.import_needed.clone().unwrap_or_else(|| format!("Not imported yet. You need your own {disc} of the game.")),
    };
    set_text(app, L_VALUE, &import);
    let data = app.game().data_dir.display().to_string();
    set_text(app, L_VALUE + 1, &data);
    let saves = manifest.texts.saves.clone().unwrap_or_else(|| "Saves are kept in the game's data folder.".into());
    set_text(app, L_VALUE + 2, &saves);
    set_text(app, L_VALUE + 3, "launcher.log and game.log (plus -previous copies). Send them with a short description; they contain no game data.");
    set_text(app, L_VALUE + 4, manifest.texts.controls.as_deref().unwrap_or(""));
    // The header: the platform's name, the launcher's and the game's versions.
    let brand = app.platform.name.clone();
    set_text(app, L_BRAND, &brand);
    let version = if manifest.version.is_empty() {
        format!("Launcher {}", roses_launcher::short_version())
    } else {
        format!("Launcher {}   ·   {} v{}", roses_launcher::short_version(), manifest.name, manifest.version)
    };
    set_text(app, L_VERSION, &version);
    set_text(app, L_HERO, manifest.hero.as_deref().unwrap_or(&manifest.name.to_uppercase()));
    set_text(app, L_SUBTITLE, manifest.subtitle.as_deref().unwrap_or(""));
    set_text(app, L_BANNER, manifest.notice.as_deref().unwrap_or(""));
    set_text(app, TOGGLE, manifest.launch_toggle.as_ref().map_or("", |t| t.label.as_str()));
    update_profile_texts(app);
    let reimport = if app.imported() { format!("Re-import {noun}…") } else { format!("Import {noun}…") };
    set_text(app, S_REIMPORT, &reimport);
    set_text(app, IMPORT, &format!("Import {noun}"));
}

fn elapsed(since: Instant) -> String {
    let seconds = since.elapsed().as_secs();
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

unsafe fn set_status(app: &mut App, text: &str, error: bool) {
    app.slot.status = text.to_owned();
    app.slot.status_error = error;
    update_texts(app);
}

unsafe fn tick(app: &mut App) {
    receive_events(app);
    receive_update(app);
    receive_profile(app);
    poll_game(app);
    poll_parked(app);
    if app.slot.importing {
        let import = app.hwnd_of(IMPORT);
        InvalidateRect(import, null(), 0);
    }
    let second = if app.slot.importing {
        app.slot.started.elapsed().as_secs()
    } else {
        app.played_seconds()
    };
    if second != app.shown_second {
        update_texts(app);
    }
    if let Some((path, since)) = app.capture.clone() {
        // Ready once checked, and with its backdrop when the game has one
        // (FM has none yet: no 90 s wait for it).
        let ready = !app.slot.busy
            && (app.slot.game_json.is_none() || app.slot.backdrop.is_some() || app.game().manifest.art.backdrop.is_none());
        if ready || since.elapsed().as_secs() > 90 {
            apply_pad_moves(app);
            let result = capture_png(app, &path);
            append_log(app, &match result {
                Ok(()) => format!("Captured the launcher to {}\n", path.display()),
                Err(error) => format!("Could not capture the launcher: {error}\n"),
            });
            app.capture = None;
            DestroyWindow(app.hwnd);
        }
    }
    if let Some((path, since)) = app.record.clone() {
        let ready = !app.slot.busy
            && (app.slot.game_json.is_none() || app.slot.backdrop.is_some() || app.game().manifest.art.backdrop.is_none());
        if ready || since.elapsed().as_secs() > 90 {
            app.record = None;
            let result = record_clip(app, &path);
            append_log(app, &match result {
                Ok(line) => format!("{line}\n"),
                Err(error) => format!("Could not record the launcher: {error}\n"),
            });
            DestroyWindow(app.hwnd);
        }
    }
}

// ---------------------------------------------------------------- game pads

/// The pad's place of a control.
fn pad_target(id: usize) -> Option<Target> {
    match id {
        IMPORT => Some(Target::Import),
        TOGGLE => Some(Target::Toggle),
        PLAY => Some(Target::Play),
        id => nav_game(id).map(Target::Game),
    }
}

fn pad_control(target: Target) -> usize {
    match target {
        Target::Import => IMPORT,
        Target::Toggle => TOGGLE,
        Target::Play => PLAY,
        Target::Game(i) => nav_id(i),
    }
}

/// The game page as the pad sees it: what can take the focus.
fn pad_page(app: &App) -> pad::Page {
    let usable = |id| is_visible(app, id) && is_enabled(app, id);
    pad::Page { games: app.games.len(), current: app.current, import: usable(IMPORT), toggle: usable(TOGGLE), play: usable(PLAY) }
}

/// Move the focus one step: on the game page between the sidebar's games,
/// Import, the toggle and Play; on the other pages through the tab stops.
unsafe fn pad_move(app: &mut App, dir: Dir) {
    let focus = GetFocus();
    if app.view != View::Game {
        let back = matches!(dir, Dir::Up | Dir::Left);
        let next = GetNextDlgTabItem(app.hwnd, focus, back as i32);
        if !next.is_null() {
            SetFocus(next);
        }
        return;
    }
    let from = if focus.is_null() {
        app.pad_focus.and_then(pad_target)
    } else {
        app.controls.iter().find(|c| c.hwnd == focus).and_then(|c| pad_target(c.id))
    };
    let id = pad_control(pad::step(from, dir, &pad_page(app)));
    app.pad_focus = Some(id);
    let hwnd = app.hwnd_of(id);
    if !hwnd.is_null() && app.capture.is_none() && app.record.is_none() {
        SetFocus(hwnd);
    }
}

/// Poll the pads and act on their presses while the launcher is in front.
unsafe fn poll_pads(hwnd: HWND, state: *mut App) {
    let (sample, note) = match (*state).pads.as_mut() {
        Some(pads) => pads.poll(),
        None => return,
    };
    if let Some(note) = note {
        append_log(&mut *state, &note);
    }
    if GetForegroundWindow() != hwnd {
        (*state).pad_input.reset();
        return;
    }
    let now = (*state).pad_clock.elapsed().as_millis() as u64;
    let actions = (*state).pad_input.update(sample.held, sample.edges, now);
    if actions.is_empty() {
        return;
    }
    // Show the focus as Tab does.
    SendMessageW(hwnd, WM_CHANGEUISTATE, (UIS_CLEAR as usize) | ((UISF_HIDEFOCUS as usize) << 16), 0);
    for action in actions {
        match action {
            Action::Move(dir) => pad_move(&mut *state, dir),
            // As Enter: the focused button, else the main one.
            Action::Activate => command(hwnd, state, 1),
            // As Escape.
            Action::Back => command(hwnd, state, 2),
            Action::Game(step) => {
                let app = &mut *state;
                let n = app.games.len() as i32;
                if n > 1 {
                    let next = (app.current as i32 + step).rem_euclid(n) as usize;
                    select_game(app, next);
                    app.pad_focus = None;
                }
            }
        }
        // A press may have opened a dialog (Import): the rest waits.
        if GetForegroundWindow() != hwnd {
            (*state).pad_input.reset();
            break;
        }
    }
}

/// A capture with [`ENV_PAD_MOVES`]: make those moves from the main button.
unsafe fn apply_pad_moves(app: &mut App) {
    let Ok(moves) = std::env::var(ENV_PAD_MOVES) else { return };
    app.pad_focus = Some(pad_control(pad_page(app).primary()));
    for word in moves.split(',').map(str::trim).filter(|w| !w.is_empty()) {
        let dir = match word.to_ascii_lowercase().as_str() {
            "up" => Dir::Up,
            "down" => Dir::Down,
            "left" => Dir::Left,
            "right" => Dir::Right,
            other => {
                append_log(app, &format!("{ENV_PAD_MOVES}: unknown move {other:?}
"));
                continue;
            }
        };
        pad_move(app, dir);
    }
    append_log(app, &format!("{ENV_PAD_MOVES}={moves}: focus on control {:?}
", app.pad_focus));
}

/// Draw the page off-screen (the backdrop, then every visible control
/// through its own `draw_item`) and write it as a PNG.
unsafe fn capture_png(app: &mut App, path: &Path) -> Result<(), String> {
    let (w, h) = (app.sc(WIDTH), app.sc(HEIGHT));
    let mut page = Dib::new(w, h).ok_or("no off-screen bitmap")?;
    paint_page(app, &page, None);
    let pixels = page.pixels();
    let mut rgb_bytes = Vec::with_capacity(pixels.len() * 3);
    for p in pixels.iter() {
        rgb_bytes.extend_from_slice(&[(p >> 16) as u8, (p >> 8) as u8, *p as u8]);
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let file = std::fs::File::create(path).map_err(|e| e.to_string())?;
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    encoder
        .write_header()
        .and_then(|mut writer| writer.write_image_data(&rgb_bytes))
        .map_err(|e| e.to_string())
}

/// [`ENV_RECORD`]: the scripted clip (`crate::record`), drawn off-screen
/// at 1.8x (1800x1080, 1:1 in the 1920x1080 segment) with a cursor; Play
/// is drawn pressed but nothing is started.
unsafe fn record_clip(app: &mut App, path: &Path) -> Result<String, String> {
    use crate::record::{self, Step, Target};
    use platform_video::{PixelLayout, Recorder, SegmentFormat, SourceSpec};
    let script = std::env::var(ENV_RECORD_SCRIPT).unwrap_or_else(|_| record::DEFAULT_SCRIPT.to_owned());
    let steps = record::parse(&script)?;
    let format = SegmentFormat::DEFAULT;
    // Draw at the size that fills the frame's height (1000x600 -> 1800x1080).
    app.scale = (format.width as f32 / WIDTH as f32).min(format.height as f32 / HEIGHT as f32);
    app.fonts = Some(Fonts::new(app.scale));
    let (w, h) = (app.sc(WIDTH), app.sc(HEIGHT));
    SetWindowPos(app.hwnd, null_mut(), 0, 0, w, h, SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOMOVE);
    layout(app);
    app.background_dirty = true;
    app.canvas_dirty = true;
    app.pad_focus = None;
    let mut recorder = Recorder::start(path, format, SourceSpec::square(w as u32, h as u32, PixelLayout::Bgrx), "roses-launcher")?;
    let page = Dib::new(w, h).ok_or("no off-screen bitmap")?;
    let cursor = LoadCursorW(null_mut(), IDC_ARROW);
    let cursor_size = (32.0 * app.scale).round() as i32;
    let fps = f64::from(format.fps);
    let silence = vec![0i16; format.samples_per_frame() as usize * 2];
    let mut at = (app.sc(780) as f64, app.sc(470) as f64);
    let mut notes = Vec::new();
    let control_at = |app: &App, (x, y): (f64, f64)| {
        app.controls
            .iter()
            .filter(|c| c.kind != Kind::Edit && is_visible(app, c.id))
            .find(|c| (c.rect.left as f64..c.rect.right as f64).contains(&x) && (c.rect.top as f64..c.rect.bottom as f64).contains(&y))
            .map(|c| (c.id, c.hwnd))
    };
    let frame = |app: &mut App, at: (f64, f64), pressed: Option<usize>, recorder: &mut Recorder| -> Result<(), String> {
        app.hover = control_at(app, at).map_or(null_mut(), |(_, hwnd)| hwnd);
        paint_page(app, &page, pressed);
        if !cursor.is_null() {
            DrawIconEx(page.dc, at.0.round() as i32, at.1.round() as i32, cursor, cursor_size, cursor_size, 0, null_mut(), DI_NORMAL);
            GdiFlush();
        }
        let bytes: Vec<u8> = std::slice::from_raw_parts(page.bits, (page.width * page.height) as usize).iter().flat_map(|p| p.to_le_bytes()).collect();
        recorder.audio(silence.clone())?;
        recorder.frame(bytes)
    };
    for step in &steps {
        match step {
            Step::Wait(s) => {
                for _ in 0..(s * fps).round() as u64 {
                    frame(app, at, None, &mut recorder)?;
                }
            }
            Step::Move(target, s) => {
                let id = match target {
                    Target::Game(id) => Some(nav_id(app.games.iter().position(|g| g.id() == id.as_str()).ok_or_else(|| format!("record script: no game {id:?}"))?)),
                    Target::Play => Some(PLAY),
                    Target::Import => Some(IMPORT),
                    Target::Toggle => Some(TOGGLE),
                    Target::Point(..) => None,
                };
                let to = match (target, id.and_then(|id| app.control(id).map(|c| c.rect))) {
                    (Target::Point(x, y), _) => ((x * app.scale) as f64, (y * app.scale) as f64),
                    (_, Some(r)) => ((r.left + r.right) as f64 / 2.0, (r.top + r.bottom) as f64 / 2.0),
                    _ => return Err(format!("record script: {target:?} is not on the page")),
                };
                let from = at;
                let n = ((s * fps).round() as u64).max(1);
                for i in 1..=n {
                    let t = record::ease(i as f64 / n as f64);
                    at = (from.0 + (to.0 - from.0) * t, from.1 + (to.1 - from.1) * t);
                    frame(app, at, None, &mut recorder)?;
                }
            }
            Step::Click => {
                let under = control_at(app, at).filter(|&(id, _)| is_enabled(app, id)).map(|(id, _)| id);
                for _ in 0..(record::PRESS_S * fps).round() as u64 {
                    frame(app, at, under, &mut recorder)?;
                }
                match under {
                    Some(id) if nav_game(id).is_some() => {
                        let index = nav_game(id).unwrap_or(0);
                        select_game(app, index);
                        app.pad_focus = None;
                        notes.push(format!("clicked {}", app.games[index].manifest.name));
                    }
                    Some(PLAY) => notes.push("clicked Play (drawn only: nothing was started)".into()),
                    Some(id) => notes.push(format!("clicked control {id} (drawn only)")),
                    None => notes.push("clicked nothing".into()),
                }
                for _ in 0..(record::CLICK_S * fps).round() as u64 {
                    frame(app, at, None, &mut recorder)?;
                }
            }
            Step::Pad(d) => {
                let dir = match d.as_str() {
                    "up" => Dir::Up,
                    "down" => Dir::Down,
                    "left" => Dir::Left,
                    _ => Dir::Right,
                };
                pad_move(app, dir);
            }
        }
    }
    let summary = recorder.finish()?;
    Ok(format!(
        "Recorded the launcher to {} ({} frames, {:.2} s of a {:.2} s script; {})",
        path.display(),
        summary.frames,
        summary.frames as f64 / fps,
        record::duration(&steps),
        notes.join(", ")
    ))
}

/// The page as the window would show it, into `page` (`pressed`: the
/// control drawn held down).
unsafe fn paint_page(app: &mut App, page: &Dib, pressed: Option<usize>) {
    refresh(app);
    ensure_canvas(app);
    let (w, h) = (app.sc(WIDTH), app.sc(HEIGHT));
    if let Some(canvas) = app.canvas.as_ref() {
        BitBlt(page.dc, 0, 0, w, h, canvas.dc, 0, 0, SRCCOPY);
    }
    let shown: Vec<(HWND, usize, RECT)> = app
        .controls
        .iter()
        .filter(|c| c.kind != Kind::Edit && is_visible(app, c.id))
        .map(|c| (c.hwnd, c.id, c.rect))
        .collect();
    for (hwnd, id, rect) in shown {
        let item = DRAWITEMSTRUCT {
            CtlType: 0,
            CtlID: id as u32,
            itemID: 0,
            itemAction: 0,
            itemState: if is_enabled(app, id) { 0 } else { ODS_DISABLED }
                | if app.pad_focus == Some(id) { ODS_FOCUS } else { 0 }
                | if pressed == Some(id) { ODS_SELECTED } else { 0 },
            hwndItem: hwnd,
            hDC: page.dc,
            rcItem: RECT { left: rect.left, top: rect.top, right: rect.right, bottom: rect.bottom },
            itemData: 0,
        };
        draw_item(app, &item);
    }
    GdiFlush();
}

// ---------------------------------------------------------------- painting

/// The window without its controls: title bar, sidebar and the backdrop.
unsafe fn build_background(app: &mut App) {
    let (w, h) = (app.sc(WIDTH), app.sc(HEIGHT));
    let mut pixels = vec![rgb(9, 9, 11); (w * h) as usize];
    let mut s = Surface {
        width: w,
        height: h,
        pixels: &mut pixels,
    };
    let (title, sidebar) = (app.sc(TITLE), app.sc(SIDEBAR));
    let (cx, cy, cw, ch) = (sidebar, title, w - sidebar, h - title);
    let (fx, fw) = (cx as f32, cw as f32);
    // A plain, very slightly graded near-black; no illustration of our own.
    let top = app.sc(TITLE + if app.slot.banner { BANNER } else { 0 });
    // With the title art the page matches its black so the art has no edge.
    let art = app.slot.backdrop.is_some() && app.view == View::Game;
    let (upper, lower) = if art {
        (rgb(4, 4, 5), rgb(4, 4, 5))
    } else {
        (rgb(17, 15, 18), rgb(9, 8, 10))
    };
    s.shade(cx, cy, cw, ch, |_, y| {
        (mix(upper, lower, (y - cy as f32) / ch as f32), 1.0)
    });
    // After an import: the title art from the player's own import, the logo
    // part only, above the bottom-right stack and fading into the background.
    if let Some(image) = app.slot.backdrop.as_ref().filter(|_| app.view == View::Game) {
        let (sy, sh) = (TITLE_ART_TOP, TITLE_ART_HEIGHT);
        let width = app.sc(TITLE_ART_WIDTH);
        let height = (width as f32 * sh as f32 / image.width as f32).round() as i32;
        let x = cx + (cw - width) / 2;
        let y = top + app.sc(14);
        s.image_fit_feathered(
            image,
            (0, sy, image.width as i32, sh),
            (x, y, width, height),
            app.sc(48) as f32,
        );
    }
    if app.view == View::Settings {
        s.blend_rect(cx, cy, cw, ch, rgb(10, 10, 13), 0.86);
    }
    if app.view == View::Profile {
        // A short crimson rule under the heading, and each text field's box.
        let x = app.sc(SIDEBAR + 48);
        s.fill(x, top + app.sc(74), app.sc(36), app.sc(2), ACCENT);
        let focus = GetFocus();
        for c in app.controls.iter().filter(|c| c.kind == Kind::Edit && is_visible(app, c.id)) {
            let (px, py) = (app.sc(EDIT_PAD_X), app.sc(EDIT_PAD_Y));
            let (bx, by) = (c.rect.left - px, c.rect.top - py);
            let (bw, bh) = (c.rect.right - c.rect.left + 2 * px, c.rect.bottom - c.rect.top + 2 * py);
            let r = 3.0 * app.scale;
            s.rounded(bx, by, bw, bh, r, FIELD_FILL, 1.0);
            let (border, width) = if c.hwnd == focus { (ACCENT, 1.5 * app.scale.max(1.0)) } else { (rgb(58, 58, 66), 1.0) };
            s.rounded_outline(bx, by, bw, bh, r, width, border, 1.0);
        }
    }
    if app.slot.banner {
        let banner = app.sc(BANNER);
        s.shade(cx, cy, cw, banner, |x, _| {
            let u = (x - fx) / fw;
            (mix(rgb(84, 16, 34), rgb(40, 10, 20), u), 0.94)
        });
        s.fill(cx, cy + banner - 1, cw, 1, rgb(122, 36, 56));
    }
    s.fill(0, 0, w, title, rgb(11, 11, 13));
    s.fill(0, title - 1, w, 1, rgb(30, 30, 35));
    s.fill(0, title, sidebar, h - title, rgb(14, 14, 17));
    s.fill(sidebar - 1, title, 1, h - title, rgb(30, 30, 35));
    let mark = app.sc(20);
    let icon = mark_image(mark as u32);
    s.rgba_over(&icon, app.sc(12), (title - mark) / 2, 1.0);
    // A hairline frame (Windows 11 draws its own rounded border over it).
    s.fill(0, 0, w, 1, rgb(44, 44, 50));
    s.fill(0, h - 1, w, 1, rgb(44, 44, 50));
    s.fill(0, 0, 1, h, rgb(44, 44, 50));
    s.fill(w - 1, 0, 1, h, rgb(44, 44, 50));
    app.background = pixels;
    app.background_dirty = false;
}

/// The oldrusty mark (src/mark.rs) at `size` px.
fn mark_image(size: u32) -> Rgba {
    Rgba {
        width: size,
        height: size,
        data: crate::mark::mark_rgba(size),
    }
}

unsafe fn ensure_canvas(app: &mut App) {
    if app.background_dirty || app.background.is_empty() {
        build_background(app);
        app.canvas_dirty = true;
    }
    let (w, h) = (app.sc(WIDTH), app.sc(HEIGHT));
    if app
        .canvas
        .as_ref()
        .is_none_or(|c| c.width != w || c.height != h)
    {
        app.canvas = Dib::new(w, h);
        app.canvas_dirty = true;
    }
    if !app.canvas_dirty {
        return;
    }
    if let Some(canvas) = app.canvas.as_mut() {
        let pixels = canvas.pixels();
        if pixels.len() == app.background.len() {
            pixels.copy_from_slice(&app.background);
        }
    }
    app.canvas_dirty = false;
}

unsafe fn draw_text(dc: HDC, font: HFONT, color: u32, rect: &mut RECT, text: &str, flags: DRAW_TEXT_FORMAT) {
    if text.is_empty() {
        return;
    }
    let old = SelectObject(dc, font);
    SetBkMode(dc, TRANSPARENT as i32);
    SetTextColor(dc, colorref(color));
    // NUL-terminated: DrawTextW with ellipsis flags may read one past the end.
    let text = wide(text);
    DrawTextW(dc, text.as_ptr(), text.len() as i32 - 1, rect, flags);
    SelectObject(dc, old);
}

unsafe fn window_text(hwnd: HWND) -> String {
    let length = GetWindowTextLengthW(hwnd);
    let mut buffer = vec![0u16; length as usize + 1];
    let copied = GetWindowTextW(hwnd, buffer.as_mut_ptr(), buffer.len() as i32);
    String::from_utf16_lossy(&buffer[..copied.max(0) as usize])
}

unsafe fn draw_item(app: &mut App, item: &DRAWITEMSTRUCT) {
    let Some(index) = app.controls.iter().position(|c| c.hwnd == item.hwndItem) else {
        return;
    };
    ensure_canvas(app);
    let (id, kind, rect) = {
        let c = &app.controls[index];
        (c.id, c.kind, c.rect)
    };
    let (w, h) = (
        item.rcItem.right - item.rcItem.left,
        item.rcItem.bottom - item.rcItem.top,
    );
    let Some(mut dib) = Dib::new(w, h) else {
        return;
    };
    let icon_width = app.sc(GAME_ICON_WIDTH).min(w) as u32;
    let logo = nav_game(id).and_then(|index| app.games.get(index)?.manifest.art.logo);
    if let Some(slot) = nav_game(id).and_then(|index| app.slot_of_mut(index)) {
        let width = icon_width;
        slot.game_icon = match (slot.backdrop.as_ref(), slot.game_icon.take()) {
            (Some(_), Some(icon)) if icon.width == width => Some(icon),
            (Some(backdrop), _) => Some(game_icon(backdrop, logo, width)),
            (None, _) => None,
        };
    }
    if let Some(canvas) = app.canvas.as_mut() {
        let width = canvas.width;
        let source = canvas.pixels();
        dib.surface().copy_from(source, width, rect.left, rect.top);
    }
    let state = item.itemState;
    let pressed = state & ODS_SELECTED != 0;
    let disabled = state & ODS_DISABLED != 0;
    let focused = state & ODS_FOCUS != 0 && state & ODS_NOFOCUSRECT == 0;
    let hover = app.hover == item.hwndItem && !disabled;
    let text = window_text(item.hwndItem);
    let fonts = app.fonts.as_ref().map(|f| f.0).unwrap_or([null_mut(); FONT_COUNT]);
    let font = |f: Font| fonts[f as usize];
    let radius = 3.0 * app.scale;
    let mut full = RECT {
        left: 0,
        top: 0,
        right: w,
        bottom: h,
    };
    let mut label: Option<(Font, u32, String, DRAW_TEXT_FORMAT)> = None;
    let centered = DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX;
    {
        let mut s = dib.surface();
        match kind {
            Kind::Primary if id == IMPORT && app.slot.importing => {
                s.rounded(0, 0, w, h, radius, rgb(26, 16, 20), 0.95);
                let fraction = app.progress();
                let filled = (w as f32 * fraction).round();
                let (hw, hh) = (w as f32 / 2.0, h as f32 / 2.0);
                let phase = (app.slot.started.elapsed().as_millis() % 1600) as f32 / 1600.0;
                let band = w as f32 * 0.25;
                let centre = phase * (filled + band) - band / 2.0;
                s.shade(0, 0, filled as i32 + 1, h, |x, y| {
                    let coverage = (0.5 - canvas::rounded_box(x, y, hw, hh, hw, hh, radius))
                        .clamp(0.0, 1.0)
                        * (filled - x + 0.5).clamp(0.0, 1.0);
                    let base = mix(rgb(112, 18, 40), rgb(184, 40, 66), x / w as f32);
                    let shimmer = (1.0 - ((x - centre).abs() / (band / 2.0)).min(1.0)).powi(2);
                    (mix(base, rgb(240, 120, 140), shimmer * 0.35), coverage)
                });
                s.rounded_outline(0, 0, w, h, radius, 1.0, rgb(120, 40, 58), 0.8);
                label = Some((
                    Font::Primary,
                    rgb(255, 255, 255),
                    format!("Importing…  {}%", (fraction * 100.0).floor() as u32),
                    centered,
                ));
            }
            Kind::Primary => {
                let base = if disabled {
                    rgb(46, 32, 37)
                } else if pressed {
                    rgb(96, 15, 31)
                } else if hover {
                    rgb(152, 28, 53)
                } else {
                    rgb(126, 21, 42)
                };
                let (hw, hh) = (w as f32 / 2.0, h as f32 / 2.0);
                s.shade(0, 0, w, h, |x, y| {
                    let coverage = (0.5 - canvas::rounded_box(x, y, hw, hh, hw, hh, radius))
                        .clamp(0.0, 1.0);
                    (mix(mix(base, rgb(255, 255, 255), 0.10), base, y / h as f32), coverage)
                });
                if !disabled {
                    s.blend_rect(app.sc(3), 1, w - app.sc(6), 1, rgb(255, 255, 255), 0.16);
                }
                let caption = if id == PLAY && app.slot.game_child.is_some() {
                    "Playing…".to_owned()
                } else if app.slot.busy && !app.slot.importing {
                    "Checking…".to_owned()
                } else {
                    text.clone()
                };
                let color = if disabled { rgb(150, 130, 136) } else { rgb(255, 255, 255) };
                label = Some((Font::Primary, color, caption, centered));
            }
            Kind::Secondary | Kind::Gear => {
                let (fill, alpha) = if pressed {
                    (rgb(6, 6, 8), 0.94)
                } else if hover {
                    (rgb(32, 32, 38), 0.94)
                } else {
                    (rgb(12, 12, 15), 0.88)
                };
                s.rounded(0, 0, w, h, radius, fill, alpha);
                let border = if hover { rgb(86, 86, 96) } else { rgb(50, 50, 58) };
                s.rounded_outline(0, 0, w, h, radius, 1.0, border, 1.0);
                let color = if disabled { rgb(100, 100, 108) } else { rgb(232, 232, 236) };
                if kind == Kind::Gear {
                    let (gx, gy, r) = (w as f32 / 2.0, h as f32 / 2.0, h as f32 * 0.24);
                    s.sdf(0, 0, w, h, color, 1.0, |x, y| canvas::gear(x, y, gx, gy, r));
                } else {
                    label = Some((Font::Body, color, text.clone(), centered));
                }
            }
            Kind::Title => {
                if hover || pressed {
                    let (fill, alpha) = if id == CLOSE {
                        (rgb(196, 43, 28), if pressed { 0.8 } else { 1.0 })
                    } else {
                        (rgb(255, 255, 255), if pressed { 0.05 } else { 0.09 })
                    };
                    s.blend_rect(0, 0, w, h, fill, alpha);
                }
                let color = if hover && id == CLOSE { rgb(255, 255, 255) } else { rgb(204, 204, 210) };
                let (mx, my, r) = (w as f32 / 2.0, h as f32 / 2.0, 5.0 * app.scale);
                let t = app.scale.max(1.0);
                match id {
                    CLOSE => s.sdf(0, 0, w, h, color, 1.0, |x, y| {
                        canvas::segment(x, y, mx - r, my - r, mx + r, my + r, t)
                            .min(canvas::segment(x, y, mx - r, my + r, mx + r, my - r, t))
                    }),
                    MINIMIZE => s.sdf(0, 0, w, h, color, 1.0, |x, y| {
                        canvas::segment(x, y, mx - r, my, mx + r, my, t)
                    }),
                    _ => label = Some((Font::Glyph, color, "?".into(), centered)),
                }
            }
            Kind::Nav => {
                let selected = (nav_game(id) == Some(app.current) && app.view == View::Game)
                    || (id == NAV_SETTINGS && app.view == View::Settings)
                    || (id == NAV_PROFILE && app.view == View::Profile);
                if selected || hover {
                    s.blend_rect(0, 0, w, h, rgb(255, 255, 255), if hover { 0.08 } else { 0.05 });
                }
                if selected {
                    s.fill(0, app.sc(10), app.sc(3), h - app.sc(20), ACCENT);
                }
                if let Some(index) = nav_game(id) {
                    let opacity = if selected || hover { 1.0 } else { 0.7 };
                    // The game's own logo once it is imported; before that its
                    // initials (manifest `art.initials`, Roses "DotR") on a
                    // plain tile: no drawing of ours stands for a game.
                    if let Some(logo) = app.slot_of(index).and_then(|slot| slot.game_icon.as_ref()) {
                        let (lw, lh) = (logo.width as i32, logo.height as i32);
                        s.rgba_over(logo, (w - lw) / 2, (h - lh) / 2, opacity);
                    } else {
                        let text = app.games[index].initials();
                        let long = text.chars().count() > 2;
                        let (bw, bh) = (app.sc(if long { 46 } else { 36 }), app.sc(36));
                        let (tx, ty) = ((w - bw) / 2, (h - bh) / 2);
                        s.rounded_outline(tx, ty, bw, bh, 4.0 * app.scale, 1.0, rgb(86, 86, 96), opacity);
                        let color = if selected || hover { rgb(236, 236, 240) } else { rgb(150, 150, 160) };
                        label = Some((if long { Font::Link } else { Font::Subtitle }, color, text, centered));
                    }
                } else if id == NAV_PROFILE {
                    let color = if selected || hover { rgb(236, 236, 240) } else { rgb(140, 140, 150) };
                    let (gx, gy, r) = (w as f32 / 2.0, h as f32 / 2.0, 11.0 * app.scale);
                    s.sdf(0, 0, w, h, color, 1.0, |x, y| canvas::person(x, y, gx, gy, r));
                    if app.profile.user.is_some() {
                        // Signed in: a small accent dot.
                        let (dx, dy, dr) = (gx + r * 0.85, gy - r * 0.75, 3.2 * app.scale);
                        s.sdf(0, 0, w, h, ACCENT, 1.0, |x, y| ((x - dx).powi(2) + (y - dy).powi(2)).sqrt() - dr);
                    }
                } else {
                    let color = if selected || hover { rgb(236, 236, 240) } else { rgb(140, 140, 150) };
                    let (gx, gy, r) = (w as f32 / 2.0, h as f32 / 2.0, 11.0 * app.scale);
                    s.sdf(0, 0, w, h, color, 1.0, |x, y| canvas::gear(x, y, gx, gy, r));
                }
            }
            Kind::Link if id == ACCOUNT => {
                // The stack's account line: quiet, right-aligned.
                let color = if hover { rgb(255, 218, 178) } else if app.profile.user.is_some() { rgb(206, 206, 214) } else { GOLD };
                let flags = DT_SINGLELINE | DT_VCENTER | DT_RIGHT | DT_NOPREFIX | DT_END_ELLIPSIS;
                label = Some((Font::Body, color, text.clone(), flags));
            }
            // The Update button, filling up while it downloads.
            Kind::Link if id == UPDATE && !matches!(app.update, Update::Link) => {
                let fill = if pressed { rgb(96, 15, 31) } else if hover { rgb(152, 28, 53) } else { rgb(126, 21, 42) };
                if let Update::Working { percent, .. } = app.update {
                    s.rounded(0, 0, w, h, radius, rgb(26, 16, 20), 0.95);
                    s.rounded(0, 0, w * percent as i32 / 100, h, radius, rgb(126, 21, 42), 1.0);
                } else {
                    s.rounded(0, 0, w, h, radius, fill, 1.0);
                }
                label = Some((Font::Link, rgb(255, 255, 255), text.clone(), centered));
            }
            Kind::Link => {
                let color = if hover { rgb(255, 218, 178) } else { GOLD };
                let f = if hover || pressed { Font::LinkHover } else { Font::Link };
                label = Some((f, color, text.clone(), centered));
            }
            Kind::Tab => {
                let selected = (id == P_MODE_LOGIN && app.profile.form == Form::LogIn)
                    || (id == P_MODE_SIGNUP && app.profile.form == Form::SignUp);
                let fill = if selected { rgb(44, 16, 25) } else if hover { rgb(32, 32, 38) } else { rgb(14, 14, 17) };
                s.rounded(0, 0, w, h, radius, fill, 0.96);
                let border = if selected { ACCENT } else if hover { rgb(86, 86, 96) } else { rgb(50, 50, 58) };
                s.rounded_outline(0, 0, w, h, radius, 1.0, border, 1.0);
                let color = if selected { rgb(255, 255, 255) } else if disabled { rgb(100, 100, 108) } else { rgb(190, 190, 198) };
                label = Some((if selected { Font::Link } else { Font::Body }, color, text.clone(), centered));
            }
            Kind::Edit => {}
            Kind::Check => {
                let on = toggle_on(app);
                let size = app.sc(16);
                let (bx, by) = (app.sc(4), (h - size) / 2);
                let r = 3.0 * app.scale;
                if on {
                    s.rounded(bx, by, size, size, r, if hover { rgb(152, 28, 53) } else { rgb(126, 21, 42) }, 1.0);
                    s.rounded_outline(bx, by, size, size, r, 1.0, ACCENT, 1.0);
                    let t = 1.6 * app.scale.max(1.0);
                    let (x0, y0, k) = (bx as f32, by as f32, size as f32);
                    s.sdf(0, 0, w, h, rgb(255, 255, 255), 1.0, |x, y| {
                        canvas::segment(x, y, x0 + k * 0.24, y0 + k * 0.52, x0 + k * 0.43, y0 + k * 0.72, t)
                            .min(canvas::segment(x, y, x0 + k * 0.43, y0 + k * 0.72, x0 + k * 0.78, y0 + k * 0.30, t))
                    });
                } else {
                    s.rounded(bx, by, size, size, r, rgb(12, 12, 15), 0.92);
                    let border = if hover { rgb(140, 140, 150) } else { rgb(96, 96, 106) };
                    s.rounded_outline(bx, by, size, size, r, 1.0, border, 1.0);
                }
                let color = if disabled { rgb(100, 100, 108) } else if on || hover { rgb(232, 232, 236) } else { MUTED };
                full.left = bx + size + app.sc(8);
                label = Some((Font::Body, color, text.clone(), DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX | DT_END_ELLIPSIS));
            }
            Kind::BannerClose => {
                if hover || pressed {
                    s.rounded(0, 0, w, h, radius, rgb(255, 255, 255), 0.12);
                }
                let (mx, my, r) = (w as f32 / 2.0, h as f32 / 2.0, 4.5 * app.scale);
                let t = app.scale.max(1.0) * 1.2;
                s.sdf(0, 0, w, h, rgb(236, 210, 216), 1.0, |x, y| {
                    canvas::segment(x, y, mx - r, my - r, mx + r, my + r, t)
                        .min(canvas::segment(x, y, mx - r, my + r, mx + r, my - r, t))
                });
            }
            Kind::Label {
                font: f,
                color,
                flags,
                ..
            } => {
                label = Some((f, color, text.clone(), flags));
            }
        }
        if focused && !matches!(kind, Kind::Label { .. }) {
            let inset = app.sc(2);
            s.rounded_outline(
                inset,
                inset,
                w - 2 * inset,
                h - 2 * inset,
                radius,
                1.5 * app.scale.max(1.0),
                FOCUS,
                0.95,
            );
        }
    }
    GdiFlush();
    if id == L_BRAND {
        // The wordmark: `[name]`, rust-red brackets around light monospace
        // text (the website's header).
        let f = font(Font::Wordmark);
        let mut x = 0;
        for (part, color) in [("[", ACCENT), (text.as_str(), TEXT), ("]", ACCENT)] {
            let mut rect = RECT { left: x, top: 0, right: w, bottom: h };
            draw_text(dib.dc, f, color, &mut rect, part, DT_SINGLELINE | DT_VCENTER | DT_NOPREFIX);
            x += text_width(f, part) + app.sc(1);
        }
        label = None;
    }
    if let Some((f, color, caption, flags)) = label {
        let spacing = match kind {
            Kind::Label { spacing, .. } => app.sc(spacing),
            _ => 0,
        };
        SetTextCharacterExtra(dib.dc, spacing);
        draw_text(dib.dc, font(f), color, &mut full, &caption, flags);
    }
    BitBlt(item.hDC, item.rcItem.left, item.rcItem.top, w, h, dib.dc, 0, 0, SRCCOPY);
}

// ---------------------------------------------------------------- tooltip

unsafe fn show_tooltip(app: &mut App) {
    let Some(fonts) = &app.fonts else {
        return;
    };
    let Some(nav) = app.control(nav_id(app.tooltip_game)) else {
        return;
    };
    let name = app.games[app.tooltip_game.min(app.games.len() - 1)].manifest.name.clone();
    let width = text_width(fonts.get(Font::Body), &name) + app.sc(24);
    let height = app.sc(30);
    let mut point = POINT {
        x: nav.rect.right + app.sc(6),
        y: (nav.rect.top + nav.rect.bottom - height) / 2,
    };
    ClientToScreen(app.hwnd, &mut point);
    SetWindowPos(
        app.tooltip,
        HWND_TOPMOST,
        point.x,
        point.y,
        width,
        height,
        SWP_NOACTIVATE | SWP_SHOWWINDOW,
    );
    InvalidateRect(app.tooltip, null(), 0);
}

unsafe fn hide_tooltip(app: &mut App) {
    if !app.tooltip.is_null() {
        ShowWindow(app.tooltip, SW_HIDE);
    }
}

unsafe extern "system" fn tooltip_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_NCHITTEST => HTTRANSPARENT as isize,
        WM_ERASEBKGND => 1,
        WM_PAINT => {
            let mut paint = PAINTSTRUCT::default();
            let dc = BeginPaint(hwnd, &mut paint);
            let state = GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const App;
            let mut rect = RECT::default();
            GetClientRect(hwnd, &mut rect);
            if let Some(mut dib) = Dib::new(rect.right, rect.bottom) {
                {
                    let mut s = dib.surface();
                    s.fill(0, 0, rect.right, rect.bottom, rgb(24, 24, 29));
                    s.rounded_outline(0, 0, rect.right, rect.bottom, 2.0, 1.0, rgb(70, 70, 80), 1.0);
                    s.fill(0, 0, 2, rect.bottom, ACCENT);
                }
                GdiFlush();
                if let Some(app) = state.as_ref().filter(|app| app.fonts.is_some()) {
                    let fonts = app.fonts.as_ref().expect("checked");
                    let name = &app.games[app.tooltip_game.min(app.games.len() - 1)].manifest.name;
                    let mut text_rect = rect;
                    draw_text(
                        dib.dc,
                        fonts.get(Font::Body),
                        TEXT,
                        &mut text_rect,
                        name,
                        DT_SINGLELINE | DT_VCENTER | DT_CENTER | DT_NOPREFIX,
                    );
                }
                BitBlt(dc, 0, 0, rect.right, rect.bottom, dib.dc, 0, 0, SRCCOPY);
            }
            EndPaint(hwnd, &paint);
            0
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

// ---------------------------------------------------------------- jobs

/// The shown game's launch toggle: on or off (remembered per game).
fn toggle_on(app: &App) -> bool {
    app.game().manifest.launch_toggle.as_ref().is_none_or(|t| app.slot.state.toggle(t))
}

unsafe fn flip_toggle(app: &mut App) {
    let Some(toggle) = app.game().manifest.launch_toggle.clone() else {
        return;
    };
    let on = !app.slot.state.toggle(&toggle);
    app.slot.state.toggles.insert(toggle.id.clone(), on);
    if let Err(error) = roses_launcher::save_state(&app.game().data_dir, &app.slot.state) {
        append_log(app, &format!("Could not save launcher settings: {error}
"));
    }
    append_log(app, &format!("{}: {}
", toggle.label, if on { "on" } else { "off" }));
    InvalidateRect(app.hwnd_of(TOGGLE), null(), 0);
}

unsafe fn play_game(app: &mut App) {
    let game = app.game().clone();
    // Development options the game takes (Roses: private research captures).
    let extra = roses_launcher::dev_args(&app.options, &game.manifest.executable.dev_options);
    let session = Some(app.profile.paths.session.clone()).filter(|p| p.is_file());
    let on = toggle_on(app);
    if let Some(toggle) = game.manifest.launch_toggle.as_ref() {
        append_log(app, &format!("{}: {}
", toggle.label, if on { "on" } else { "off" }));
    }
    match game.launch(on, &extra, session.as_deref()) {
        Ok(child) => {
            append_log(app, &format!("Started {} (process {}).\n", game.manifest.name, child.id()));
            app.slot.game_child = Some(child);
            app.slot.game_started = Some(Instant::now());
            let running = game.manifest.texts.running.clone().unwrap_or_else(|| "The game is running.".into());
            set_status(app, &running, false);
        }
        Err(error) => {
            append_log(app, &format!("{error}\n"));
            set_status(app, &first_line(&error), true);
        }
    }
    refresh(app);
}

unsafe fn poll_game(app: &mut App) {
    let Some(child) = app.slot.game_child.as_mut() else {
        return;
    };
    let result = child.try_wait();
    let data_dir = app.game().data_dir.clone();
    let (message, error) = match result {
        Ok(None) => return,
        Ok(Some(status)) if status.success() => ("The game closed.\n".to_owned(), false),
        Ok(Some(status)) => (
            format!(
                "The game stopped ({status}). Its messages are in {}. Click Report a problem to open the logs folder.\n",
                roses_launcher::logs_dir(&data_dir).join(roses_launcher::GAME_LOG).display()
            ),
            true,
        ),
        Err(error) => (format!("Could not query the game process: {error}\n"), true),
    };
    app.slot.game_child = None;
    if let Some(started) = app.slot.game_started.take() {
        app.slot.state.played_seconds += started.elapsed().as_secs();
        if let Err(error) = roses_launcher::save_state(&data_dir, &app.slot.state) {
            append_log(app, &format!("Could not save the play time: {error}\n"));
        }
    }
    append_log(app, &message);
    if error {
        set_status(
            app,
            "The game stopped unexpectedly. Click Report a problem to send the logs.",
            true,
        );
    } else {
        let ready = ready_status(app.game());
        set_status(app, &ready, false);
    }
    refresh(app);
}

fn ready_status(game: &Game) -> String {
    game.manifest.texts.ready.clone().unwrap_or_else(|| "Ready. Click Play.".into())
}

fn not_imported_status(game: &Game, iso: Option<&Path>) -> String {
    let disc = game.manifest.discs.first().map(|d| format!("{} copy of the game", d.label)).unwrap_or_else(|| "copy of the game".into());
    let kinds = roses_launcher::registry::disc_kinds(&game.manifest.disc);
    // "an .iso", "a .cue or .bin"
    let article = if kinds.strip_prefix('.').is_some_and(|k| k.starts_with(['a', 'e', 'i', 'o', 'u'])) { "an" } else { "a" };
    let serial = game.manifest.discs.first().map(|d| format!(" ({article} {kinds} of {})", d.serial)).unwrap_or_default();
    match iso.and_then(Path::file_name) {
        Some(name) => format!(
            "Ready to import {}. Requires your own {disc}; nothing is downloaded.",
            name.to_string_lossy()
        ),
        None => format!("Requires your own {disc}{serial}. Nothing is downloaded."),
    }
}

fn first_line(text: &str) -> String {
    let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or(text);
    let line: String = line.chars().take(220).collect();
    line
}

// ---------------------------------------------------------------- profile

unsafe fn update_profile_texts(app: &mut App) {
    let p = &app.profile;
    let server = match &p.config {
        Ok(c) => {
            let name = p.server_name.clone().or_else(|| c.name.clone());
            let how = match (c.tls, &c.pin) {
                (true, Some(_)) => "secure connection, pinned certificate",
                (true, None) => "secure connection",
                (false, _) => "this computer only, not encrypted",
            };
            match name {
                Some(name) => format!("{name}\n{} · {how}", c.server),
                None => format!("{}\n{how}", c.server),
            }
        }
        Err(e) => e.clone(),
    };
    let account = match (&p.user, p.busy && !p.reached) {
        (Some(user), _) => format!("Signed in as {user}"),
        (None, true) => "Checking…".into(),
        (None, false) => "Not signed in".into(),
    };
    let stack = match (&p.user, &p.email) {
        (Some(user), Some(status)) if status.prompt_email => format!("Signed in as {user} · add an email address"),
        (Some(user), _) => format!("Signed in as {user}"),
        (None, _) => "Not signed in · Profile".into(),
    };
    let email_line = match &p.email {
        Some(status) => roses_launcher::profile::email_line(status),
        None if p.busy => "Checking…".into(),
        None => "Add an address to reset a forgotten password.".into(),
    };
    let verified = p.email.as_ref().is_some_and(|s| s.email.is_some());
    let email_caption = if matches!(p.form, Form::Reset | Form::ResetCode) { "NAME OR EMAIL ADDRESS" } else if verified { "NEW EMAIL ADDRESS" } else { "EMAIL ADDRESS" };
    let (message, error) = (p.message.clone(), p.error);
    let submit = match p.form {
        Form::LogIn => "Log in",
        Form::SignUp => "Sign up",
        Form::Change => "Change password",
        Form::Reset => "Send reset code",
        Form::ResetCode => "Set new password",
        Form::Account => "",
    };
    let (password, confirm) = match p.form {
        Form::Change | Form::ResetCode => ("CURRENT PASSWORD", "CONFIRM NEW PASSWORD"),
        _ => ("PASSWORD", "CONFIRM PASSWORD"),
    };
    let name_caption = if p.form == Form::SignUp { "NAME  (3 TO 12 LETTERS OR DIGITS)" } else { "NAME" };
    set_text(app, L_P_SERVER, &server);
    set_text(app, L_P_ACCOUNT, &account);
    set_text(app, ACCOUNT, &stack);
    set_text(app, L_P_MESSAGE, &message);
    set_text(app, P_SUBMIT, submit);
    set_text(app, E_NAME + FIELD_CAPTION, name_caption);
    set_text(app, E_PASSWORD + FIELD_CAPTION, password);
    set_text(app, E_NEW + FIELD_CAPTION, "NEW PASSWORD  (8 CHARACTERS OR MORE)");
    set_text(app, E_CONFIRM + FIELD_CAPTION, confirm);
    set_text(app, E_INVITE + FIELD_CAPTION, "INVITE CODE");
    set_text(app, E_EMAIL + FIELD_CAPTION, email_caption);
    set_text(app, E_CODE + FIELD_CAPTION, "CODE FROM THE EMAIL  (XXXX-XXXX)");
    set_text(app, L_P_EMAIL, &email_line);
    set_text(app, P_EMAIL_SEND, if verified { "Change" } else { "Send code" });
    let color = if error { ERROR_TEXT } else { rgb(196, 220, 200) };
    if let Some(control) = app.controls.iter_mut().find(|c| c.id == L_P_MESSAGE) {
        if let Kind::Label { color: c, .. } = &mut control.kind {
            if *c != color {
                *c = color;
                InvalidateRect(control.hwnd, null(), 0);
            }
        }
    }
}

/// Show another form on the Profile page (passwords are cleared).
unsafe fn set_form(app: &mut App, form: Form) {
    for id in [E_PASSWORD, E_NEW, E_CONFIRM] {
        SetWindowTextW(app.hwnd_of(id), wide("").as_ptr());
    }
    if form == Form::Reset {
        // Start from the name typed for logging in, if any.
        let name = window_text(app.hwnd_of(E_NAME));
        SetWindowTextW(app.hwnd_of(E_EMAIL), wide(name.trim()).as_ptr());
        SetWindowTextW(app.hwnd_of(E_CODE), wide("").as_ptr());
    }
    if app.profile.form != form {
        app.profile.form = form;
        app.profile.message.clear();
        app.profile.error = false;
    }
    layout(app);
    app.background_dirty = true;
    refresh(app);
    focus_first(app);
}

/// Run an account job on a worker thread; its result arrives in `tick`.
unsafe fn profile_job(app: &mut App, job: roses_launcher::profile::Job) {
    let quiet = job == roses_launcher::profile::Job::Check;
    app.profile.busy = true;
    if !quiet || app.view == View::Profile {
        app.profile.message = "Contacting the server…".into();
        app.profile.error = false;
    }
    let what = job.label();
    app.profile.last_job = what;
    append_log(app, &format!("Profile: {what}\n"));
    let paths = app.profile.paths.clone();
    let sender = app.profile.sender.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| roses_launcher::profile::run(&paths, job))
            .unwrap_or_else(|_| Err("The account job stopped unexpectedly.".into()));
        let _ = sender.send(result);
    });
    refresh(app);
}

unsafe fn submit_profile(app: &mut App) {
    use roses_launcher::profile::{check_change, check_log_in, check_sign_up, Job};
    let text = |id| window_text(app.hwnd_of(id));
    let (name, password, new, confirm, invite) = (text(E_NAME), text(E_PASSWORD), text(E_NEW), text(E_CONFIRM), text(E_INVITE));
    let name = name.trim().to_string();
    let invite_required = app.profile.invite_required == Some(true);
    let checked = match app.profile.form {
        Form::SignUp => check_sign_up(&name, &password, &confirm, invite_required.then_some(invite.as_str()))
            .map(|()| Job::SignUp { username: name, password, invite }),
        Form::LogIn => check_log_in(&name, &password).map(|()| Job::LogIn { username: name, password }),
        Form::Change => check_change(&password, &new, &confirm)
            .map(|()| Job::ChangePassword { old_password: password, new_password: new }),
        Form::Reset => {
            let account = text(E_EMAIL).trim().to_string();
            if account.is_empty() {
                Err("Type your name or your verified email address.".to_string())
            } else {
                Ok(Job::RequestReset { account })
            }
        }
        Form::ResetCode => {
            let (account, code) = (text(E_EMAIL).trim().to_string(), text(E_CODE));
            if roses_launcher::profile::check_code(&code).is_err() {
                Err("Type the code from the email (XXXX-XXXX).".to_string())
            } else {
                // The current-password field is not shown: only the new one counts.
                check_change("unused", &new, &confirm).map(|()| Job::ResetPassword { account, code, new_password: new })
            }
        }
        Form::Account => return,
    };
    match checked {
        Ok(job) => profile_job(app, job),
        Err(message) => {
            app.profile.message = message;
            app.profile.error = true;
            refresh(app);
        }
    }
}

/// "Send code": give or change the account's address.
unsafe fn submit_email(app: &mut App) {
    let email = window_text(app.hwnd_of(E_EMAIL)).trim().to_string();
    match roses_launcher::profile::check_email(&email) {
        Ok(()) => profile_job(app, roses_launcher::profile::Job::SetEmail { email }),
        Err(message) => {
            app.profile.message = message;
            app.profile.error = true;
            refresh(app);
        }
    }
}

/// "Verify": the code from the verification email.
unsafe fn submit_code(app: &mut App) {
    let code = window_text(app.hwnd_of(E_CODE));
    match roses_launcher::profile::check_code(&code) {
        Ok(()) => profile_job(app, roses_launcher::profile::Job::VerifyEmail { code }),
        Err(message) => {
            app.profile.message = message;
            app.profile.error = true;
            refresh(app);
        }
    }
}

unsafe fn receive_profile(app: &mut App) {
    let Ok(result) = app.profile.receiver.try_recv() else { return };
    let p = &mut app.profile;
    p.busy = false;
    let before = p.form;
    match result {
        Ok(outcome) => {
            p.reached = true;
            p.server_name = outcome.server_name.or(p.server_name.take());
            p.invite_required = outcome.invite_required.or(p.invite_required);
            p.email_offered = outcome.email_offered.unwrap_or(p.email_offered);
            p.user = outcome.signed_in;
            if p.user.is_none() {
                p.email = None;
            } else if outcome.email.is_some() {
                p.email = outcome.email;
            }
            // A quiet check says nothing new: keep what the page shows
            // (a form message), only drop "Contacting the server…".
            if !outcome.message.is_empty() || p.message.starts_with("Contacting") {
                p.message = outcome.message;
                p.error = false;
            }
            p.form = match (&p.user, p.form, p.last_job) {
                (Some(_), ..) => Form::Account,
                // The reset code is on its way: ask for it and a new password.
                (None, Form::Reset, "request password reset") => Form::ResetCode,
                (None, Form::ResetCode, "reset password") => Form::LogIn,
                (None, Form::Account | Form::Change, _) => Form::LogIn,
                (None, form, _) => form,
            };
        }
        Err(message) => {
            // The page keeps what it knew (the saved name stays shown).
            if message == roses_launcher::profile::problem_text(platform_net::account::AuthProblem::SessionExpired) {
                p.user = None;
                p.form = Form::LogIn;
                p.reached = true;
            }
            p.message = message;
            p.error = true;
        }
    }
    let text = format!("Profile: {}\n", app.profile.message);
    append_log(app, &text);
    let form = app.profile.form;
    for id in [E_PASSWORD, E_NEW, E_CONFIRM] {
        if form != before || !app.profile.error {
            SetWindowTextW(app.hwnd_of(id), wide("").as_ptr());
        }
    }
    if !app.profile.error && matches!(app.profile.last_job, "verify email" | "set email" | "reset password") {
        SetWindowTextW(app.hwnd_of(E_CODE), wide("").as_ptr());
    }
    // The address field shows the account's address when it is empty.
    let shown = app.profile.email.as_ref().and_then(|s| s.pending_email.clone().or_else(|| s.email.clone()));
    if let (Some(address), Form::Account) = (shown, form) {
        if window_text(app.hwnd_of(E_EMAIL)).trim().is_empty() {
            SetWindowTextW(app.hwnd_of(E_EMAIL), wide(&address).as_ptr());
        }
    }
    layout(app);
    app.background_dirty = true;
    refresh(app);
    if app.view == View::Profile {
        focus_first(app);
    }
}

/// The disc dialog's title for `game` ("Choose your own PAL ISO of the game").
fn iso_title(game: &Game) -> String {
    let noun = game.manifest.disc.noun();
    match game.manifest.discs.first() {
        Some(d) => format!("Choose your own {} {noun} of {}", d.label.split(' ').next().unwrap_or(&d.label), game.manifest.name),
        None => format!("Choose your own {noun} of {}", game.manifest.name),
    }
}

/// Ask for the player's disc image (the kinds its manifest's
/// `disc.extensions` names; an .iso by default). Runs a nested message loop;
/// touches no App state.
unsafe fn choose_iso(hwnd: HWND, game: &Game) -> Option<PathBuf> {
    let mut buffer = vec![0u16; 32768];
    let filter: Vec<u16> = roses_launcher::registry::disc_filter(&game.manifest.disc).encode_utf16().chain(Some(0)).collect();
    let title = wide(iso_title(game));
    let extension = wide(game.manifest.disc.extensions()[0]);
    let mut dialog = OPENFILENAMEW {
        lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
        hwndOwner: hwnd,
        lpstrFilter: filter.as_ptr(),
        lpstrFile: buffer.as_mut_ptr(),
        nMaxFile: buffer.len() as u32,
        lpstrTitle: title.as_ptr(),
        lpstrDefExt: extension.as_ptr(),
        Flags: OFN_FILEMUSTEXIST
            | OFN_PATHMUSTEXIST
            | OFN_NOCHANGEDIR
            | OFN_EXPLORER
            | OFN_DONTADDTORECENT,
        ..Default::default()
    };
    if GetOpenFileNameW(&mut dialog) != 0 {
        let length = buffer.iter().position(|&c| c == 0).unwrap_or(0);
        Some(PathBuf::from(OsString::from_wide(&buffer[..length])))
    } else {
        None
    }
}

unsafe fn start_import(app: &mut App, iso: PathBuf) {
    app.slot.busy = true;
    app.slot.importing = true;
    app.slot.started = Instant::now();
    app.slot.step = None;
    app.slot.step_started = Instant::now();
    app.slot.line_buffer.clear();
    app.slot.iso = Some(iso.clone());
    append_log(app, &format!("Importing {}\n", iso.display()));
    let reading = format!("Reading your {} (it is never changed). This takes a few minutes.", app.game().manifest.disc.noun());
    set_status(app, &reading, false);
    refresh(app);
    let game = app.game().clone();
    // Development options the importer takes (its manifest's `dev_options`; Roses: none).
    let extra = roses_launcher::dev_args(&app.options, &game.manifest.importer.dev_options);
    let events = app.slot.sender.clone();
    std::thread::spawn(move || {
        let result = std::panic::catch_unwind(|| roses_launcher::registry::import(&game, &iso, &extra, &events))
            .unwrap_or_else(|_| Err("The import worker stopped unexpectedly.".into()));
        let event = if game.checks_imports() {
            Event::Complete(result.map(|done| done.verified))
        } else {
            Event::Imported(result.map(|done| Some(done.marker)))
        };
        let _ = events.send(event);
    });
}

unsafe fn load_backdrop(app: &mut App) {
    // The game's own art in its import (its manifest's `art.backdrop`).
    let backdrop = app.game().manifest.art.backdrop.clone();
    app.slot.backdrop = app.slot.game_json.as_deref().and_then(|game| {
        roses_launcher::backdrop_candidates(game, backdrop.as_deref())
            .into_iter()
            .find_map(|path| roses_launcher::load_png_rgba(&path).ok())
            .map(|(width, height, data)| Rgba {
                width,
                height,
                data,
            })
    });
    app.slot.game_icon = None;
    app.background_dirty = true;
}

/// The game's own logo (from the player's import) sized for the sidebar
/// tile at the current scale: black backing keyed out, then area-averaged
/// so it stays crisp at 100-200 %.
fn game_icon(backdrop: &Rgba, source: Option<[u32; 4]>, width: u32) -> Rgba {
    let whole = [0, 0, backdrop.width, backdrop.height];
    let [sx, sy, sw, sh] = source.unwrap_or(whole).map(|v| v as i32);
    let mut logo = Rgba {
        width: backdrop.width,
        height: backdrop.height,
        data: backdrop.data.clone(),
    };
    logo.key_out_black(20.0, 56.0);
    let height = ((width as f32 * sh as f32 / sw as f32).round() as u32).max(1);
    logo.area_scaled((sx, sy, sw, sh), width.max(1), height)
}

unsafe fn receive_events(app: &mut App) {
    let mut changed = false;
    // Each worker emits at most two 64 KiB streams and one completion message.
    for _ in 0..64 {
        let Ok(event) = app.slot.receiver.try_recv() else {
            break;
        };
        changed = true;
        match event {
            Event::Log(text) => {
                append_log(app, &text);
                app.slot.line_buffer.push_str(&text);
                while let Some(end) = app.slot.line_buffer.find('\n') {
                    let line: String = app.slot.line_buffer.drain(..=end).collect();
                    if let Some(step) = roses_launcher::parse_step(&line) {
                        app.slot.step = Some(step);
                        app.slot.step_started = Instant::now();
                    }
                }
                if app.slot.line_buffer.len() > 4096 {
                    app.slot.line_buffer.clear();
                }
            }
            Event::Complete(result) => {
                app.slot.busy = false;
                match result {
                    Ok(Some(verified)) => {
                        if app.slot.importing {
                            let seconds = app.slot.started.elapsed().as_secs();
                            append_log(
                                app,
                                &format!("Import finished in {} min {} s.\n", seconds / 60, seconds % 60),
                            );
                        }
                        append_log(app, "Library metadata verified. Click Play to start from the title screen.\n");
                        app.slot.verified = Some(verified);
                        app.slot.game_json = app.game().latest_marker();
                        load_backdrop(app);
                        let text = if app.slot.importing {
                            "Import complete. Click Play to start from the title screen.".to_owned()
                        } else {
                            ready_status(app.game())
                        };
                        set_status(app, &text, false);
                    }
                    Ok(None) => {
                        let text = not_imported_status(app.game(), app.slot.iso.as_deref());
                        set_status(app, &text, false);
                    }
                    Err(error) => {
                        append_log(app, &format!("{error}\n"));
                        let text = if app.slot.importing {
                            // Let the next click choose a different file.
                            app.slot.iso = None;
                            format!("Import failed: {} Report a problem opens the logs.", first_line(&error))
                        } else {
                            format!("Your earlier import could not be loaded. Import your {} again, or click Report a problem.", app.game().manifest.disc.noun())
                        };
                        set_status(app, &text, true);
                    }
                }
                app.slot.importing = false;
                app.slot.step = None;
            }
            Event::Imported(result) => {
                app.slot.busy = false;
                match result {
                    Ok(Some(marker)) => {
                        let text = if app.slot.importing { "Import complete. Click Play.".to_owned() } else { ready_status(app.game()) };
                        app.slot.game_json = Some(marker);
                        set_status(app, &text, false);
                    }
                    Ok(None) => {
                        let text = not_imported_status(app.game(), app.slot.iso.as_deref());
                        set_status(app, &text, false);
                    }
                    Err(error) => {
                        append_log(app, &format!("{error}\n"));
                        if app.slot.importing {
                            app.slot.iso = None;
                        }
                        set_status(app, &format!("Import failed: {} Report a problem opens the logs.", first_line(&error)), true);
                    }
                }
                app.slot.importing = false;
                app.slot.step = None;
            }
        }
    }
    if changed {
        refresh(app);
    }
}

/// Open the logs folder and say which files to send.
unsafe fn report_problem(app: &mut App) {
    // The game's issue page, when its links are set up (else the logs only).
    if let Some(url) = app.game().manifest.links.issues.clone() {
        open_configured(app, Some(&url), "");
    }
    let logs = roses_launcher::logs_dir(&app.game().data_dir);
    let _ = std::fs::create_dir_all(&logs);
    append_log(
        app,
        &format!(
            "To report a problem: say what you were doing and send the files in {} (launcher.log and game.log, plus their -previous copies). They contain no game data.\n",
            logs.display()
        ),
    );
    match roses_launcher::open_folder(&logs) {
        Ok(()) => set_status(
            app,
            "Opened the logs folder: send launcher.log and game.log with a short description of what happened.",
            false,
        ),
        Err(error) => set_status(app, &format!("Could not open the logs folder: {error}"), true),
    }
}

unsafe fn append_log(app: &mut App, text: &str) {
    if let Some(file) = app.log_file.as_mut() {
        use std::io::Write;
        let _ = file.write_all(text.as_bytes());
        let _ = file.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_fits_small_and_scaled_work_areas() {
        for (dpi, work) in [
            (96, (0, 0, 1024, 550)),
            (144, (-1280, 40, 0, 720)),
            (96, (0, 0, 640, 480)),
            (120, (0, 0, 1920, 1040)),
            (192, (0, 0, 2560, 1400)),
            (168, (0, 0, 1920, 1032)),
        ] {
            let work = RECT {
                left: work.0,
                top: work.1,
                right: work.2,
                bottom: work.3,
            };
            let scale = window_scale(dpi, work);
            assert!(scale <= dpi as f32 / 96.0 + 1e-4);
            if work.right - work.left >= 700 {
                assert!((WIDTH as f32 * scale).round() as i32 <= work.right - work.left);
                assert!((HEIGHT as f32 * scale).round() as i32 <= work.bottom - work.top);
            }
        }
        let full_hd = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        assert_eq!(window_scale(96, full_hd), 1.0);
        assert_eq!(window_scale(144, full_hd), 1.5);
    }

    #[test]
    fn the_game_icon_fits_the_sidebar_tile_at_every_scale() {
        // A stand-in for texetc-00: black, with a bright block in the logo area.
        let (w, h) = (640u32, 448u32);
        let mut data = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                let inside = (100..540).contains(&x) && (80..260).contains(&y);
                let v = if inside { 230 } else { 12 };
                data[i..i + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        let backdrop = Rgba { width: w, height: h, data };
        for scale in [1.0f32, 1.25, 1.5, 1.75, 2.0] {
            let width = (GAME_ICON_WIDTH as f32 * scale).round() as u32;
            let icon = game_icon(&backdrop, Some([24, 30, 592, 276]), width);
            assert_eq!(icon.width, width);
            assert!(icon.width as f32 <= SIDEBAR as f32 * scale);
            assert!(icon.height as f32 <= 56.0 * scale - 20.0 * scale, "{scale}");
            // The black backing is keyed out, the logo itself is opaque.
            assert_eq!(icon.data[3], 0);
            let centre = ((icon.height / 2 * icon.width + icon.width / 2) * 4) as usize;
            assert_eq!(icon.data[centre + 3], 255);
            assert_eq!(icon.data[centre], 230);
        }
    }

    #[test]
    fn controls_stay_inside_the_window_and_do_not_overlap() {
        for banner in [false, true] {
            let rects = logical_layout(banner, [40, 470, 80, 120], &[], Form::Account, 3, EmailArea { offered: true });
            for (id, [x, y, w, h]) in &rects {
                assert!(*x >= 0 && *y >= 0 && w > &0 && h > &0, "{id}");
                assert!(x + w <= WIDTH && y + h <= HEIGHT, "{id} outside");
            }
            // Controls shown together in the game view must not overlap
            // (IMPORT and PLAY share a place; only one is visible).
            let game: Vec<_> = rects
                .iter()
                .filter(|(id, _)| {
                    [IMPORT, TOGGLE, SAVES, REPORT, GEAR, ACCOUNT, L_HERO, L_SUBTITLE, L_PROGRESS, L_STATUS, NAV_GAME, NAV_GAMES + 1, NAV_GAMES + 2, NAV_PROFILE, NAV_SETTINGS, L_BRAND, L_VERSION, UPDATE, HELP]
                        .contains(id)
                })
                .collect();
            for (i, (a, ra)) in game.iter().enumerate() {
                for (b, rb) in &game[i + 1..] {
                    let apart = ra[0] + ra[2] <= rb[0]
                        || rb[0] + rb[2] <= ra[0]
                        || ra[1] + ra[3] <= rb[1]
                        || rb[1] + rb[3] <= ra[1];
                    assert!(apart, "{a} overlaps {b}");
                }
            }
            // Every Profile form fits and its controls do not overlap.
            for (form, fields) in [
                (Form::LogIn, vec![E_NAME, E_PASSWORD]),
                (Form::SignUp, vec![E_NAME, E_PASSWORD, E_CONFIRM]),
                (Form::SignUp, vec![E_NAME, E_PASSWORD, E_CONFIRM, E_INVITE]),
                (Form::Change, vec![E_PASSWORD, E_NEW, E_CONFIRM]),
                (Form::Account, vec![]),
                (Form::Reset, vec![E_EMAIL]),
                (Form::ResetCode, vec![E_EMAIL, E_CODE, E_NEW, E_CONFIRM]),
            ] {
                let top = TITLE + if banner { BANNER } else { 0 };
                let mut rects = profile_layout(top, &fields, form, EmailArea { offered: true });
                let all_fields: Vec<usize> = if form == Form::Account { vec![E_EMAIL, E_CODE] } else { fields.clone() };
                rects.retain(|(id, _)| match *id {
                    P_MODE_LOGIN | P_MODE_SIGNUP => matches!(form, Form::LogIn | Form::SignUp),
                    P_CHANGE | P_LOGOUT => form == Form::Account,
                    _ => true,
                });
                for (id, r) in rects.iter_mut() {
                    if all_fields.contains(id) {
                        // The field's whole box.
                        *r = [r[0] - EDIT_PAD_X, r[1] - EDIT_PAD_Y, r[2] + 2 * EDIT_PAD_X, r[3] + 2 * EDIT_PAD_Y];
                    }
                    assert!(r[0] >= SIDEBAR && r[1] >= top && r[0] + r[2] <= WIDTH && r[1] + r[3] <= HEIGHT, "{form:?} {id}");
                }
                for (i, (a, ra)) in rects.iter().enumerate() {
                    for (b, rb) in &rects[i + 1..] {
                        let apart = ra[0] + ra[2] <= rb[0]
                            || rb[0] + rb[2] <= ra[0]
                            || ra[1] + ra[3] <= rb[1]
                            || rb[1] + rb[3] <= ra[1];
                        assert!(apart, "{form:?}: {a} overlaps {b}");
                    }
                }
            }
        }
    }

    #[test]
    fn sidebar_entries_name_their_games() {
        for index in 0..roses_launcher::registry::MAX_GAMES {
            assert_eq!(nav_game(nav_id(index)), Some(index));
        }
        assert_eq!(nav_game(NAV_PROFILE), None);
        assert_eq!(nav_game(NAV_SETTINGS), None);
        assert_eq!(roses_launcher::initials("Example Game"), "EG");
        assert_eq!(roses_launcher::initials("Yu-Gi-Oh! The Duelists of the Roses"), "YT");
        assert_eq!(roses_launcher::initials("lowercase only"), "L");
        // Every sidebar entry fits above Settings with the most games.
        let rects = logical_layout(false, [40, 470, 80, 120], &[], Form::Account, roses_launcher::registry::MAX_GAMES, EmailArea::default());
        let bottom = |id: usize| rects.iter().find(|(i, _)| *i == id).map(|(_, r)| r[1] + r[3]).unwrap();
        let settings_top = rects.iter().find(|(i, _)| *i == NAV_SETTINGS).unwrap().1[1];
        assert!(bottom(NAV_PROFILE) <= settings_top);
        assert!(bottom(nav_id(roses_launcher::registry::MAX_GAMES - 1)) <= settings_top);
    }
}
