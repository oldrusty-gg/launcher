//! Game pads in the launcher (Claude, 14 Oct 2026): the d-pad or the left
//! stick moves the focus between the sidebar's games, Import, the launch
//! toggle ("Online Play and extras") and Play; A / Cross (or Start)
//! activates the focused button, B / Circle goes back, LB/RB or L1/R1
//! switch to the previous / next game.
//!
//! Read the way the game reads them (`crates/roses-game/src/pads.rs`
//! and `pad_input.rs`), so the buttons are the same by position:
//! - XInput controllers (Xbox and compatibles) through windows-sys
//!   (`Win32_UI_Input_XboxController`, xinput1_4.dll, part of Windows);
//!   empty ports are asked again every 2 s on a thread of their own.
//! - DualSense / DualShock 4 as raw HID input reports (Windows has no XInput
//!   for them): the game's reader, copied — a scanner thread every 2 s, a
//!   reader thread per pad, no feature report sent. `ROSES_NO_HID_PADS=1`
//!   turns it off, as in the game.
//!
//! Gamepads count only while the launcher is the foreground window; the
//! mouse and keyboard are untouched (the pad only moves the focus and
//! clicks the focused button as Enter does).

/// Pad bits as the game builds them (`pads::bit`).
pub mod bit {
    pub const L2: u16 = 0x0001;
    pub const R2: u16 = 0x0002;
    pub const L1: u16 = 0x0004;
    pub const R1: u16 = 0x0008;
    pub const TRIANGLE: u16 = 0x0010;
    pub const CIRCLE: u16 = 0x0020;
    pub const CROSS: u16 = 0x0040;
    pub const SQUARE: u16 = 0x0080;
    pub const SELECT: u16 = 0x0100;
    pub const L3: u16 = 0x0200;
    pub const R3: u16 = 0x0400;
    pub const START: u16 = 0x0800;
    pub const UP: u16 = 0x1000;
    pub const RIGHT: u16 = 0x2000;
    pub const DOWN: u16 = 0x4000;
    pub const LEFT: u16 = 0x8000;
}

/// XInput's trigger threshold (also used for the Sony triggers).
pub const TRIGGER_THRESHOLD: u8 = 30;
/// The left stick moves the focus past half its travel.
pub const STICK_THRESHOLD: i16 = 16_384;
/// The same on the Sony stick bytes (0 = left / up, 0x80 = centre).
pub const STICK_BYTE_THRESHOLD: u8 = 64;
/// A held direction repeats after this long, then every [`REPEAT_EVERY_MS`].
pub const REPEAT_AFTER_MS: u64 = 400;
pub const REPEAT_EVERY_MS: u64 = 150;

/// XInput's `XINPUT_GAMEPAD` → pad bits (the game's `pads::xinput`), the
/// left stick added as the d-pad.
pub fn xinput_bits(buttons: u16, left_trigger: u8, right_trigger: u8, lx: i16, ly: i16) -> u16 {
    const MAP: [(u16, u16); 14] = [
        (0x0001, bit::UP),
        (0x0002, bit::DOWN),
        (0x0004, bit::LEFT),
        (0x0008, bit::RIGHT),
        (0x0010, bit::START),
        (0x0020, bit::SELECT),
        (0x0040, bit::L3),
        (0x0080, bit::R3),
        (0x0100, bit::L1),
        (0x0200, bit::R1),
        (0x1000, bit::CROSS),
        (0x2000, bit::CIRCLE),
        (0x4000, bit::SQUARE),
        (0x8000, bit::TRIANGLE),
    ];
    let mut out = MAP.iter().filter(|(x, _)| buttons & x != 0).fold(0, |a, (_, p)| a | p);
    if left_trigger > TRIGGER_THRESHOLD {
        out |= bit::L2;
    }
    if right_trigger > TRIGGER_THRESHOLD {
        out |= bit::R2;
    }
    if ly > STICK_THRESHOLD {
        out |= bit::UP;
    } else if ly < -STICK_THRESHOLD {
        out |= bit::DOWN;
    }
    if lx > STICK_THRESHOLD {
        out |= bit::RIGHT;
    } else if lx < -STICK_THRESHOLD {
        out |= bit::LEFT;
    }
    out
}

