//! The UI thread's half of the clipboard on Windows, the counterpart of
//! `clipboard_ui` on Linux: it reports local copies to the worker, reads the
//! local clipboard when the server pastes, and puts the server's selection
//! on the Windows clipboard with delayed rendering, so nothing is fetched
//! until a program pastes. The proxy carries the markers Windows defines
//! for clipboard monitors to skip; otherwise Clipboard History and the
//! cross-device clipboard would fetch every remote copy at once. Monitors
//! that ignore the markers still read what they want.
//!
//! Text and images (PNG, and a plain bitmap for programs that only read
//! `CF_DIB`) cross in both directions. Copied files do not cross yet.
//!
//! A program pasting the server's selection waits in `GetClipboardData`
//! while the UI thread fetches the item, so the fetch has a deadline. The
//! keyboard hook runs on its own thread and does not wait for this one.

use std::cell::RefCell;
use std::io;
use std::sync::mpsc as std_mpsc;
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::mpsc::{self, UnboundedSender, WeakUnboundedSender};
use tokio::sync::oneshot;

use gliff_proto::clipboard::{
    forwardable_mimes, is_text_mime, local_mimes_for_offer, resolve_mime, CHUNK, TEXT_MIME,
};
use gliff_proto::ClipboardFile;
use gliff_transport::clipboard::files::LocalFiles;
use windows_sys::Win32::Foundation::{GlobalFree, HANDLE, HWND};
use windows_sys::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
    GetClipboardOwner, GetOpenClipboardWindow, IsClipboardFormatAvailable, OpenClipboard,
    RegisterClipboardFormatW, SetClipboardData,
};
use windows_sys::Win32::System::Memory::{
    GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
};
use windows_sys::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};
use windows_sys::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

use super::convert::{crlf_to_lf, dib_to_png, lf_to_crlf, png_to_dib};
use super::gdi::wide;
use crate::clipboard::ToWorker;

const IMAGE_MIME: &str = "image/png";
/// How long a paste may wait for the server before the program gets nothing.
const RENDER_TIMEOUT: Duration = Duration::from_secs(30);
/// When gliff closes owning the clipboard, text is still fetched so it
/// outlives the window, but only this long.
const EXIT_RENDER_TIMEOUT: Duration = Duration::from_secs(3);

const TEXT: u32 = CF_UNICODETEXT as u32;
const DIB: u32 = CF_DIB as u32;

#[derive(Clone)]
struct RemoteOffer {
    serial: u32,
    mimes: Vec<String>,
    /// Weak, so a replaced session's channel is not kept open by an offer.
    tx: WeakUnboundedSender<ToWorker>,
}

pub struct Clipboard {
    hwnd: HWND,
    /// The registered "PNG" format that browsers and Office use.
    png: u32,
    /// Formats whose presence marks a selection that must not be shared
    /// (password managers set them).
    private: [u32; 2],
    /// Its value 0 also marks a private selection.
    history: u32,
    /// Its value 0 keeps a selection off the cross-device clipboard.
    cloud: u32,
    remote: RefCell<Option<RemoteOffer>>,
    /// The last item fetched for the current offer, so the PNG and bitmap
    /// formats of one image are fetched once.
    fetched: RefCell<Option<(u32, String, Vec<u8>)>>,
}

impl Clipboard {
    /// Watch the clipboard for `hwnd`.
    pub fn new(hwnd: HWND) -> Self {
        let register = |name: &str| {
            // SAFETY: a NUL-terminated name; returns 0 on failure.
            unsafe { RegisterClipboardFormatW(wide(name).as_ptr()) }
        };
        // SAFETY: `hwnd` is our live window; it now gets WM_CLIPBOARDUPDATE.
        if unsafe { AddClipboardFormatListener(hwnd) } == 0 {
            tracing::warn!(error = %io::Error::last_os_error(), "cannot watch the clipboard");
        }
        Self {
            hwnd,
            png: register("PNG"),
            private: [
                register("ExcludeClipboardContentFromMonitorProcessing"),
                register("Clipboard Viewer Ignore"),
            ],
            history: register("CanIncludeInClipboardHistory"),
            cloud: register("CanUploadToCloudClipboard"),
            remote: RefCell::new(None),
            fetched: RefCell::new(None),
        }
    }

