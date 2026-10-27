//! The Windows window: one machine per window, its picture drawn with GDI,
//! keyboard and pointer forwarded, the clipboard shared, and reconnects
//! handled the way the Linux client handles them.
//!
//! Everything here runs on the UI thread. The decode worker (`net`) runs on
//! its own thread as on Linux; two small relay threads hand its frames and
//! statuses over by posting window messages, so the UI never polls.

use std::cell::{Cell, OnceCell, RefCell};
use std::collections::{BTreeSet, VecDeque};
use std::path::PathBuf;
use std::process::ExitCode;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use clap::Parser;
use gliff_proto::{Axis, ClientMsg};
use gliff_transport::clipboard::progress::{describe, State};
use gliff_transport::SshTarget;
use tokio::sync::mpsc::{unbounded_channel, UnboundedSender};
use tokio::sync::watch;
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, RECT, WPARAM};
use windows_sys::Win32::Graphics::Dwm::{DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE};
use windows_sys::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint, FillRect, GetDC,
    GetMonitorInfoW, InvalidateRect, MonitorFromWindow, ReleaseDC, SelectObject, SetBkMode,
    SetTextColor, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_CHARSET, DEFAULT_PITCH,
    DT_CALCRECT, DT_CENTER, DT_NOPREFIX, DT_WORDBREAK, FW_NORMAL, HDC, HFONT, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Threading::GetCurrentProcess;
use windows_sys::Win32::UI::HiDpi::{
    GetDpiForSystem, GetDpiForWindow, SetProcessDpiAwarenessContext,
    DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{
    GetKeyboardLayoutNameW, ReleaseCapture, SetCapture, TrackMouseEvent, TME_LEAVE,
    TRACKMOUSEEVENT, VK_F11,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CheckMenuItem, CreateWindowExW, DefWindowProcW, DestroyCursor, DispatchMessageW,
    GetClientRect, GetMessageW, GetSystemMenu, GetWindowLongPtrW, GetWindowPlacement, IsIconic,
    KillTimer, LoadCursorW, PostMessageW, PostQuitMessage, RegisterClassExW, SetCursor, SetTimer,
    SetWindowLongPtrW, SetWindowPlacement, SetWindowPos, ShowWindow, TranslateMessage,
    CW_USEDEFAULT, GWL_STYLE, HCURSOR, HTCLIENT, HWND_TOP, IDC_ARROW, MF_BYCOMMAND, MF_CHECKED,
    MF_SEPARATOR, MF_STRING, MF_UNCHECKED, MINMAXINFO, MSG, SIZE_MINIMIZED, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SW_SHOWDEFAULT,
    WA_INACTIVE, WINDOWPLACEMENT, WM_ACTIVATE, WM_APP, WM_CAPTURECHANGED, WM_CLIPBOARDUPDATE,
    WM_DESTROY, WM_DESTROYCLIPBOARD, WM_DPICHANGED, WM_ERASEBKGND, WM_GETMINMAXINFO,
    WM_INPUTLANGCHANGE, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_PAINT, WM_RBUTTONDOWN, WM_RBUTTONUP,
    WM_RENDERALLFORMATS, WM_RENDERFORMAT, WM_SETCURSOR, WM_SIZE, WM_SYSCOMMAND, WM_TIMER,
    WM_XBUTTONDOWN, WM_XBUTTONUP, WNDCLASSEXW, WS_OVERLAPPEDWINDOW, XBUTTON1,
};

use super::clipboard::Clipboard;
use super::fit::{fit, Fit};
use super::gdi::{self, wide, Pixels};
use super::gpu::{DisplayFrame, Gpu};
use super::hook::{self, WM_APP_KEY};
use super::keys::{self, Hotkey};
use super::layout;
use super::present::{label_width, place, Anchor, Presenter, Source};
use crate::clipboard::ToWorker;
use crate::cursor::has_visible_shape;
use crate::net::{Endpoint, Frame, Picture, Status, Worker};
use crate::recent::Config;

const APP_NAME: &str = "gliff";

#[derive(Parser, Clone)]
#[command(
    name = APP_NAME,
    version,
    about = "Remote-desktop a Hyprland session over ssh"
)]
struct Cli {
    /// `user@host` to ssh to and spawn gliff-server. Without it, gliff
    /// connects to the machine it showed last.
    host: Option<String>,
    /// Disable clipboard sharing in both directions.
    #[arg(long)]
    no_clipboard: bool,
    /// Dev: connect directly to a `gliff-server --listen` address.
    #[arg(long, conflicts_with = "host")]
    connect: Option<String>,
    /// Remote gliff-server path.
    #[arg(long, default_value = "gliff-server")]
    server_bin: String,
    /// Create a private headless output on the remote, sized and scaled to
    /// this window, instead of mirroring the remote's focused screen.
    #[arg(long, conflicts_with = "output")]
    headless: bool,
    /// Mirror the named remote output (e.g. `DP-1`) instead of the focused one.
    #[arg(long)]
    output: Option<String>,
    /// Hotkey that releases captured shortcuts and hands the keyboard back to
    /// Windows. Forms: a chord like `shift+escape` or `ctrl+alt+q`;
    /// `double-<key>` for a double-tap (e.g. `double-escape`); or `none` to
    /// disable. The screen recaptures when you click it again.
    #[arg(long, default_value = "shift+escape")]
    release_hotkey: String,
    /// Start in full screen. F11 toggles it while the keyboard is not
    /// captured, as does the window menu.
    #[arg(long)]
    fullscreen: bool,
    /// `cpu` decodes with OpenH264 and draws with GDI instead of Media
    /// Foundation and Direct3D 11.
    #[arg(long, value_parser = ["gpu", "cpu"], hide = true)]
    video: Option<String>,
}

/// In `Win32_UI_Controls`, which is large to pull in for one constant.
const WM_MOUSELEAVE: u32 = 0x02A3;

/// Messages the relay threads post. `wParam` is the worker's generation.
const WM_APP_FRAME: u32 = WM_APP + 2;
const WM_APP_STATUS: u32 = WM_APP + 3;

const TIMER_RESIZE: usize = 1;
const TIMER_HINT: usize = 2;
const TIMER_RECONNECT: usize = 3;

/// Window menu entries; system command ids must keep their low four bits
/// clear.
const IDM_FULLSCREEN: usize = 0x10;
const IDM_STATS: usize = 0x20;
const IDM_RECONNECT: usize = 0x30;

const MAX_RETRIES: u32 = 5;

/// What one worker's relay threads hand to the UI thread.
#[derive(Default)]
struct Inbox {
    /// Latest-wins: a newer frame replaces one the UI has not taken yet.
    frame: Mutex<Option<Picture>>,
    /// A WM_APP_FRAME is on its way, so another need not be posted.
    frame_posted: AtomicBool,
    status: Mutex<VecDeque<Status>>,
}