/// Sony HID input reports (the game's `pads::sony`), the left stick added
/// as the d-pad.
pub mod sony {
    use super::bit;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Model {
        DualSense,
        DualShock4,
    }

    pub fn model(vendor: u16, product: u16) -> Option<Model> {
        if vendor != 0x054C {
            return None;
        }
        match product {
            0x0CE6 | 0x0DF2 => Some(Model::DualSense),
            0x05C4 | 0x09CC | 0x0BA0 => Some(Model::DualShock4),
            _ => None,
        }
    }

    pub fn bluetooth(device_path: &str, input_report_len: usize) -> bool {
        device_path.to_ascii_lowercase().contains("00001124-0000-1000-8000-00805f9b34fb") || input_report_len > 64
    }

    pub fn parse(model: Model, bluetooth: bool, report: &[u8]) -> Option<u16> {
        match (model, report.first()?) {
            (Model::DualSense, 0x01) if !bluetooth => full(report, 1),
            (Model::DualSense, 0x31) => full(report, 2),
            (Model::DualSense, 0x01) | (Model::DualShock4, 0x01) => common(report, 1),
            (Model::DualShock4, 0x11) => common(report, 3),
            _ => None,
        }
    }

    fn full(r: &[u8], o: usize) -> Option<u16> {
        let b = r.get(o..o + 9)?;
        Some(buttons(b[7], b[8], b[4], b[5]) | stick(b[0], b[1]))
    }

    fn common(r: &[u8], o: usize) -> Option<u16> {
        let b = r.get(o..o + 9)?;
        Some(buttons(b[4], b[5], b[7], b[8]) | stick(b[0], b[1]))
    }

    fn stick(x: u8, y: u8) -> u16 {
        let t = super::STICK_BYTE_THRESHOLD;
        let mut out = 0;
        if y < 0x80 - t {
            out |= bit::UP;
        } else if y > 0x80 + t {
            out |= bit::DOWN;
        }
        if x < 0x80 - t {
            out |= bit::LEFT;
        } else if x > 0x80 + t {
            out |= bit::RIGHT;
        }
        out
    }