    fn owns(&self) -> bool {
        // SAFETY: no arguments; returns the owner window or null.
        unsafe { GetClipboardOwner() == self.hwnd }
    }

    fn available(format: u32) -> bool {
        // SAFETY: a query by format number, open or not.
        format != 0 && unsafe { IsClipboardFormatAvailable(format) } != 0
    }

    /// Whether the selection is marked private. Needs the clipboard open
    /// to read the history flag's value.
    fn is_private(&self) -> bool {
        if self.private.iter().any(|&f| Self::available(f)) {
            return true;
        }
        Self::available(self.history)
            && read_global(self.history).is_some_and(|v| v.get(..4) == Some(&[0, 0, 0, 0]))
    }

    /// The mime types to offer the server for the current local selection,
    /// or `None` when the change is our own proxy for the server's.
    pub fn local_offer(&self) -> Option<Vec<String>> {
        if self.owns() {
            return None;
        }
        let Some(_open) = Open::new(self.hwnd) else {
            return Some(Vec::new());
        };
        if self.is_private() {
            tracing::debug!("local selection is marked private; not offered");
            return Some(Vec::new());
        }
        let mut mimes = Vec::new();
        if Self::available(TEXT) {
            mimes.push(TEXT_MIME.to_string());
        }
        if Self::available(self.png) || Self::available(DIB) {
            mimes.push(IMAGE_MIME.to_string());
        }
        Some(forwardable_mimes(&mimes))
    }

    /// Report the local selection to a session's worker.
    pub fn offer_to(&self, tx: &UnboundedSender<ToWorker>) {
        if let Some(mime_types) = self.local_offer() {
            let _ = tx.send(ToWorker::LocalOffer {
                mime_types,
                files: LocalFiles::default(),
            });
        }
    }

    /// Read `mime_type` from the local clipboard for the server, streaming
    /// it into `reply` from another thread; dropping `reply` ends the item.
    pub fn read_local(&self, mime_type: &str, reply: mpsc::Sender<io::Result<Bytes>>) {
        let data = self.read(mime_type);
        std::thread::spawn(move || match data {
            Ok(data) => {
                for chunk in data.chunks(CHUNK) {
                    if reply
                        .blocking_send(Ok(Bytes::copy_from_slice(chunk)))
                        .is_err()
                    {
                        break;
                    }
                }
            }
            Err(e) => {
                let _ = reply.blocking_send(Err(e));
            }
        });
    }

    fn read(&self, mime_type: &str) -> io::Result<Vec<u8>> {
        // Reading our own proxy would ask the server for its own item.
        if self.owns() {
            return Err(io::Error::other("clipboard holds the server's offer"));
        }
        let _open = Open::new(self.hwnd).ok_or_else(|| io::Error::other("clipboard is busy"))?;
        if self.is_private() {
            return Err(io::Error::other("clipboard selection is marked private"));
        }
        if is_text_mime(mime_type) {
            let raw = read_global(TEXT).ok_or_else(|| io::Error::other("clipboard has no text"))?;
            let units: Vec<u16> = raw
                .chunks_exact(2)
                .map(|c| u16::from_le_bytes([c[0], c[1]]))
                .take_while(|&u| u != 0)
                .collect();
            return Ok(crlf_to_lf(&String::from_utf16_lossy(&units)).into_bytes());
        }
        if mime_type == IMAGE_MIME {
            if let Some(png) = read_global(self.png) {
                return Ok(png);
            }
            let dib = read_global(DIB).ok_or_else(|| io::Error::other("clipboard has no image"))?;
            return dib_to_png(&dib).map_err(io::Error::other);
        }
        Err(io::Error::other(format!("clipboard has no {mime_type}")))
    }

