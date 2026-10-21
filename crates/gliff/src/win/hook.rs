//! The keyboard capture: a low-level keyboard hook that, while the window
//! is in front and captures, takes every key (Win, Alt+Tab, Alt+F4
//! included) before Windows acts on it and posts it to the window for the
//! remote.
//!
//! The hook runs on its own thread with its own message loop. Windows drops
//! a low-level hook that keeps it waiting, so it must never wait on the UI
//! thread, which can block on a clipboard paste; it only posts.
//!
//! Each key keeps the owner its press had: a key pressed before the capture
//! began is released to Windows even if the capture is on by then, and a
//! key whose press went to the remote has its release swallowed even after
//! the capture ends, so neither side ever sees a key stuck down.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::VK_PACKET;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetForegroundWindow, GetMessageW, PostMessageW, SetWindowsHookExW, HC_ACTION,
    KBDLLHOOKSTRUCT, LLKHF_EXTENDED, MSG, WH_KEYBOARD_LL, WM_APP, WM_KEYDOWN, WM_SYSKEYDOWN,
};

/// Posted to the window for each captured key event. `wParam` is the
/// virtual key, `lParam` the scan code with [`EXTENDED`] and [`PRESSED`].
pub const WM_APP_KEY: u32 = WM_APP + 1;
pub const EXTENDED: isize = 1 << 16;
pub const PRESSED: isize = 1 << 17;

static CAPTURE: AtomicBool = AtomicBool::new(false);
static TARGET: AtomicIsize = AtomicIsize::new(0);

#[derive(Clone, Copy, PartialEq)]
enum Owner {
    Windows,
    Remote,
}

thread_local! {
    /// Keys held down now, by scan code and extended flag, and who got
    /// their press.
    static HELD: RefCell<HashMap<u32, Owner>> = RefCell::new(HashMap::new());
}

/// Start or stop taking keys from Windows. Keys already held keep going
/// where their press went.
pub fn set_capture(on: bool) {
    CAPTURE.store(on, Ordering::SeqCst);
}

/// Install the hook on its own thread, posting to `hwnd`.
pub fn start(hwnd: HWND) -> std::io::Result<()> {
    TARGET.store(hwnd as isize, Ordering::SeqCst);
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("gliff-keyboard".into())
        .spawn(move || {
            // SAFETY: the hook procedure is a plain function that lives for
            // the program; a low-level hook needs this module's handle and
            // runs on this thread, whose message loop follows.
            let hook = unsafe {
                SetWindowsHookExW(
                    WH_KEYBOARD_LL,
                    Some(hook_proc),
                    GetModuleHandleW(std::ptr::null()),
                    0,
                )
            };
            if hook.is_null() {
                let _ = tx.send(Err(std::io::Error::last_os_error()));
                return;
            }
            let _ = tx.send(Ok(()));
            // SAFETY: MSG is plain data; zeroed is a valid empty message.
            let mut msg: MSG = unsafe { std::mem::zeroed() };
            // SAFETY: `msg` is a valid out pointer. The loop only pumps
            // the hook's calls; nothing is posted to this thread.
            while unsafe { GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) } > 0 {}
        })?;
    rx.recv()
        .unwrap_or_else(|_| Err(std::io::Error::other("keyboard thread ended")))
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        // SAFETY: for HC_ACTION, lParam points to the event's
        // KBDLLHOOKSTRUCT for the duration of this call.
        let kb = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
        if take(kb, wparam as u32) {
            return 1;
        }
    }
    // SAFETY: passes the event on unchanged; the hook handle is ignored.
    unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
}

/// Decide who gets this key event; true means it is the remote's and
/// Windows must not see it.
fn take(kb: &KBDLLHOOKSTRUCT, msg: u32) -> bool {
    // Characters typed by input methods and on-screen keyboards have no key.
    if kb.vkCode == u32::from(VK_PACKET) {
        return false;
    }
    let pressed = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let extended = kb.flags & LLKHF_EXTENDED != 0;
    let id = kb.scanCode | if extended { 1 << 16 } else { 0 };
    let target = TARGET.load(Ordering::SeqCst) as HWND;
    let owner = HELD.with(|held| {
        let mut held = held.borrow_mut();
        if pressed {
            *held.entry(id).or_insert_with(|| {
                // SAFETY: no arguments; returns a handle or null.
                let front = unsafe { GetForegroundWindow() };
                if CAPTURE.load(Ordering::SeqCst) && front == target {
                    Owner::Remote
                } else {
                    Owner::Windows
                }
            })
        } else {
            held.remove(&id).unwrap_or(Owner::Windows)
        }
    });
    if owner == Owner::Windows {
        return false;
    }
    // AltGr arrives as a synthetic left Ctrl (scan code 0x21D) ahead of
    // right Alt. The remote's keymap makes right Alt AltGr by itself, and
    // an extra Ctrl would turn every AltGr character into a shortcut.
    if kb.scanCode & 0xFF00 == 0 {
        let mut packed = (kb.scanCode & 0xFF) as isize;
        if extended {
            packed |= EXTENDED;
        }
        if pressed {
            packed |= PRESSED;
        }
        // SAFETY: posting plain values to a window handle; a stale or
        // destroyed handle makes the call fail, which is harmless.
        unsafe { PostMessageW(target, WM_APP_KEY, kb.vkCode as usize, packed) };
    }
    true
}