    fn buttons(b0: u8, b1: u8, l2: u8, r2: u8) -> u16 {
        const HAT: [u16; 8] = [
            bit::UP,
            bit::UP | bit::RIGHT,
            bit::RIGHT,
            bit::DOWN | bit::RIGHT,
            bit::DOWN,
            bit::DOWN | bit::LEFT,
            bit::LEFT,
            bit::UP | bit::LEFT,
        ];
        const B0: [u16; 4] = [bit::SQUARE, bit::CROSS, bit::CIRCLE, bit::TRIANGLE];
        const B1: [u16; 8] = [bit::L1, bit::R1, 0, 0, bit::SELECT, bit::START, bit::L3, bit::R3];
        let mut out = HAT.get(usize::from(b0 & 0x0F)).copied().unwrap_or(0);
        for (i, &p) in B0.iter().enumerate() {
            if b0 & (0x10 << i) != 0 {
                out |= p;
            }
        }
        for (i, &p) in B1.iter().enumerate() {
            if b1 & (1 << i) != 0 {
                out |= p;
            }
        }
        if l2 > super::TRIGGER_THRESHOLD {
            out |= bit::L2;
        }
        if r2 > super::TRIGGER_THRESHOLD {
            out |= bit::R2;
        }
        out
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Up,
    Down,
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Move(Dir),
    /// A / Cross or Start: click the focused button (as Enter).
    Activate,
    /// B / Circle: back (as Escape).
    Back,
    /// LB/RB, L1/R1: the previous (-1) or next (+1) game.
    Game(i32),
}

/// Presses → actions, with a repeat for a held direction.
#[derive(Clone, Debug, Default)]
pub struct Input {
    held: u16,
    /// The direction held and when it next repeats (ms since the press).
    repeat: Option<(u16, u64)>,
}

impl Input {
    /// This poll's actions: `held` = buttons down now, `edges` = buttons
    /// that went down since the last poll (a tap between polls), `now_ms`
    /// = a clock in milliseconds.
    pub fn update(&mut self, held: u16, edges: u16, now_ms: u64) -> Vec<Action> {
        let pressed = (held & !self.held) | edges;
        self.held = held;
        let mut out = Vec::new();
        let dirs = [(bit::UP, Dir::Up), (bit::DOWN, Dir::Down), (bit::LEFT, Dir::Left), (bit::RIGHT, Dir::Right)];
        for (b, d) in dirs {
            if pressed & b != 0 {
                out.push(Action::Move(d));
                self.repeat = Some((b, now_ms + REPEAT_AFTER_MS));
            }
        }
        match self.repeat {
            Some((b, _)) if held & b == 0 => self.repeat = None,
            Some((b, at)) if now_ms >= at && pressed & b == 0 => {
                out.extend(dirs.iter().filter(|(x, _)| *x == b).map(|&(_, d)| Action::Move(d)));
                self.repeat = Some((b, now_ms + REPEAT_EVERY_MS));
            }
            _ => {}
        }
        if pressed & (bit::CROSS | bit::START) != 0 {
            out.push(Action::Activate);
        }
        if pressed & bit::CIRCLE != 0 {
            out.push(Action::Back);
        }
        if pressed & bit::L1 != 0 {
            out.push(Action::Game(-1));
        }
        if pressed & bit::R1 != 0 {
            out.push(Action::Game(1));
        }
        out
    }

    /// Forget the held buttons (the launcher lost the foreground).
    pub fn reset(&mut self) {
        *self = Self::default();
    }
}

/// What the pad moves between on the game page.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// Sidebar entry of game `i`.
    Game(usize),
    Import,
    Toggle,
    Play,
}

/// The game page as the pad sees it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Page {
    pub games: usize,
    /// The game shown.
    pub current: usize,
    pub import: bool,
    pub toggle: bool,
    pub play: bool,
}

impl Page {
    fn shown(&self, t: Target) -> bool {
        match t {
            Target::Game(i) => i < self.games,
            Target::Import => self.import,
            Target::Toggle => self.toggle,
            Target::Play => self.play,
        }
    }

    /// The main button: Play, else Import, else the game's sidebar entry.
    pub fn primary(&self) -> Target {
        if self.play {
            Target::Play
        } else if self.import {
            Target::Import
        } else {
            Target::Game(self.current.min(self.games.saturating_sub(1)))
        }
    }
}

/// Where a move from `from` goes. The layout: the games stacked in the
/// sidebar on the left; bottom right, the toggle left of Play (Import in
/// Play's place before an import). Anything else focused (or nothing) goes
/// to the main button first.
pub fn step(from: Option<Target>, dir: Dir, page: &Page) -> Target {
    let Some(from) = from.filter(|&t| page.shown(t)) else {
        return page.primary();
    };
    let sidebar = Target::Game(page.current.min(page.games.saturating_sub(1)));
    match (from, dir) {
        (Target::Game(i), Dir::Up) => Target::Game(i.saturating_sub(1)),
        (Target::Game(i), Dir::Down) => Target::Game((i + 1).min(page.games - 1)),
        (Target::Game(i), Dir::Left) => Target::Game(i),
        (Target::Game(_), Dir::Right) if page.toggle => Target::Toggle,
        (Target::Game(_), Dir::Right) => page.primary(),
        (Target::Toggle, Dir::Right) if page.play => Target::Play,
        (Target::Toggle, Dir::Left) => sidebar,
        (Target::Play, Dir::Left) if page.toggle => Target::Toggle,
        (Target::Play | Target::Import, Dir::Left) => sidebar,
        (t, _) => t,
    }
}