    /// Make the server's offer the Windows selection, with delayed
    /// rendering, or drop ours for an offer Windows cannot paste.
    pub fn set_remote_offer(
        &self,
        tx: &UnboundedSender<ToWorker>,
        serial: u32,
        mime_types: &[String],
        files: &[ClipboardFile],
    ) {
        let mimes = local_mimes_for_offer(mime_types, false);
        let text = mimes.iter().any(|m| is_text_mime(m));
        let image = mimes.iter().any(|m| m == IMAGE_MIME);
        if !files.is_empty() {
            tracing::info!(
                files = files.len(),
                "copied files are not shared on Windows yet"
            );
        }
        let Some(_open) = Open::new(self.hwnd) else {
            tracing::warn!("clipboard is busy; the server's selection was not taken");
            return;
        };
        self.fetched.borrow_mut().take();
        if !text && !image {
            if self.owns() {
                // SAFETY: the clipboard is open by this window.
                unsafe { EmptyClipboard() };
            }
            self.remote.borrow_mut().take();
            return;
        }
        // SAFETY: the clipboard is open by this window, which becomes its
        // owner; a null handle announces a format rendered on request.
        unsafe {
            EmptyClipboard();
            if text {
                SetClipboardData(TEXT, std::ptr::null_mut());
            }
            if image {
                if self.png != 0 {
                    SetClipboardData(self.png, std::ptr::null_mut());
                }
                SetClipboardData(DIB, std::ptr::null_mut());
            }
        }
        // Presence marks the first two; the flags say no with a 0 value.
        for marker in [self.private[0], self.private[1], self.history, self.cloud] {
            if marker != 0 {
                set_global(marker, &0u32.to_le_bytes());
            }
        }
        // EmptyClipboard sent WM_DESTROYCLIPBOARD for any older proxy of
        // ours, so this is stored after it.
        *self.remote.borrow_mut() = Some(RemoteOffer {
            serial,
            mimes,
            tx: tx.downgrade(),
        });
    }

    /// WM_RENDERFORMAT: a program pastes `format` from the server's offer.
    pub fn render(&self, format: u32) {
        self.render_within(format, RENDER_TIMEOUT);
    }

    fn render_within(&self, format: u32, timeout: Duration) {
        let Some(offer) = self.remote.borrow().clone() else {
            return;
        };
        let mut pid = 0;
        // SAFETY: plain queries; a null window yields process id 0.
        unsafe { GetWindowThreadProcessId(GetOpenClipboardWindow(), &mut pid) };
        tracing::debug!(format, pid, "a program pastes from the server's selection");
        let mime = if format == TEXT {
            match resolve_mime(TEXT_MIME, &offer.mimes) {
                Some(m) => m.to_string(),
                None => return,
            }
        } else if format == self.png || format == DIB {
            IMAGE_MIME.to_string()
        } else {
            return;
        };
        let data = match self.fetch(&offer, &mime, timeout) {
            Ok(data) => data,
            Err(e) => {
                tracing::warn!(mime, error = %e, "paste from the server failed");
                return;
            }
        };
        let rendered = if format == TEXT {
            let text = lf_to_crlf(&String::from_utf8_lossy(&data));
            let units: Vec<u8> = text
                .encode_utf16()
                .chain(std::iter::once(0))
                .flat_map(u16::to_le_bytes)
                .collect();
            Ok(units)
        } else if format == DIB {
            png_to_dib(&data)
        } else {
            Ok(data)
        };
        match rendered {
            Ok(bytes) => set_global(format, &bytes),
            Err(e) => tracing::warn!(mime, error = %e, "cannot convert the pasted item"),
        }
    }