/// The picture on screen, kept for repaints.
enum Shown {
    /// From the CPU decoder.
    Cpu(gliff_sw::PooledFrame),
    /// From the hardware decoder; holding it keeps its texture.
    Gpu(DisplayFrame),
}

#[derive(Clone, Copy, PartialEq)]
enum Pointer {
    Default,
    /// The frames carry the remote cursor already.
    Hidden,
    Remote(HCURSOR),
}

struct App {
    hwnd: HWND,
    cli: Cli,
    endpoint: Endpoint,
    name: String,
    hotkey: Hotkey,
    config_path: PathBuf,
    keymap: watch::Sender<String>,
    clipboard: Option<Clipboard>,

    // The connection.
    input_tx: RefCell<Option<UnboundedSender<ToWorker>>>,
    /// Bumped by every `start_session`, so messages from an older worker
    /// are dropped.
    generation: Cell<u64>,
    inbox: RefCell<Arc<Inbox>>,
    retries: Cell<u32>,
    gave_up: Cell<bool>,
    remembered: Cell<bool>,
    connected: Cell<bool>,
    /// The local selection has been offered to this worker.
    offered: Cell<bool>,
    headless: Cell<bool>,
    /// Geometry of the frame on screen, which pointer mapping follows.
    stream_size: Cell<(u32, u32)>,
    stream_view: Cell<(u32, u32)>,
    stream_scale: Cell<f64>,
    /// The view from the last StreamConfig; only the resize gate reads it.
    server_view: Cell<(u32, u32)>,
    fps_cap: Cell<u32>,
    resize_requested: Cell<(u32, u32)>,
    resize_scale: Cell<f64>,
    frame: RefCell<Option<Shown>>,
    /// Draws through Direct3D 11 when the device could be made; GDI
    /// otherwise.
    presenter: RefCell<Option<Presenter>>,
    /// A Direct3D draw has failed and been logged.
    present_failed: Cell<bool>,
    pointer: Cell<Pointer>,
    status: RefCell<String>,
    stats: RefCell<String>,
    show_stats: Cell<bool>,
    hint: RefCell<String>,
    capture_hinted: Cell<bool>,

    // The window and input.
    /// Client area in device pixels.
    client: Cell<(u32, u32)>,
    /// DPI / 96.
    scale: Cell<f64>,
    font: Cell<HFONT>,
    active: Cell<bool>,
    inside: Cell<bool>,
    /// Set by the release hotkey: keys stay with Windows until the pointer
    /// leaves the window or the picture is clicked.
    released: Cell<bool>,
    capturing: Cell<bool>,
    /// Evdev codes held down on the remote.
    pressed_keys: RefCell<BTreeSet<u32>>,
    /// Modifier virtual keys held down, for the release hotkey.
    held_mods: RefCell<BTreeSet<u32>>,
    last_tap: RefCell<Option<Instant>>,
    buttons: Cell<u32>,
    /// Wheel movement not yet a whole notch, vertical and horizontal.
    wheel: Cell<(i32, i32)>,
    /// The style and placement to restore when leaving full screen.
    windowed: RefCell<Option<(isize, WINDOWPLACEMENT)>>,
}

thread_local! {
    static APP: OnceCell<Rc<App>> = const { OnceCell::new() };
}