/// One gamepad at a poll: buttons down and buttons pressed since the last.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sample {
    pub held: u16,
    pub edges: u16,
}

/// All connected pads (XInput, then Sony HID), merged into one.
#[cfg(windows)]
pub struct Pads {
    xinput: xinput::XInput,
    hid: hid::Sony,
    connected: usize,
}

#[cfg(windows)]
impl Pads {
    pub fn start() -> Self {
        Self { xinput: xinput::XInput::start(), hid: hid::Sony::start(), connected: 0 }
    }

    /// The pads merged, and a line for the log when the count changed.
    pub fn poll(&mut self) -> (Sample, Option<String>) {
        let mut samples = self.xinput.poll();
        samples.extend(self.hid.poll());
        let note = (samples.len() != self.connected).then(|| format!("Game pads connected: {}\n", samples.len()));
        self.connected = samples.len();
        let merged = samples.iter().fold(Sample::default(), |a, s| Sample { held: a.held | s.held, edges: a.edges | s.edges });
        (merged, note)
    }
}

#[cfg(windows)]
mod xinput {
    use super::Sample;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use windows_sys::Win32::UI::Input::XboxController::{XInputGetState, XINPUT_STATE};

    pub struct XInput {
        live: Arc<[AtomicBool; 4]>,
        last: [u16; 4],
    }

    impl XInput {
        pub fn start() -> Self {
            let live: Arc<[AtomicBool; 4]> = Arc::new(Default::default());
            let scan = live.clone();
            // Asking an empty port is slow: only this thread does it.
            let _ = std::thread::Builder::new().name("launcher-xinput-scan".into()).spawn(move || loop {
                for user in 0..4u32 {
                    if !scan[user as usize].load(Ordering::Relaxed) {
                        let mut s = XINPUT_STATE::default();
                        // SAFETY: a valid XINPUT_STATE; the call is thread-safe.
                        if unsafe { XInputGetState(user, &mut s) } == 0 {
                            scan[user as usize].store(true, Ordering::Relaxed);
                        }
                    }
                }
                std::thread::sleep(std::time::Duration::from_secs(2));
            });
            Self { live, last: [0; 4] }
        }

        pub fn poll(&mut self) -> Vec<Sample> {
            let mut out = Vec::new();
            for user in 0..4usize {
                if !self.live[user].load(Ordering::Relaxed) {
                    self.last[user] = 0;
                    continue;
                }
                let mut s = XINPUT_STATE::default();
                // SAFETY: a valid XINPUT_STATE for the call.
                if unsafe { XInputGetState(user as u32, &mut s) } != 0 {
                    self.live[user].store(false, Ordering::Relaxed);
                    self.last[user] = 0;
                    continue;
                }
                let g = &s.Gamepad;
                let held = super::xinput_bits(g.wButtons, g.bLeftTrigger, g.bRightTrigger, g.sThumbLX, g.sThumbLY);
                out.push(Sample { held, edges: held & !self.last[user] });
                self.last[user] = held;
            }
            out
        }
    }
}

/// The game's Sony HID reader (`crates/roses-game/src/pad_input.rs`,
/// module `hid`), copied: the launcher depends on no game crate.
#[cfg(windows)]
#[allow(dead_code)]
mod hid {
    use super::sony::{self, Model};
    use super::Sample;
    use std::ffi::c_void;
    use std::sync::{Arc, Mutex};

    #[repr(C)]
    #[derive(Clone, Copy, Default)]
    struct Guid {
        data1: u32,
        data2: u16,
        data3: u16,
        data4: [u8; 8],
    }
    #[repr(C)]
    struct InterfaceData {
        size: u32,
        class: Guid,
        flags: u32,
        reserved: usize,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Attributes {
        size: u32,
        vendor: u16,
        product: u16,
        version: u16,
    }
    #[repr(C)]
    #[derive(Default)]
    struct Caps {
        usage: u16,
        usage_page: u16,
        input_len: u16,
        output_len: u16,
        feature_len: u16,
        reserved: [u16; 17],
        counts: [u16; 10],
    }

