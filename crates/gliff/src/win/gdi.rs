//! Small Win32 helpers: wide strings, the remote cursor as an `HCURSOR`,
//! and drawing the decoded frame.

use std::ffi::c_void;

use windows_sys::Win32::Foundation::RECT;
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, FillRect, GetStockObject, SetBrushOrgEx,
    SetStretchBltMode, StretchDIBits, BITMAPINFO, BITMAPINFOHEADER, BITMAPV5HEADER, BI_BITFIELDS,
    BI_RGB, BLACK_BRUSH, COLORONCOLOR, DIB_RGB_COLORS, HALFTONE, HBRUSH, HDC, SRCCOPY,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{CreateIconIndirect, HCURSOR, ICONINFO};

use super::fit::Fit;

/// `s` as a NUL-terminated UTF-16 string.
pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// The remote cursor image (BGRA, straight alpha) as a Windows cursor, or
/// `None` when it cannot be made.
pub fn cursor_from_bgra(
    width: u32,
    height: u32,
    hot_x: i32,
    hot_y: i32,
    bgra: &[u8],
) -> Option<HCURSOR> {
    let needed = width as usize * height as usize * 4;
    if width == 0 || height == 0 || width > 1024 || height > 1024 || bgra.len() < needed {
        return None;
    }
    // SAFETY: BITMAPV5HEADER is plain data; zero is valid for every field.
    let mut header: BITMAPV5HEADER = unsafe { std::mem::zeroed() };
    header.bV5Size = std::mem::size_of::<BITMAPV5HEADER>() as u32;
    header.bV5Width = width as i32;
    header.bV5Height = -(height as i32); // top-down, like the wire image
    header.bV5Planes = 1;
    header.bV5BitCount = 32;
    header.bV5Compression = BI_BITFIELDS;
    header.bV5RedMask = 0x00FF_0000;
    header.bV5GreenMask = 0x0000_FF00;
    header.bV5BlueMask = 0x0000_00FF;
    header.bV5AlphaMask = 0xFF00_0000;
    let mut bits: *mut c_void = std::ptr::null_mut();
    // SAFETY: the header describes a 32-bit top-down DIB and `bits`
    // receives a pointer to its pixel memory, owned by the returned bitmap.
    let color = unsafe {
        CreateDIBSection(
            std::ptr::null_mut(),
            (&header as *const BITMAPV5HEADER).cast::<BITMAPINFO>(),
            DIB_RGB_COLORS,
            &mut bits,
            std::ptr::null_mut(),
            0,
        )
    };
    if color.is_null() || bits.is_null() {
        return None;
    }
    // SAFETY: the section holds exactly width * height * 4 bytes, and
    // `bgra` has at least that many.
    unsafe { std::ptr::copy_nonoverlapping(bgra.as_ptr(), bits.cast::<u8>(), needed) };
    // An all-zero monochrome mask: with a 32-bit colour bitmap, the alpha
    // channel decides what shows.
    // SAFETY: a null pointer asks for an uninitialised (zeroed) bitmap.
    let mask = unsafe { CreateBitmap(width as i32, height as i32, 1, 1, std::ptr::null()) };
    let info = ICONINFO {
        fIcon: 0,
        xHotspot: hot_x.clamp(0, width as i32 - 1) as u32,
        yHotspot: hot_y.clamp(0, height as i32 - 1) as u32,
        hbmMask: mask,
        hbmColor: color,
    };
    // SAFETY: both bitmaps are valid; the cursor copies them, so they are
    // deleted right after.
    let cursor = unsafe { CreateIconIndirect(&info) };
    // SAFETY: the bitmaps are ours and no longer selected anywhere.
    unsafe {
        DeleteObject(color);
        DeleteObject(mask);
    }
    (!cursor.is_null()).then_some(cursor)
}

/// A decoded frame ready to draw: packed BGRX rows.
pub struct Pixels<'a> {
    pub width: u32,
    pub height: u32,
    pub data: &'a [u8],
}

/// Paint the client area: the frame at `fit`, black around it. An integer
/// enlargement repeats pixels exactly; any other size is resampled.
pub fn draw_frame(hdc: HDC, client: (i32, i32), fit: &Fit, frame: &Pixels) {
    let (cw, ch) = client;
    let bars = [
        RECT {
            left: 0,
            top: 0,
            right: cw,
            bottom: fit.y,
        },
        RECT {
            left: 0,
            top: fit.y + fit.height,
            right: cw,
            bottom: ch,
        },
        RECT {
            left: 0,
            top: fit.y,
            right: fit.x,
            bottom: fit.y + fit.height,
        },
        RECT {
            left: fit.x + fit.width,
            top: fit.y,
            right: cw,
            bottom: fit.y + fit.height,
        },
    ];
    for r in &bars {
        if r.right > r.left && r.bottom > r.top {
            fill_black(hdc, r);
        }
    }
    if frame.data.len() < frame.width as usize * frame.height as usize * 4 {
        return;
    }
    // SAFETY: BITMAPINFO is plain data; zero is valid for every field.
    let mut bmi: BITMAPINFO = unsafe { std::mem::zeroed() };
    bmi.bmiHeader = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: frame.width as i32,
        biHeight: -(frame.height as i32), // top-down
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        ..bmi.bmiHeader
    };
    // Exact when each frame pixel becomes a whole block of screen pixels
    // (a reduced stream is stretched to the view, so check the frame).
    let (dw, dh) = (fit.width.max(0) as u32, fit.height.max(0) as u32);
    let exact = dw >= frame.width
        && dh >= frame.height
        && dw.is_multiple_of(frame.width)
        && dh.is_multiple_of(frame.height)
        && dw / frame.width == dh / frame.height;
    // SAFETY: `hdc` is a live device context; the pixel buffer holds
    // width * height * 4 bytes as the header describes.
    unsafe {
        SetStretchBltMode(hdc, if exact { COLORONCOLOR } else { HALFTONE });
        if !exact {
            SetBrushOrgEx(hdc, 0, 0, std::ptr::null_mut());
        }
        StretchDIBits(
            hdc,
            fit.x,
            fit.y,
            fit.width,
            fit.height,
            0,
            0,
            frame.width as i32,
            frame.height as i32,
            frame.data.as_ptr().cast(),
            &bmi,
            DIB_RGB_COLORS,
            SRCCOPY,
        );
    }
}

pub fn fill_black(hdc: HDC, r: &RECT) {
    // SAFETY: stock objects need no cleanup; `hdc` is a live device context.
    unsafe { FillRect(hdc, r, GetStockObject(BLACK_BRUSH) as HBRUSH) };
}