pub fn main() -> ExitCode {
    let cli = Cli::parse();
    // SAFETY: called before any window exists; failure leaves the default.
    unsafe { SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    end_children_with_this_process();
    let hotkey = match Hotkey::parse(&cli.release_hotkey) {
        Ok(h) => h,
        Err(e) => {
            eprintln!("gliff: --release-hotkey: {e}");
            return ExitCode::from(2);
        }
    };
    let config_path = Config::default_path();
    let Some(endpoint) = endpoint_from_cli(&cli, &config_path) else {
        eprintln!("gliff: name the machine to connect to, e.g. `gliff user@host`");
        return ExitCode::from(2);
    };
    let name = match &endpoint {
        Endpoint::Ssh(t) => t.host.clone(),
        Endpoint::Tcp(addr) => addr.clone(),
    };
    let hwnd = match create_window(&format!("{name} — Gliff")) {
        Ok(hwnd) => hwnd,
        Err(e) => {
            eprintln!("gliff: cannot open a window: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = hook::start(hwnd) {
        tracing::warn!(error = %e, "cannot capture system shortcuts; Win and Alt+Tab stay local");
    }
    let clipboard = (!cli.no_clipboard).then(|| Clipboard::new(hwnd));
    let presenter = open_presenter(hwnd, cli.video.as_deref());
    let (keymap, _) = watch::channel(current_keymap());
    // SAFETY: `hwnd` is the live window just created.
    let dpi = unsafe { GetDpiForWindow(hwnd) }.max(96);
    let app = Rc::new(App {
        hwnd,
        endpoint,
        name,
        hotkey,
        config_path,
        keymap,
        clipboard,
        input_tx: RefCell::new(None),
        generation: Cell::new(0),
        inbox: RefCell::new(Arc::default()),
        retries: Cell::new(0),
        gave_up: Cell::new(false),
        remembered: Cell::new(false),
        connected: Cell::new(false),
        offered: Cell::new(false),
        headless: Cell::new(false),
        stream_size: Cell::new((0, 0)),
        stream_view: Cell::new((0, 0)),
        stream_scale: Cell::new(1.0),
        server_view: Cell::new((0, 0)),
        fps_cap: Cell::new(0),
        resize_requested: Cell::new((0, 0)),
        resize_scale: Cell::new(0.0),
        frame: RefCell::new(None),
        presenter: RefCell::new(presenter),
        present_failed: Cell::new(false),
        pointer: Cell::new(Pointer::Default),
        status: RefCell::new(String::new()),
        stats: RefCell::new(String::new()),
        show_stats: Cell::new(false),
        hint: RefCell::new(String::new()),
        capture_hinted: Cell::new(false),
        client: Cell::new((0, 0)),
        scale: Cell::new(f64::from(dpi) / 96.0),
        font: Cell::new(create_font(dpi)),
        active: Cell::new(false),
        inside: Cell::new(false),
        released: Cell::new(false),
        capturing: Cell::new(false),
        pressed_keys: RefCell::new(BTreeSet::new()),
        held_mods: RefCell::new(BTreeSet::new()),
        last_tap: RefCell::new(None),
        buttons: Cell::new(0),
        wheel: Cell::new((0, 0)),
        windowed: RefCell::new(None),
        cli,
    });
    APP.with(|a| {
        let _ = a.set(app.clone());
    });
    app.read_client_size();
    app.add_menu_items();
    // SAFETY: `hwnd` is our window; the attribute is a BOOL.
    unsafe {
        let dark: i32 = 1;
        DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE as u32,
            (&dark as *const i32).cast(),
            4,
        );
        ShowWindow(hwnd, SW_SHOWDEFAULT);
    }
    if app.cli.fullscreen {
        app.toggle_fullscreen();
    }
    app.start_session();

    // SAFETY: MSG is plain data; zeroed is a valid empty message.
    let mut msg: MSG = unsafe { std::mem::zeroed() };
    // SAFETY: the standard message loop on the window's thread.
    while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {
        // SAFETY: `msg` was just filled by GetMessageW.
        unsafe {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    ExitCode::SUCCESS
}

/// The machine named on the command line, else the one shown last.
fn endpoint_from_cli(cli: &Cli, config_path: &std::path::Path) -> Option<Endpoint> {
    if let Some(addr) = &cli.connect {
        return Some(Endpoint::Tcp(addr.clone()));
    }
    let host = cli.host.clone().or_else(|| {
        let config = Config::load(config_path);
        config.shown.or_else(|| config.machines.last().cloned())
    })?;
    let mut t = SshTarget::new(host);
    t.server_bin = cli.server_bin.clone();
    t.server_args = match (&cli.output, cli.headless) {
        (Some(name), _) => vec!["--output".into(), name.clone()],
        (None, true) => vec!["--headless".into()],
        (None, false) => vec!["--output".into(), "auto".into()],
    };
    Some(Endpoint::Ssh(t))
}

/// The Direct3D 11 presenter, and with it the device the decode worker uses,
/// unless `--video cpu` asked for the CPU and GDI path or the device or swap
/// chain cannot be made.
fn open_presenter(hwnd: HWND, video: Option<&str>) -> Option<Presenter> {
    // The flag, else GLIFF_VIDEO, as the decode worker reads them.
    if gliff_sw::VideoMode::resolve(video) == gliff_sw::VideoMode::Cpu {
        return None;
    }
    let mut r = RECT {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    // SAFETY: `r` is a valid out pointer for our window's client area.
    unsafe { GetClientRect(hwnd, &mut r) };
    let size = (
        (r.right - r.left).max(1) as u32,
        (r.bottom - r.top).max(1) as u32,
    );
    match Gpu::create().and_then(|gpu| Ok((Presenter::new(hwnd, gpu.clone(), size)?, gpu))) {
        Ok((presenter, gpu)) => {
            tracing::info!(gpu = %gpu.name, "decoding and drawing on the GPU");
            Gpu::share(&gpu);
            Some(presenter)
        }
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "no Direct3D 11; decoding on the CPU and drawing with GDI");
            None
        }
    }
}

/// Put this process in a job that ends with it, so an ssh child cannot
/// outlive gliff however it exits (Windows does not end children with
/// their parent).
fn end_children_with_this_process() {
    // SAFETY: plain job-object calls on a fresh job; the handle is leaked
    // on purpose, since closing it (at exit) is what ends the children.
    unsafe {
        let job = CreateJobObjectW(std::ptr::null(), std::ptr::null());
        if job.is_null() {
            return;
        }
        let mut info: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = std::mem::zeroed();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&info as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        );
        if set == 0 || AssignProcessToJobObject(job, GetCurrentProcess()) == 0 {
            tracing::debug!(error = %std::io::Error::last_os_error(), "ssh will not be tied to this process");
        }
    }
}

fn create_window(title: &str) -> std::io::Result<HWND> {
    let class = wide("gliff");
    let title = wide(title);
    // SAFETY: registers a class whose procedure lives for the program and
    // creates a top-level window of it; all strings are NUL-terminated and
    // outlive the calls.
    unsafe {
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: 0,
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: std::ptr::null_mut(),
            hCursor: std::ptr::null_mut(),
            hbrBackground: std::ptr::null_mut(),
            lpszMenuName: std::ptr::null(),
            lpszClassName: class.as_ptr(),
            hIconSm: std::ptr::null_mut(),
        };
        if RegisterClassExW(&wc) == 0 {
            return Err(std::io::Error::last_os_error());
        }
        let dpi = GetDpiForSystem().max(96) as i32;
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            title.as_ptr(),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280 * dpi / 96,
            760 * dpi / 96,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            return Err(std::io::Error::last_os_error());
        }
        Ok(hwnd)
    }
}

fn create_font(dpi: u32) -> HFONT {
    let face = wide("Segoe UI");
    // SAFETY: a NUL-terminated face name; the font is deleted when replaced.
    unsafe {
        CreateFontW(
            -(15 * dpi as i32 / 96),
            0,
            0,
            0,
            FW_NORMAL as i32,
            0,
            0,
            0,
            u32::from(DEFAULT_CHARSET),
            u32::from(OUT_DEFAULT_PRECIS),
            u32::from(CLIP_DEFAULT_PRECIS),
            u32::from(CLEARTYPE_QUALITY),
            DEFAULT_PITCH as u32,
            face.as_ptr(),
        )
    }
}

/// The XKB keymap matching the active Windows layout.
fn current_keymap() -> String {
    let mut buf = [0u16; 9];
    // SAFETY: the buffer holds KL_NAMELENGTH (9) characters.
    let ok = unsafe { GetKeyboardLayoutNameW(buf.as_mut_ptr()) } != 0;
    let klid = if ok {
        String::from_utf16_lossy(&buf[..8])
    } else {
        String::new()
    };
    let l = layout::xkb_layout(&klid);
    tracing::info!(
        klid,
        layout = l.layout,
        variant = l.variant,
        "keyboard layout"
    );
    layout::keymap_text(l)
}

extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    // The window exists before the app does (creation sends messages), so
    // early messages get the default handling.
    let app = APP.with(|a| a.get().cloned());
    if let Some(app) = app {
        if let Some(result) = app.handle(msg, wparam, lparam) {
            return result;
        }
    }
    // SAFETY: the default procedure for our own window's message.
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn loword(v: usize) -> u32 {
    (v & 0xFFFF) as u32
}

fn hiword(v: usize) -> u32 {
    ((v >> 16) & 0xFFFF) as u32
}

/// Signed client coordinates from a mouse message's lParam.
fn point(lparam: LPARAM) -> (i32, i32) {
    let v = lparam as usize;
    (
        i32::from((v & 0xFFFF) as u16 as i16),
        i32::from(((v >> 16) & 0xFFFF) as u16 as i16),
    )
}