    type Handle = isize;
    const INVALID: Handle = -1;
    const DIGCF_PRESENT: u32 = 0x2;
    const DIGCF_DEVICEINTERFACE: u32 = 0x10;
    const GENERIC_READ: u32 = 0x8000_0000;
    const FILE_SHARE_READ: u32 = 1;
    const FILE_SHARE_WRITE: u32 = 2;
    const OPEN_EXISTING: u32 = 3;
    const HIDP_STATUS_SUCCESS: i32 = 0x0011_0000;

    #[link(name = "hid", kind = "raw-dylib")]
    unsafe extern "system" {
        fn HidD_GetHidGuid(guid: *mut Guid);
        fn HidD_GetAttributes(h: Handle, a: *mut Attributes) -> u8;
        fn HidD_GetPreparsedData(h: Handle, p: *mut isize) -> u8;
        fn HidD_FreePreparsedData(p: isize) -> u8;
        fn HidP_GetCaps(p: isize, caps: *mut Caps) -> i32;
    }
    #[link(name = "setupapi", kind = "raw-dylib")]
    unsafe extern "system" {
        fn SetupDiGetClassDevsW(class: *const Guid, enumerator: *const u16, parent: isize, flags: u32) -> isize;
        fn SetupDiEnumDeviceInterfaces(set: isize, info: *const c_void, class: *const Guid, index: u32, data: *mut InterfaceData) -> i32;
        fn SetupDiGetDeviceInterfaceDetailW(set: isize, data: *const InterfaceData, detail: *mut c_void, size: u32, required: *mut u32, info: *mut c_void) -> i32;
        fn SetupDiDestroyDeviceInfoList(set: isize) -> i32;
    }
    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn CreateFileW(name: *const u16, access: u32, share: u32, security: *const c_void, disposition: u32, flags: u32, template: Handle) -> Handle;
        fn ReadFile(h: Handle, buf: *mut c_void, len: u32, read: *mut u32, overlapped: *mut c_void) -> i32;
        fn CloseHandle(h: Handle) -> i32;
    }

    struct Slot {
        path: String,
        held: u16,
        edges: u16,
        alive: bool,
    }

    #[derive(Default)]
    pub struct Sony {
        slots: Arc<Mutex<Vec<Slot>>>,
    }

    impl Sony {
        pub fn start() -> Self {
            if std::env::var("ROSES_NO_HID_PADS").is_ok_and(|v| v == "1") {
                return Self::default();
            }
            let slots: Arc<Mutex<Vec<Slot>>> = Arc::default();
            let scan = slots.clone();
            let _ = std::thread::Builder::new().name("launcher-hid-pads".into()).spawn(move || loop {
                scan_once(&scan);
                std::thread::sleep(std::time::Duration::from_secs(2));
            });
            Self { slots }
        }