    /// Fetch `mime` from the server, waiting at most `timeout`.
    fn fetch(&self, offer: &RemoteOffer, mime: &str, timeout: Duration) -> Result<Vec<u8>, String> {
        if let Some((serial, m, data)) = self.fetched.borrow().as_ref() {
            if *serial == offer.serial && m == mime {
                return Ok(data.clone());
            }
        }
        let tx = offer.tx.upgrade().ok_or("the session has ended")?;
        let (sink, mut chunks) = mpsc::channel::<Bytes>(4);
        let (result, outcome) = oneshot::channel();
        tx.send(ToWorker::Fetch {
            mime_type: mime.to_string(),
            serial: offer.serial,
            sink,
            result,
        })
        .map_err(|_| "the session has ended")?;
        // Collect on another thread so the wait can have a deadline; the
        // thread ends with the transfer either way.
        let (done_tx, done_rx) = std_mpsc::channel();
        std::thread::spawn(move || {
            let mut data = Vec::new();
            while let Some(chunk) = chunks.blocking_recv() {
                data.extend_from_slice(&chunk);
            }
            let r = outcome
                .blocking_recv()
                .unwrap_or_else(|_| Err("the transfer was cancelled".into()));
            let _ = done_tx.send(r.map(|()| data));
        });
        let data = done_rx
            .recv_timeout(timeout)
            .map_err(|_| format!("no answer from the server within {}s", timeout.as_secs()))??;
        *self.fetched.borrow_mut() = Some((offer.serial, mime.to_string(), data.clone()));
        Ok(data)
    }

    /// WM_RENDERALLFORMATS: the window is closing while it owns the
    /// clipboard. Text is fetched so it can still be pasted; anything
    /// larger is dropped rather than holding the exit up.
    pub fn render_all(&self) {
        let Some(_open) = Open::new(self.hwnd) else {
            return;
        };
        if !self.owns() {
            return;
        }
        self.render_within(TEXT, EXIT_RENDER_TIMEOUT);
    }

    /// WM_DESTROYCLIPBOARD: another program took the clipboard.
    pub fn released(&self) {
        self.remote.borrow_mut().take();
        self.fetched.borrow_mut().take();
    }
}

/// The clipboard, open for this window until dropped. Another program may
/// hold it for a moment, so opening retries briefly.
struct Open;

impl Open {
    fn new(hwnd: HWND) -> Option<Self> {
        for _ in 0..10 {
            // SAFETY: `hwnd` is our live window.
            if unsafe { OpenClipboard(hwnd) } != 0 {
                return Some(Self);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        None
    }
}

impl Drop for Open {
    fn drop(&mut self) {
        // SAFETY: this guard opened the clipboard.
        unsafe { CloseClipboard() };
    }
}

/// Copy a clipboard format's bytes; the clipboard must be open.
fn read_global(format: u32) -> Option<Vec<u8>> {
    // SAFETY: the clipboard is open (callers hold `Open`); the handle stays
    // owned by the clipboard and is only read between lock and unlock.
    unsafe {
        let handle: HANDLE = GetClipboardData(format);
        if handle.is_null() {
            return None;
        }
        let size = GlobalSize(handle);
        let ptr = GlobalLock(handle).cast::<u8>();
        if ptr.is_null() {
            return None;
        }
        let data = std::slice::from_raw_parts(ptr, size).to_vec();
        GlobalUnlock(handle);
        Some(data)
    }
}

/// Hand `bytes` to the clipboard as `format`. The clipboard must be open
/// for us, or be rendering for us.
fn set_global(format: u32, bytes: &[u8]) {
    // SAFETY: a fresh movable block of the right size, filled between lock
    // and unlock; the clipboard owns it once SetClipboardData succeeds and
    // it is freed here otherwise.
    unsafe {
        let handle = GlobalAlloc(GMEM_MOVEABLE, bytes.len().max(1));
        if handle.is_null() {
            return;
        }
        let ptr = GlobalLock(handle).cast::<u8>();
        if ptr.is_null() {
            GlobalFree(handle);
            return;
        }
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr, bytes.len());
        GlobalUnlock(handle);
        if SetClipboardData(format, handle).is_null() {
            tracing::warn!(error = %io::Error::last_os_error(), "clipboard rejected the pasted item");
            GlobalFree(handle);
        }
    }
}