impl App {
    /// Handle a message, or `None` for the default handling.
    fn handle(self: &Rc<Self>, msg: u32, wparam: WPARAM, lparam: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                // SAFETY: PAINTSTRUCT is plain data, filled by BeginPaint
                // for our window and returned to EndPaint.
                unsafe {
                    let mut ps: PAINTSTRUCT = std::mem::zeroed();
                    let hdc = BeginPaint(self.hwnd, &mut ps);
                    if !self.present() {
                        self.paint(hdc);
                    }
                    EndPaint(self.hwnd, &ps);
                }
                Some(0)
            }
            // Every pixel is painted, so erasing first would only flicker.
            WM_ERASEBKGND => Some(1),
            WM_SIZE => {
                if wparam as u32 != SIZE_MINIMIZED {
                    let size = (loword(lparam as usize), hiword(lparam as usize));
                    self.client.set(size);
                    if let Some(p) = self.presenter.borrow_mut().as_mut() {
                        if let Err(e) = p.resize(size) {
                            tracing::warn!(error = %format!("{e:#}"), "cannot resize the swap chain");
                        }
                    }
                    self.size_changed();
                }
                Some(0)
            }
            WM_DPICHANGED => {
                let dpi = loword(wparam).max(96);
                self.scale.set(f64::from(dpi) / 96.0);
                let old = self.font.replace(create_font(dpi));
                // SAFETY: lParam points to the suggested window rectangle;
                // the old font is ours and no longer selected.
                unsafe {
                    DeleteObject(old);
                    let r = &*(lparam as *const RECT);
                    SetWindowPos(
                        self.hwnd,
                        std::ptr::null_mut(),
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOZORDER | SWP_NOACTIVATE,
                    );
                }
                self.size_changed();
                Some(0)
            }
            WM_GETMINMAXINFO => {
                // SAFETY: lParam points to the MINMAXINFO to fill in.
                let info = unsafe { &mut *(lparam as *mut MINMAXINFO) };
                info.ptMinTrackSize = POINT { x: 480, y: 320 };
                Some(0)
            }
            WM_SETCURSOR if loword(lparam as usize) == HTCLIENT => {
                self.apply_pointer();
                Some(1)
            }
            WM_ACTIVATE => {
                self.active.set(loword(wparam) != WA_INACTIVE);
                self.update_capture();
                None
            }
            WM_MOUSEMOVE => {
                if !self.inside.replace(true) {
                    let mut track = TRACKMOUSEEVENT {
                        cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: self.hwnd,
                        dwHoverTime: 0,
                    };
                    // SAFETY: a filled-in request for our window.
                    unsafe { TrackMouseEvent(&mut track) };
                    self.update_capture();
                }
                let (x, y) = self.to_remote(point(lparam));
                self.send(ClientMsg::PointerMotion { x, y });
                Some(0)
            }
            WM_MOUSELEAVE => {
                self.inside.set(false);
                // A deliberate release lasts while the pointer rests on the
                // picture; leaving and coming back captures again.
                self.released.set(false);
                self.update_capture();
                Some(0)
            }
            WM_LBUTTONDOWN | WM_RBUTTONDOWN | WM_MBUTTONDOWN | WM_XBUTTONDOWN => {
                self.button(msg, wparam, true);
                Some(if msg == WM_XBUTTONDOWN { 1 } else { 0 })
            }
            WM_LBUTTONUP | WM_RBUTTONUP | WM_MBUTTONUP | WM_XBUTTONUP => {
                self.button(msg, wparam, false);
                Some(if msg == WM_XBUTTONUP { 1 } else { 0 })
            }
            WM_CAPTURECHANGED if lparam as HWND != self.hwnd => {
                self.release_buttons();
                Some(0)
            }
            WM_MOUSEWHEEL => {
                self.wheel(Axis::Vertical, hiword(wparam) as u16 as i16);
                Some(0)
            }
            WM_MOUSEHWHEEL => {
                self.wheel(Axis::Horizontal, hiword(wparam) as u16 as i16);
                Some(0)
            }
            WM_APP_KEY => {
                self.key(wparam as u32, lparam);
                Some(0)
            }
            // Keys reach the window only while they are not captured.
            WM_KEYDOWN if wparam == usize::from(VK_F11) => {
                self.toggle_fullscreen();
                Some(0)
            }
            WM_APP_FRAME => {
                self.take_frame(wparam as u64);
                Some(0)
            }
            WM_APP_STATUS => {
                self.take_statuses(wparam as u64);
                Some(0)
            }
            WM_TIMER => {
                self.timer(wparam);
                Some(0)
            }
            WM_SYSCOMMAND => match wparam & 0xFFF0 {
                IDM_FULLSCREEN => {
                    self.toggle_fullscreen();
                    Some(0)
                }
                IDM_STATS => {
                    self.show_stats.set(!self.show_stats.get());
                    self.check_menu_item(IDM_STATS, self.show_stats.get());
                    self.invalidate();
                    Some(0)
                }
                IDM_RECONNECT => {
                    self.retries.set(0);
                    self.start_session();
                    Some(0)
                }
                _ => None,
            },
            WM_INPUTLANGCHANGE => {
                self.keymap.send_replace(current_keymap());
                None
            }
            WM_CLIPBOARDUPDATE => {
                if let (Some(cb), Some(tx)) = (&self.clipboard, self.input_tx.borrow().as_ref()) {
                    cb.offer_to(tx);
                }
                Some(0)
            }
            WM_RENDERFORMAT => {
                if let Some(cb) = &self.clipboard {
                    cb.render(wparam as u32);
                }
                Some(0)
            }
            WM_RENDERALLFORMATS => {
                if let Some(cb) = &self.clipboard {
                    cb.render_all();
                }
                Some(0)
            }
            WM_DESTROYCLIPBOARD => {
                if let Some(cb) = &self.clipboard {
                    cb.released();
                }
                Some(0)
            }
            WM_DESTROY => {
                hook::set_capture(false);
                // Closing the input ends the worker and its ssh child.
                self.input_tx.borrow_mut().take();
                // SAFETY: ends this thread's message loop.
                unsafe { PostQuitMessage(0) };
                Some(0)
            }
            _ => None,
        }
    }

    // ---- the connection ----

    /// Start a worker. Each call is a new generation; messages and timers
    /// of an older one are ignored.
    fn start_session(self: &Rc<Self>) {
        // SAFETY: our window's timer; killing one that is not set is fine.
        unsafe { KillTimer(self.hwnd, TIMER_RECONNECT) };
        // A reconnect from the menu replaces a live session: release what
        // it holds and drop its picture before the new one starts.
        if self.connected.get() {
            self.show_disconnected();
        }
        if let Endpoint::Ssh(_) = self.endpoint {
            if !ssh_installed() {
                self.set_status(
                    "ssh was not found.\nInstall “OpenSSH Client” under Settings › System › Optional features.",
                );
                self.gave_up.set(true);
                return;
            }
        }
        let (frame_tx, frame_rx) = sync_channel::<Picture>(2);
        let (status_tx, status_rx) = channel::<Status>();
        let (input_tx, input_rx) = unbounded_channel::<ToWorker>();
        // Replacing the sender closes the old worker's input, which ends it.
        *self.input_tx.borrow_mut() = Some(input_tx);
        self.reset_resize();
        self.offered.set(false);
        self.gave_up.set(false);
        let generation = self.generation.get() + 1;
        self.generation.set(generation);
        let inbox = Arc::new(Inbox::default());
        *self.inbox.borrow_mut() = inbox.clone();
        self.set_status(&match &self.endpoint {
            Endpoint::Ssh(t) => format!(
                "Connecting to {}…\nIf ssh asks for a password or to trust the host, answer it in the terminal.",
                t.host
            ),
            Endpoint::Tcp(addr) => format!("Connecting to {addr}…"),
        });

        let worker = Worker {
            endpoint: self.endpoint.clone(),
            video: gliff_sw::VideoMode::resolve(self.cli.video.as_deref()),
            frames: frame_tx,
            status: status_tx,
            input: input_rx,
            keymap: self.keymap.subscribe(),
        };
        std::thread::Builder::new()
            .name("gliff-net".into())
            .spawn(move || worker.run())
            .expect("spawn network thread");
        // Window handles are not Send; posting to one from any thread is
        // fine, so the relays carry it as a number.
        let hwnd = self.hwnd as isize;
        {
            let inbox = inbox.clone();
            std::thread::Builder::new()
                .name("gliff-frames".into())
                .spawn(move || {
                    for picture in frame_rx {
                        *inbox.frame.lock().unwrap_or_else(|e| e.into_inner()) = Some(picture);
                        if !inbox.frame_posted.swap(true, Ordering::SeqCst) {
                            post(hwnd, WM_APP_FRAME, generation);
                        }
                    }
                })
                .expect("spawn frame relay");
        }
        std::thread::Builder::new()
            .name("gliff-status".into())
            .spawn(move || {
                for status in status_rx {
                    inbox
                        .status
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .push_back(status);
                    post(hwnd, WM_APP_STATUS, generation);
                }
            })
            .expect("spawn status relay");
    }

    fn reset_resize(&self) {
        self.resize_requested.set((0, 0));
        self.resize_scale.set(0.0);
    }

    fn take_frame(&self, generation: u64) {
        if generation != self.generation.get() {
            return;
        }
        let inbox = self.inbox.borrow().clone();
        inbox.frame_posted.store(false, Ordering::SeqCst);
        let Some(p) = inbox.frame.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let frame = match p.frame {
            Frame::Bgra(frame) => Shown::Cpu(frame),
            Frame::Dmabuf(frame) => Shown::Gpu(frame),
        };
        // Geometry follows the frame that really paints, so the pointer
        // never maps against a picture that is not on screen.
        self.stream_size.set(p.stream);
        self.stream_view.set(p.view);
        self.stream_scale
            .set(f64::from(p.scale_milli.max(1)) / 1000.0);
        *self.frame.borrow_mut() = Some(frame);
        self.paint_now();
    }

    fn take_statuses(self: &Rc<Self>, generation: u64) {
        if generation != self.generation.get() {
            return;
        }
        let inbox = self.inbox.borrow().clone();
        loop {
            let next = inbox
                .status
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .pop_front();
            let Some(status) = next else {
                return;
            };
            if !self.on_status(status) {
                return;
            }
        }
    }

    /// Apply one status; false once the worker has ended.
    fn on_status(self: &Rc<Self>, status: Status) -> bool {
        match status {
            Status::Connected {
                video,
                view_width,
                view_height,
                fps_cap,
                headless,
            } => {
                self.server_view.set((view_width, view_height));
                self.fps_cap.set(fps_cap);
                self.headless.set(headless);
                self.resize_requested.set((0, 0));
                self.retries.set(0);
                self.connected.set(true);
                self.set_status("");
                self.set_stats(&video);
                self.remember_machine();
                // A fresh server starts at its own default size; bring it to
                // the window.
                self.request_resize();
                if headless && self.pointer.get() == Pointer::Default {
                    self.set_pointer(Pointer::Hidden);
                }
                if !self.offered.replace(true) {
                    if let (Some(cb), Some(tx)) = (&self.clipboard, self.input_tx.borrow().as_ref())
                    {
                        cb.offer_to(tx);
                    }
                }
                self.update_capture();
            }
            Status::Stats {
                fps,
                mbit,
                decode_ms,
                video,
            } => {
                let (sw, sh) = self.stream_size.get();
                let (vw, vh) = self.stream_view.get();
                let stream = if (sw, sh) == (vw, vh) {
                    format!("{sw}x{sh}")
                } else {
                    format!("{sw}x{sh} → {vw}x{vh}")
                };
                let cap = self.fps_cap.get();
                let line = format!(
                    "{video}  {fps:.0} fps  {mbit:.1} Mbit/s  decode {decode_ms:.1} ms  {stream} @{cap}"
                );
                tracing::debug!(stats = %line);
                self.set_stats(&line);
            }
            Status::Cursor(latest) => {
                let Some(crate::net::CursorShape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb,
                }) = latest.take()
                else {
                    return true;
                };
                let remote = has_visible_shape(&argb)
                    .then(|| gdi::cursor_from_bgra(width, height, hot_x, hot_y, &argb))
                    .flatten();
                let pointer = match remote {
                    Some(c) => Pointer::Remote(c),
                    None if self.headless.get() => Pointer::Hidden,
                    None => Pointer::Default,
                };
                self.set_pointer(pointer);
            }
            Status::ClipboardOffer {
                serial,
                mime_types,
                files,
            } => {
                if let (Some(cb), Some(tx)) = (&self.clipboard, self.input_tx.borrow().as_ref()) {
                    cb.set_remote_offer(tx, serial, &mime_types, &files);
                }
            }
            Status::ClipboardRead {
                mime_type,
                generation,
                reply,
            } => {
                if let Some(cb) = &self.clipboard {
                    cb.read_local(&mime_type, generation, reply);
                }
            }
            Status::ClipboardTransfer { id, progress } => {
                tracing::debug!(id, state = ?progress.state, "clipboard transfer");
                match &progress.state {
                    State::Running => self.show_hint(&format!(
                        "Pasting {}: {}",
                        progress.label,
                        describe(&progress)
                    )),
                    State::Failed(e) => self.show_hint(&format!("Paste failed: {e}")),
                    State::Done | State::Cancelled => self.hide_hint(),
                }
            }
            Status::Error(e) => {
                tracing::error!(machine = %self.name, error = %e, "connection failed");
                self.show_disconnected();
                self.set_status(&format!("Error: {e}"));
                self.schedule_reconnect();
                return false;
            }
            Status::Incompatible(message) => {
                tracing::error!(machine = %self.name, error = %message, "incompatible server");
                self.show_disconnected();
                self.set_status(&message);
                self.gave_up.set(true);
                return false;
            }
            Status::Closed => {
                self.show_disconnected();
                self.schedule_reconnect();
                return false;
            }
        }
        true
    }

    fn schedule_reconnect(&self) {
        let n = self.retries.get() + 1;
        self.retries.set(n);
        if n > MAX_RETRIES {
            self.set_status("Disconnected — click to reconnect");
            self.gave_up.set(true);
            return;
        }
        let error = self.status.borrow().clone();
        let attempt = format!("Reconnecting… (attempt {n})");
        self.set_status(&if error.is_empty() {
            attempt
        } else {
            format!("{error}\n{attempt}")
        });
        // SAFETY: a timer on our window, delivered as WM_TIMER.
        unsafe { SetTimer(self.hwnd, TIMER_RECONNECT, 1500, None) };
    }

    /// Back to a black screen with no stream geometry.
    fn show_disconnected(&self) {
        self.connected.set(false);
        self.update_capture();
        self.frame.borrow_mut().take();
        self.stream_size.set((0, 0));
        self.stream_view.set((0, 0));
        self.server_view.set((0, 0));
        self.set_pointer(Pointer::Default);
        self.set_stats("");
        self.release_buttons();
    }

    /// Remember the machine on the first successful connect, so a bare
    /// `gliff` comes back to it.
    fn remember_machine(&self) {
        if !matches!(self.endpoint, Endpoint::Ssh(_)) || self.remembered.replace(true) {
            return;
        }
        let mut config = Config::load(&self.config_path);
        config.remember(&self.name);
        config.set_open(&self.name, true);
        config.shown = Some(self.name.clone());
        if let Err(e) = config.save(&self.config_path) {
            tracing::warn!(path = %self.config_path.display(), error = %e, "cannot save config");
        }
    }

    fn send(&self, msg: ClientMsg) {
        if !self.connected.get() {
            return;
        }
        if let Some(tx) = self.input_tx.borrow().as_ref() {
            let _ = tx.send(ToWorker::Send(msg));
        }
    }

    // ---- size ----

    fn read_client_size(&self) {
        let mut r = RECT {
            left: 0,
            top: 0,
            right: 0,
            bottom: 0,
        };
        // SAFETY: `r` is a valid out pointer for our window's client area.
        unsafe { GetClientRect(self.hwnd, &mut r) };
        self.client.set((
            (r.right - r.left).max(0) as u32,
            (r.bottom - r.top).max(0) as u32,
        ));
    }

    /// Ask the server to match the window once the size has settled for
    /// 200 ms, so a drag-resize does not restart the encoder on every step.
    fn size_changed(&self) {
        // SAFETY: a timer on our window; setting it again restarts it.
        unsafe { SetTimer(self.hwnd, TIMER_RESIZE, 200, None) };
        self.invalidate();
    }

    /// Send a Resize if the server's view does not already match the
    /// window. The gate compares the server's view, not the stream, so a
    /// stream the server reduced on purpose is left alone.
    fn request_resize(&self) {
        let (w, h) = self.client.get();
        let size = (w & !1, h & !1);
        let scale = self.scale.get();
        if !self.connected.get()
            || size.0 < 64
            || size.1 < 64
            || (self.resize_scale.get() == scale
                && (size == self.server_view.get() || size == self.resize_requested.get()))
        {
            return;
        }
        self.resize_requested.set(size);
        self.resize_scale.set(scale);
        self.send(ClientMsg::Resize {
            width: size.0,
            height: size.1,
            scale: scale as f32,
        });
    }

    fn fit(&self) -> Option<Fit> {
        fit(self.stream_view.get(), self.client.get(), self.scale.get())
    }

    fn to_remote(&self, p: (i32, i32)) -> (f64, f64) {
        match self.fit() {
            Some(f) => f.to_remote(
                p,
                self.stream_view.get(),
                self.stream_size.get(),
                self.stream_scale.get(),
            ),
            None => (0.0, 0.0),
        }
    }

    fn toggle_fullscreen(&self) {
        let hwnd = self.hwnd;
        let windowed = self.windowed.borrow_mut().take();
        // SAFETY: style, placement and monitor calls on our own window with
        // valid in/out structures. No RefCell is borrowed across them, as
        // they resize the window synchronously.
        unsafe {
            match windowed {
                Some((style, placement)) => {
                    SetWindowLongPtrW(hwnd, GWL_STYLE, style);
                    SetWindowPlacement(hwnd, &placement);
                    SetWindowPos(
                        hwnd,
                        std::ptr::null_mut(),
                        0,
                        0,
                        0,
                        0,
                        SWP_NOMOVE
                            | SWP_NOSIZE
                            | SWP_NOZORDER
                            | SWP_NOOWNERZORDER
                            | SWP_FRAMECHANGED,
                    );
                }
                None => {
                    let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
                    let mut placement: WINDOWPLACEMENT = std::mem::zeroed();
                    placement.length = std::mem::size_of::<WINDOWPLACEMENT>() as u32;
                    let mut monitor: MONITORINFO = std::mem::zeroed();
                    monitor.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
                    if GetWindowPlacement(hwnd, &mut placement) == 0
                        || GetMonitorInfoW(
                            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST),
                            &mut monitor,
                        ) == 0
                    {
                        return;
                    }
                    *self.windowed.borrow_mut() = Some((style, placement));
                    SetWindowLongPtrW(hwnd, GWL_STYLE, style & !(WS_OVERLAPPEDWINDOW as isize));
                    let r = monitor.rcMonitor;
                    SetWindowPos(
                        hwnd,
                        HWND_TOP,
                        r.left,
                        r.top,
                        r.right - r.left,
                        r.bottom - r.top,
                        SWP_NOOWNERZORDER | SWP_FRAMECHANGED,
                    );
                }
            }
        }
        self.check_menu_item(IDM_FULLSCREEN, self.windowed.borrow().is_some());
    }

    fn add_menu_items(&self) {
        // SAFETY: our window's own system menu, with NUL-terminated labels.
        unsafe {
            let menu = GetSystemMenu(self.hwnd, 0);
            if menu.is_null() {
                return;
            }
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
            for (id, label) in [
                (IDM_FULLSCREEN, "Full screen\tF11"),
                (IDM_STATS, "Show statistics"),
                (IDM_RECONNECT, "Reconnect"),
            ] {
                AppendMenuW(menu, MF_STRING, id, wide(label).as_ptr());
            }
        }
    }

    fn check_menu_item(&self, id: usize, checked: bool) {
        // SAFETY: our window's own system menu.
        unsafe {
            let menu = GetSystemMenu(self.hwnd, 0);
            if !menu.is_null() {
                let state = if checked { MF_CHECKED } else { MF_UNCHECKED };
                CheckMenuItem(menu, id as u32, MF_BYCOMMAND | state);
            }
        }
    }

    // ---- input ----

    /// The keyboard goes to the remote while the window is in front, the
    /// pointer is over it, and the user has not released it.
    fn update_capture(&self) {
        let want =
            self.active.get() && self.inside.get() && !self.released.get() && self.connected.get();
        if want == self.capturing.replace(want) {
            return;
        }
        hook::set_capture(want);
        if want {
            if self.hotkey != Hotkey::None && !self.capture_hinted.replace(true) {
                self.show_hint(&format!(
                    "Shortcuts captured — {} to release",
                    self.hotkey.describe()
                ));
            }
        } else {
            self.release_pressed_keys();
        }
    }

    /// A key from the hook.
    fn key(&self, vk: u32, packed: LPARAM) {
        let scan = (packed & 0xFF) as u32;
        let extended = packed & hook::EXTENDED != 0;
        let pressed = packed & hook::PRESSED != 0;
        let modifier = keys::modifier_of(vk);
        tracing::trace!(
            vk,
            scan,
            extended,
            pressed,
            capturing = self.capturing.get(),
            "key"
        );
        if pressed {
            // A press that raced the end of the capture stays local.
            if !self.capturing.get() {
                return;
            }
            if modifier == 0 && self.hotkey.matches(vk, self.mods(), &self.last_tap) {
                self.release_pressed_keys();
                self.released.set(true);
                self.update_capture();
                self.show_hint("Shortcuts released — click the screen to capture again");
                return;
            }
            if modifier != 0 {
                self.held_mods.borrow_mut().insert(vk);
            }
        } else if modifier != 0 {
            self.held_mods.borrow_mut().remove(&vk);
        }
        let Some(code) = keys::evdev_code(scan, extended, vk) else {
            tracing::debug!(vk, scan, extended, "key has no evdev code");
            return;
        };
        // Auto-repeat presses are dropped (the remote repeats on its own),
        // and so are releases of keys whose press was never sent.
        let forward = {
            let mut keys = self.pressed_keys.borrow_mut();
            if pressed {
                keys.insert(code)
            } else {
                keys.remove(&code)
            }
        };
        if forward {
            self.send(ClientMsg::Key {
                keycode: code,
                pressed,
            });
        }
    }

    fn mods(&self) -> u8 {
        self.held_mods
            .borrow()
            .iter()
            .fold(0, |m, &vk| m | keys::modifier_of(vk))
    }

    /// Release every key held on the remote, as when the keyboard goes back
    /// to Windows.
    fn release_pressed_keys(&self) {
        self.held_mods.borrow_mut().clear();
        let held = std::mem::take(&mut *self.pressed_keys.borrow_mut());
        for code in held {
            self.send(ClientMsg::Key {
                keycode: code,
                pressed: false,
            });
        }
    }

    fn button(self: &Rc<Self>, msg: u32, wparam: WPARAM, pressed: bool) {
        let (bit, evdev) = match msg {
            WM_LBUTTONDOWN | WM_LBUTTONUP => (1, 0x110), // BTN_LEFT
            WM_RBUTTONDOWN | WM_RBUTTONUP => (2, 0x111), // BTN_RIGHT
            WM_MBUTTONDOWN | WM_MBUTTONUP => (4, 0x112), // BTN_MIDDLE
            _ if hiword(wparam) == u32::from(XBUTTON1) => (8, 0x116), // BTN_SIDE (back)
            _ => (16, 0x115),                            // BTN_EXTRA (forward)
        };
        if pressed {
            if self.gave_up.get() {
                self.retries.set(0);
                self.start_session();
                return;
            }
            self.released.set(false);
            self.update_capture();
            // Keep getting the pointer while a button is held outside.
            if self.buttons.get() == 0 {
                // SAFETY: captures the mouse for our own window.
                unsafe { SetCapture(self.hwnd) };
            }
            self.buttons.set(self.buttons.get() | bit);
        } else {
            if self.buttons.get() & bit == 0 {
                return;
            }
            self.buttons.set(self.buttons.get() & !bit);
            if self.buttons.get() == 0 {
                // SAFETY: releases our own capture.
                unsafe { ReleaseCapture() };
            }
        }
        self.send(ClientMsg::PointerButton {
            button: evdev,
            pressed,
        });
    }

    /// Release the remote's buttons, as when the mouse capture is lost.
    fn release_buttons(&self) {
        let held = self.buttons.replace(0);
        for (bit, evdev) in [(1, 0x110), (2, 0x111), (4, 0x112), (8, 0x116), (16, 0x115)] {
            if held & bit != 0 {
                self.send(ClientMsg::PointerButton {
                    button: evdev,
                    pressed: false,
                });
            }
        }
    }

    /// A wheel turn of `delta` (120 per notch, positive away from the user
    /// or to the right). High-resolution wheels report fractions of a notch;
    /// they scroll smoothly and count a step at each whole notch.
    fn wheel(&self, axis: Axis, delta: i16) {
        let delta = i32::from(delta);
        let (v, h) = self.wheel.get();
        let total = if axis == Axis::Vertical { v } else { h } + delta;
        let notches = total / 120;
        let rest = total - notches * 120;
        self.wheel.set(if axis == Axis::Vertical {
            (rest, h)
        } else {
            (v, rest)
        });
        // The remote's axes grow downwards and rightwards.
        let sign = if axis == Axis::Vertical { -1 } else { 1 };
        self.send(ClientMsg::PointerAxis {
            axis,
            value: f64::from(sign * delta) / 120.0 * 15.0,
            discrete: (notches != 0).then_some(sign * notches),
            stop: false,
        });
    }

    fn set_pointer(&self, pointer: Pointer) {
        let old = self.pointer.replace(pointer);
        if self.inside.get() {
            self.apply_pointer();
        }
        if let Pointer::Remote(c) = old {
            if old != pointer {
                // SAFETY: a cursor we created, no longer the one shown.
                unsafe { DestroyCursor(c) };
            }
        }
    }

    fn apply_pointer(&self) {
        // SAFETY: a cursor we own or a shared system cursor; null hides it.
        unsafe {
            SetCursor(match self.pointer.get() {
                Pointer::Remote(c) => c,
                Pointer::Hidden => std::ptr::null_mut(),
                Pointer::Default => LoadCursorW(std::ptr::null_mut(), IDC_ARROW),
            });
        }
    }

    // ---- drawing ----

    fn invalidate(&self) {
        // SAFETY: invalidates our whole client area.
        unsafe { InvalidateRect(self.hwnd, std::ptr::null(), 0) };
    }

    /// Draw a new frame right away rather than waiting for WM_PAINT, which
    /// Windows delivers only when the queue is otherwise empty.
    fn paint_now(&self) {
        // SAFETY: a query on our own window.
        if unsafe { IsIconic(self.hwnd) } != 0 {
            return;
        }
        let started = Instant::now();
        if !self.present() {
            // SAFETY: a device context for our own window, released after.
            unsafe {
                let hdc = GetDC(self.hwnd);
                if hdc.is_null() {
                    return;
                }
                self.paint(hdc);
                ReleaseDC(self.hwnd, hdc);
            }
        }
        tracing::trace!(us = started.elapsed().as_micros() as u64, "painted");
    }

    /// Draw through Direct3D; false when there is no presenter and GDI
    /// draws instead.
    fn present(&self) -> bool {
        let mut presenter = self.presenter.borrow_mut();
        let Some(presenter) = presenter.as_mut() else {
            return false;
        };
        let frame = self.frame.borrow();
        let source = match (frame.as_ref(), self.fit()) {
            (Some(f), Some(fit)) => {
                let source = match f {
                    Shown::Gpu(frame) => Source::Nv12(frame),
                    Shown::Cpu(frame) => Source::Bgra {
                        width: frame.width,
                        height: frame.height,
                        data: frame.pixels.as_ref(),
                    },
                };
                let dest = windows::Win32::Foundation::RECT {
                    left: fit.x,
                    top: fit.y,
                    right: fit.x + fit.width,
                    bottom: fit.y + fit.height,
                };
                Some((source, dest))
            }
            _ => None,
        };
        let labels = self.labels(source.is_some());
        let labels: Vec<(&str, Anchor)> = labels.iter().map(|(t, a)| (t.as_str(), *a)).collect();
        if let Err(e) = presenter.render(source, &labels, self.scale.get()) {
            // Once: a lost device fails every frame after.
            if !self.present_failed.replace(true) {
                tracing::warn!(error = %format!("{e:#}"), "Direct3D drawing failed");
            }
        }
        true
    }

    /// The GDI path, without a presenter. Only CPU frames come this way.
    fn paint(&self, hdc: HDC) {
        let (cw, ch) = self.client.get();
        let client = (cw as i32, ch as i32);
        let frame = self.frame.borrow();
        let picture = match (frame.as_ref(), self.fit()) {
            (Some(Shown::Cpu(f)), Some(fit)) => {
                gdi::draw_frame(
                    hdc,
                    client,
                    &fit,
                    &Pixels {
                        width: f.width,
                        height: f.height,
                        data: f.pixels.as_ref(),
                    },
                );
                true
            }
            _ => {
                gdi::fill_black(
                    hdc,
                    &RECT {
                        left: 0,
                        top: 0,
                        right: client.0,
                        bottom: client.1,
                    },
                );
                false
            }
        };
        for (text, anchor) in self.labels(picture) {
            self.label(hdc, &text, anchor);
        }
    }

    /// The status (when there is no picture), statistics and hint labels.
    fn labels(&self, picture: bool) -> Vec<(String, Anchor)> {
        let mut labels = Vec::new();
        let status = self.status.borrow();
        if !picture && !status.is_empty() {
            labels.push((status.clone(), Anchor::Center));
        }
        let stats = self.stats.borrow();
        if self.show_stats.get() && !stats.is_empty() {
            labels.push((stats.clone(), Anchor::TopLeft));
        }
        let hint = self.hint.borrow();
        if !hint.is_empty() {
            labels.push((hint.clone(), Anchor::Bottom));
        }
        labels
    }

    /// Draw `text` on a dark plate at `anchor`.
    fn label(&self, hdc: HDC, text: &str, anchor: Anchor) {
        let (cw, ch) = self.client.get();
        let (cw, ch) = (cw as i32, ch as i32);
        let pad = (8.0 * self.scale.get()) as i32;
        let max_width = label_width(cw, self.scale.get(), pad);
        let mut text: Vec<u16> = text.encode_utf16().collect();
        let format = DT_CENTER | DT_WORDBREAK | DT_NOPREFIX;
        // SAFETY: drawing on a live device context with our font, a
        // temporary brush that is deleted, and a mutable UTF-16 buffer.
        unsafe {
            let old_font = SelectObject(hdc, self.font.get());
            let mut r = RECT {
                left: 0,
                top: 0,
                right: max_width,
                bottom: 0,
            };
            DrawTextW(
                hdc,
                text.as_mut_ptr(),
                text.len() as i32,
                &mut r,
                format | DT_CALCRECT,
            );
            let (w, h) = (r.right - r.left, r.bottom - r.top);
            let (x, y) = place(anchor, (cw, ch), (w, h), pad);
            let mut text_rect = RECT {
                left: x,
                top: y,
                right: x + w,
                bottom: y + h,
            };
            let plate = RECT {
                left: x - pad,
                top: y - pad / 2,
                right: x + w + pad,
                bottom: y + h + pad / 2,
            };
            let brush = CreateSolidBrush(0x0020_2020);
            FillRect(hdc, &plate, brush);
            DeleteObject(brush);
            SetBkMode(hdc, TRANSPARENT as i32);
            SetTextColor(hdc, 0x00E8_E8E8);
            DrawTextW(
                hdc,
                text.as_mut_ptr(),
                text.len() as i32,
                &mut text_rect,
                format,
            );
            SelectObject(hdc, old_font);
        }
    }

    fn set_status(&self, text: &str) {
        *self.status.borrow_mut() = text.to_string();
        self.invalidate();
    }

    fn set_stats(&self, text: &str) {
        *self.stats.borrow_mut() = text.to_string();
        if self.show_stats.get() {
            self.invalidate();
        }
    }

    fn show_hint(&self, text: &str) {
        *self.hint.borrow_mut() = text.to_string();
        // SAFETY: a timer on our window; setting it again restarts it.
        unsafe { SetTimer(self.hwnd, TIMER_HINT, 3500, None) };
        self.invalidate();
    }

    fn hide_hint(&self) {
        self.hint.borrow_mut().clear();
        self.invalidate();
    }

    fn timer(self: &Rc<Self>, id: usize) {
        // Every timer here is one-shot.
        // SAFETY: our own window's timer.
        unsafe { KillTimer(self.hwnd, id) };
        match id {
            TIMER_RESIZE => self.request_resize(),
            TIMER_HINT => self.hide_hint(),
            TIMER_RECONNECT => self.start_session(),
            _ => {}
        }
    }
}

fn post(hwnd: isize, msg: u32, generation: u64) {
    // SAFETY: posting plain values; a window that is gone makes the call
    // fail harmlessly.
    unsafe { PostMessageW(hwnd as HWND, msg, generation as usize, 0) };
}

/// Whether `ssh.exe` is on the PATH (Windows ships it as an optional
/// feature, on by default).
fn ssh_installed() -> bool {
    std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|dir| dir.join("ssh.exe").is_file()))
}