        pub fn poll(&mut self) -> Vec<Sample> {
            let mut slots = self.slots.lock().unwrap_or_else(|e| e.into_inner());
            slots.retain(|s| s.alive);
            slots
                .iter_mut()
                .map(|s| {
                    let out = Sample { held: s.held, edges: s.edges };
                    s.edges = 0;
                    out
                })
                .collect()
        }
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    fn hid_paths() -> Vec<String> {
        let mut guid = Guid::default();
        let mut out = Vec::new();
        // SAFETY: documented SetupDi calls with valid buffers; the detail
        // buffer is u32-aligned and as large as the call asked for.
        unsafe {
            HidD_GetHidGuid(&mut guid);
            let set = SetupDiGetClassDevsW(&guid, std::ptr::null(), 0, DIGCF_PRESENT | DIGCF_DEVICEINTERFACE);
            if set == INVALID {
                return out;
            }
            for index in 0.. {
                let mut data = InterfaceData { size: size_of::<InterfaceData>() as u32, class: Guid::default(), flags: 0, reserved: 0 };
                if SetupDiEnumDeviceInterfaces(set, std::ptr::null(), &guid, index, &mut data) == 0 {
                    break;
                }
                let mut required = 0u32;
                SetupDiGetDeviceInterfaceDetailW(set, &data, std::ptr::null_mut(), 0, &mut required, std::ptr::null_mut());
                if required < 8 {
                    continue;
                }
                let mut buf = vec![0u32; (required as usize).div_ceil(4)];
                buf[0] = if size_of::<usize>() == 8 { 8 } else { 6 };
                if SetupDiGetDeviceInterfaceDetailW(set, &data, buf.as_mut_ptr().cast(), required, std::ptr::null_mut(), std::ptr::null_mut()) == 0 {
                    continue;
                }
                let chars = std::slice::from_raw_parts(buf.as_ptr().cast::<u16>().add(2), (required as usize - 4) / 2);
                let len = chars.iter().position(|&c| c == 0).unwrap_or(chars.len());
                out.push(String::from_utf16_lossy(&chars[..len]));
            }
            SetupDiDestroyDeviceInfoList(set);
        }
        out
    }

    fn scan_once(slots: &Arc<Mutex<Vec<Slot>>>) {
        for path in hid_paths() {
            let key = path.to_ascii_lowercase();
            if slots.lock().unwrap_or_else(|e| e.into_inner()).iter().any(|s| s.alive && s.path == key) {
                continue;
            }
            // SAFETY: a NUL-terminated path; the handle is closed below.
            let probe = unsafe { CreateFileW(wide(&path).as_ptr(), 0, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, 0) };
            if probe == INVALID {
                continue;
            }
            let mut attr = Attributes { size: size_of::<Attributes>() as u32, ..Default::default() };
            // SAFETY: valid handle and out-pointer.
            let model = if unsafe { HidD_GetAttributes(probe, &mut attr) } != 0 { sony::model(attr.vendor, attr.product) } else { None };
            let caps = model.and_then(|_| caps(probe)).filter(|c| c.usage_page == 1 && (c.usage == 5 || c.usage == 4));
            // SAFETY: the probe handle is ours.
            unsafe { CloseHandle(probe) };
            let (Some(model), Some(caps)) = (model, caps) else {
                continue;
            };
            // SAFETY: as above, with read access for ReadFile.
            let h = unsafe { CreateFileW(wide(&path).as_ptr(), GENERIC_READ, FILE_SHARE_READ | FILE_SHARE_WRITE, std::ptr::null(), OPEN_EXISTING, 0, 0) };
            if h == INVALID {
                continue;
            }
            let bluetooth = sony::bluetooth(&path, usize::from(caps.input_len));
            slots.lock().unwrap_or_else(|e| e.into_inner()).push(Slot { path: key.clone(), held: 0, edges: 0, alive: true });
            let shared = slots.clone();
            let len = usize::from(caps.input_len).max(64);
            let spawned = std::thread::Builder::new().name("launcher-hid-pad".into()).spawn(move || read_loop(h, model, bluetooth, len, &key, &shared));
            if spawned.is_err() {
                // SAFETY: the thread did not take the handle.
                unsafe { CloseHandle(h) };
            }
        }
    }

    fn caps(h: Handle) -> Option<Caps> {
        let mut pre = 0isize;
        // SAFETY: valid handle; the preparsed data is freed after use.
        unsafe {
            if HidD_GetPreparsedData(h, &mut pre) == 0 {
                return None;
            }
            let mut caps = Caps::default();
            let ok = HidP_GetCaps(pre, &mut caps) == HIDP_STATUS_SUCCESS;
            HidD_FreePreparsedData(pre);
            ok.then_some(caps)
        }
    }

    fn read_loop(h: Handle, model: Model, bluetooth: bool, len: usize, key: &str, slots: &Mutex<Vec<Slot>>) {
        let mut buf = vec![0u8; len];
        loop {
            let mut read = 0u32;
            // SAFETY: a synchronous handle and a buffer of `len` bytes.
            if unsafe { ReadFile(h, buf.as_mut_ptr().cast(), len as u32, &mut read, std::ptr::null_mut()) } == 0 {
                break;
            }
            let Some(bits) = sony::parse(model, bluetooth, &buf[..(read as usize).min(len)]) else {
                continue;
            };
            if let Some(s) = slots.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|s| s.alive && s.path == key) {
                s.edges |= bits & !s.held;
                s.held = bits;
            }
        }
        if let Some(s) = slots.lock().unwrap_or_else(|e| e.into_inner()).iter_mut().find(|s| s.alive && s.path == key) {
            s.alive = false;
        }
        // SAFETY: the handle is ours.
        unsafe { CloseHandle(h) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(games: usize, current: usize, imported: bool, toggle: bool) -> Page {
        Page { games, current, import: !imported, toggle: imported && toggle, play: imported }
    }

    #[test]
    fn the_focus_moves_between_the_sidebar_toggle_and_play() {
        let p = page(2, 0, true, true);
        // Nothing focused (or Saves, Settings, ...): the main button first.
        assert_eq!(step(None, Dir::Down, &p), Target::Play);
        // Play -> toggle -> the shown game's sidebar entry, and back.
        assert_eq!(step(Some(Target::Play), Dir::Left, &p), Target::Toggle);
        assert_eq!(step(Some(Target::Toggle), Dir::Left, &p), Target::Game(0));
        assert_eq!(step(Some(Target::Game(0)), Dir::Right, &p), Target::Toggle);
        assert_eq!(step(Some(Target::Toggle), Dir::Right, &p), Target::Play);
        // Up / down walk the games and stop at the ends.
        assert_eq!(step(Some(Target::Game(0)), Dir::Down, &p), Target::Game(1));
        assert_eq!(step(Some(Target::Game(1)), Dir::Down, &p), Target::Game(1));
        assert_eq!(step(Some(Target::Game(1)), Dir::Up, &p), Target::Game(0));
        assert_eq!(step(Some(Target::Game(0)), Dir::Up, &p), Target::Game(0));
        assert_eq!(step(Some(Target::Game(0)), Dir::Left, &p), Target::Game(0));
        // Play is the right-most: right, up, down stay.
        for d in [Dir::Right, Dir::Up, Dir::Down] {
            assert_eq!(step(Some(Target::Play), d, &p), Target::Play);
        }
        // Left from the main area goes to the game shown, not the first.
        let second = page(3, 2, true, true);
        assert_eq!(step(Some(Target::Toggle), Dir::Left, &second), Target::Game(2));
    }

    #[test]
    fn the_focus_skips_what_is_not_shown() {
        // Not imported: Import instead of Play, no toggle.
        let p = page(2, 1, false, true);
        assert_eq!(step(None, Dir::Up, &p), Target::Import);
        assert_eq!(step(Some(Target::Game(1)), Dir::Right, &p), Target::Import);
        assert_eq!(step(Some(Target::Import), Dir::Left, &p), Target::Game(1));
        // Play (no longer shown) counts as nothing focused.
        assert_eq!(step(Some(Target::Play), Dir::Left, &p), Target::Import);
        assert_eq!(step(Some(Target::Toggle), Dir::Right, &p), Target::Import);
        // A game without a launch toggle (FM): Play <-> sidebar.
        let fm = page(2, 1, true, false);
        assert_eq!(step(Some(Target::Play), Dir::Left, &fm), Target::Game(1));
        assert_eq!(step(Some(Target::Game(0)), Dir::Right, &fm), Target::Play);
        // One game only.
        let one = page(1, 0, true, true);
        assert_eq!(step(Some(Target::Game(0)), Dir::Down, &one), Target::Game(0));
    }

    #[test]
    fn buttons_map_by_position_on_both_pads() {
        // XInput: A Cross, B Circle, LB/RB L1/R1, Start, d-pad, stick.
        assert_eq!(xinput_bits(0x1000 | 0x0004, 0, 0, 0, 0), bit::CROSS | bit::LEFT);
        assert_eq!(xinput_bits(0x2000 | 0x0100 | 0x0200 | 0x0010, 0, 0, 0, 0), bit::CIRCLE | bit::L1 | bit::R1 | bit::START);
        assert_eq!(xinput_bits(0, 31, 30, 0, 0), bit::L2);
        assert_eq!(xinput_bits(0, 0, 0, 0, i16::MIN), bit::DOWN);
        assert_eq!(xinput_bits(0, 0, 0, i16::MAX, 20_000), bit::RIGHT | bit::UP);
        assert_eq!(xinput_bits(0, 0, 0, 7_849, -7_849), 0, "resting drift");
        // DualSense over USB: Cross, hat W, R1, left stick down.
        let mut usb = [0u8; 64];
        usb[0] = 0x01;
        usb[1..5].copy_from_slice(&[0x80, 0xF0, 0x80, 0x80]);
        usb[8] = 0x20 | 6;
        usb[9] = 0x02;
        assert_eq!(sony::parse(sony::Model::DualSense, false, &usb), Some(bit::CROSS | bit::LEFT | bit::R1 | bit::DOWN));
        // DualShock 4 over USB: Circle, L1, Options, hat none.
        let mut ds4 = [0u8; 64];
        ds4[0] = 0x01;
        ds4[1..5].copy_from_slice(&[0x80; 4]);
        ds4[5] = 0x40 | 8;
        ds4[6] = 0x01 | 0x20;
        assert_eq!(sony::parse(sony::Model::DualShock4, false, &ds4), Some(bit::CIRCLE | bit::L1 | bit::START));
        assert_eq!(sony::model(0x054C, 0x0CE6), Some(sony::Model::DualSense));
        assert_eq!(sony::model(0x045E, 0x0CE6), None);
    }

    #[test]
    fn presses_become_actions_and_a_held_direction_repeats() {
        let mut i = Input::default();
        assert_eq!(i.update(bit::DOWN, bit::DOWN, 0), [Action::Move(Dir::Down)]);
        // Held: nothing until the repeat, then every REPEAT_EVERY_MS.
        assert!(i.update(bit::DOWN, 0, 100).is_empty());
        assert!(i.update(bit::DOWN, 0, REPEAT_AFTER_MS - 1).is_empty());
        assert_eq!(i.update(bit::DOWN, 0, REPEAT_AFTER_MS), [Action::Move(Dir::Down)]);
        assert!(i.update(bit::DOWN, 0, REPEAT_AFTER_MS + 100).is_empty());
        assert_eq!(i.update(bit::DOWN, 0, REPEAT_AFTER_MS + REPEAT_EVERY_MS), [Action::Move(Dir::Down)]);
        assert!(i.update(0, 0, 2000).is_empty());
        assert!(i.update(0, 0, 5000).is_empty(), "released: no repeat");
        // Buttons act once per press; a tap between polls still counts.
        assert_eq!(i.update(bit::CROSS, bit::CROSS, 6000), [Action::Activate]);
        assert!(i.update(bit::CROSS, 0, 7000).is_empty());
        assert_eq!(i.update(0, bit::CIRCLE, 7100), [Action::Back]);
        assert_eq!(i.update(bit::L1, bit::L1, 7200), [Action::Game(-1)]);
        assert_eq!(i.update(bit::R1, bit::R1, 7300), [Action::Game(1)]);
        assert_eq!(i.update(bit::START, bit::START, 7400), [Action::Activate]);
        // Triangle, Square, Select and the triggers do nothing here.
        assert!(i.update(bit::TRIANGLE | bit::SQUARE | bit::SELECT | bit::L2 | bit::R2, 0, 7500).is_empty());
        i.reset();
        assert_eq!(i.update(bit::RIGHT, 0, 8000), [Action::Move(Dir::Right)]);
    }
}
