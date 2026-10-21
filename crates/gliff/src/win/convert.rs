//! Clipboard data between Windows formats and the mime types the server
//! speaks: text line endings, and device-independent bitmaps (`CF_DIB`)
//! against PNG.

/// Largest bitmap side and pixel count accepted, which bounds the memory
/// one paste can take whatever its header claims.
const MAX_SIDE: u32 = 16_384;
const MAX_PIXELS: u64 = 64 * 1024 * 1024;

/// Windows text to Unix line endings.
pub fn crlf_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Unix text to Windows line endings; lines already ending in CRLF stay.
pub fn lf_to_crlf(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + text.len() / 32);
    let mut prev = '\0';
    for c in text.chars() {
        if c == '\n' && prev != '\r' {
            out.push('\r');
        }
        out.push(c);
        prev = c;
    }
    out
}

fn u16_at(b: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

fn i32_at(b: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

const BI_RGB: u32 = 0;
const BI_BITFIELDS: u32 = 3;

/// Encode a `CF_DIB` or `CF_DIBV5` block (32- or 24-bit, uncompressed or
/// with the standard bit fields) as PNG.
pub fn dib_to_png(dib: &[u8]) -> Result<Vec<u8>, String> {
    let bad = || "malformed bitmap".to_string();
    let header = u32_at(dib, 0).ok_or_else(bad)? as usize;
    if header < 40 || header > dib.len() {
        return Err(bad());
    }
    let width = i32_at(dib, 4).ok_or_else(bad)?;
    let height = i32_at(dib, 8).ok_or_else(bad)?;
    let bits = u16_at(dib, 14).ok_or_else(bad)?;
    let compression = u32_at(dib, 16).ok_or_else(bad)?;
    let colors_used = u32_at(dib, 32).ok_or_else(bad)? as usize;
    let (w, h) = (width.unsigned_abs(), height.unsigned_abs());
    if width <= 0
        || h == 0
        || w > MAX_SIDE
        || h > MAX_SIDE
        || u64::from(w) * u64::from(h) > MAX_PIXELS
    {
        return Err(format!("unsupported bitmap size {width}x{height}"));
    }
    let mut masks = None;
    let mut offset = header;
    match (bits, compression) {
        (32 | 24, BI_RGB) => {}
        (32, BI_BITFIELDS) => {
            // A V4/V5 header holds the masks; a plain one is followed by them.
            let at = if header >= 52 { 40 } else { header };
            let m = (
                u32_at(dib, at).ok_or_else(bad)?,
                u32_at(dib, at + 4).ok_or_else(bad)?,
                u32_at(dib, at + 8).ok_or_else(bad)?,
            );
            if header < 52 {
                offset += 12;
            }
            masks = Some(m);
        }
        _ => {
            return Err(format!(
                "unsupported bitmap format ({bits} bit, compression {compression})"
            ))
        }
    }
    if masks.is_some_and(|m| m != (0x00FF_0000, 0x0000_FF00, 0x0000_00FF)) {
        return Err("unsupported bitmap channel layout".into());
    }
    offset += colors_used * 4;
    let bpp = usize::from(bits / 8);
    let stride = (w as usize * bpp).div_ceil(4) * 4;
    let pixels = dib
        .get(offset..offset + stride * h as usize)
        .ok_or_else(bad)?;
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for row in 0..h as usize {
        // Positive heights are stored bottom row first.
        let src = if height > 0 {
            h as usize - 1 - row
        } else {
            row
        };
        let line = &pixels[src * stride..src * stride + w as usize * bpp];
        for px in line.chunks_exact(bpp) {
            let a = if bpp == 4 { px[3] } else { 255 };
            rgba.extend_from_slice(&[px[2], px[1], px[0], a]);
        }
    }
    // A 32-bit bitmap without alpha leaves the fourth byte zero; showing
    // that as fully transparent would lose the picture.
    if bpp == 4 && rgba.chunks_exact(4).all(|p| p[3] == 0) {
        rgba.chunks_exact_mut(4).for_each(|p| p[3] = 255);
    }
    encode_png(w, h, &rgba)
}

fn encode_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut enc = png::Encoder::new(&mut out, width, height);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header().map_err(|e| e.to_string())?;
    writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    writer.finish().map_err(|e| e.to_string())?;
    Ok(out)
}

/// Decode a PNG into a `CF_DIB` block: 32-bit, bottom-up, opaque. Most
/// Windows programs ignore the fourth byte of a plain DIB, so translucent
/// pixels are blended onto white rather than left to chance; programs that
/// understand transparency take the PNG format offered alongside.
pub fn png_to_dib(data: &[u8]) -> Result<Vec<u8>, String> {
    let mut decoder = png::Decoder::new(std::io::Cursor::new(data));
    decoder.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = decoder.read_info().map_err(|e| e.to_string())?;
    let (w, h) = {
        let info = reader.info();
        (info.width, info.height)
    };
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || u64::from(w) * u64::from(h) > MAX_PIXELS
    {
        return Err(format!("unsupported image size {w}x{h}"));
    }
    let mut buf = vec![0; reader.output_buffer_size()];
    let frame = reader.next_frame(&mut buf).map_err(|e| e.to_string())?;
    let channels = frame.color_type.samples();
    if frame.bit_depth != png::BitDepth::Eight {
        return Err("unsupported PNG bit depth".into());
    }
    let mut dib = Vec::with_capacity(40 + (w * h * 4) as usize);
    dib.extend_from_slice(&40u32.to_le_bytes());
    dib.extend_from_slice(&(w as i32).to_le_bytes());
    dib.extend_from_slice(&(h as i32).to_le_bytes());
    dib.extend_from_slice(&1u16.to_le_bytes()); // planes
    dib.extend_from_slice(&32u16.to_le_bytes()); // bits per pixel
    dib.extend_from_slice(&BI_RGB.to_le_bytes());
    dib.extend_from_slice(&(w * h * 4).to_le_bytes()); // image size
    dib.extend_from_slice(&[0; 16]); // resolution and palette: none
    for row in (0..h as usize).rev() {
        let line = &buf[row * frame.line_size..row * frame.line_size + w as usize * channels];
        for px in line.chunks_exact(channels) {
            let (r, g, b, a) = match channels {
                1 => (px[0], px[0], px[0], 255),
                2 => (px[0], px[0], px[0], px[1]),
                3 => (px[0], px[1], px[2], 255),
                _ => (px[0], px[1], px[2], px[3]),
            };
            let over_white =
                |c: u8| ((u32::from(c) * u32::from(a) + 255 * (255 - u32::from(a))) / 255) as u8;
            dib.extend_from_slice(&[over_white(b), over_white(g), over_white(r), 255]);
        }
    }
    Ok(dib)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_endings_round_trip() {
        assert_eq!(crlf_to_lf("a\r\nb\r\n"), "a\nb\n");
        assert_eq!(lf_to_crlf("a\nb\n"), "a\r\nb\r\n");
        assert_eq!(lf_to_crlf("a\r\nb\n"), "a\r\nb\r\n");
        assert_eq!(lf_to_crlf("no newline"), "no newline");
        // A lone CR is not a line ending to rewrite.
        assert_eq!(crlf_to_lf("a\rb"), "a\rb");
    }

    fn decode_png(png: &[u8]) -> (u32, u32, Vec<u8>) {
        let mut reader = png::Decoder::new(std::io::Cursor::new(png))
            .read_info()
            .unwrap();
        let mut buf = vec![0; reader.output_buffer_size()];
        let info = reader.next_frame(&mut buf).unwrap();
        buf.truncate(info.buffer_size());
        (info.width, info.height, buf)
    }

    /// A 2x2 image: red, green / blue, half-transparent white.
    fn sample_rgba() -> Vec<u8> {
        vec![
            255, 0, 0, 255, 0, 255, 0, 255, //
            0, 0, 255, 255, 255, 255, 255, 128,
        ]
    }

    #[test]
    fn png_to_dib_is_bottom_up_bgr_and_opaque() {
        let png = encode_png(2, 2, &sample_rgba()).unwrap();
        let dib = png_to_dib(&png).unwrap();
        assert_eq!(dib.len(), 40 + 16);
        assert_eq!(i32_at(&dib, 8), Some(2)); // positive height: bottom-up
                                              // First stored row is the bottom one: blue, then white over white.
        assert_eq!(&dib[40..44], &[255, 0, 0, 255]);
        assert_eq!(&dib[44..48], &[255, 255, 255, 255]);
        // Then the top row: red, green.
        assert_eq!(&dib[48..52], &[0, 0, 255, 255]);
        assert_eq!(&dib[52..56], &[0, 255, 0, 255]);
    }

    #[test]
    fn dib_round_trips_through_png() {
        let png = encode_png(2, 2, &sample_rgba()).unwrap();
        let back = dib_to_png(&png_to_dib(&png).unwrap()).unwrap();
        let (w, h, rgba) = decode_png(&back);
        assert_eq!((w, h), (2, 2));
        assert_eq!(
            &rgba[..12],
            &[255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255]
        );
    }

    #[test]
    fn top_down_24_bit_rows_are_padded() {
        // 1x2, 24-bit, top-down: each 3-byte row is padded to 4.
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&(-2i32).to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&24u16.to_le_bytes());
        dib.extend_from_slice(&[0; 24]);
        dib.extend_from_slice(&[0, 0, 255, 0]); // red, padding
        dib.extend_from_slice(&[255, 0, 0, 0]); // blue, padding
        let (w, h, rgba) = decode_png(&dib_to_png(&dib).unwrap());
        assert_eq!((w, h), (1, 2));
        assert_eq!(rgba, vec![255, 0, 0, 255, 0, 0, 255, 255]);
    }

    #[test]
    fn bitfields_after_a_plain_header_are_skipped() {
        let mut dib = Vec::new();
        dib.extend_from_slice(&40u32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&1i32.to_le_bytes());
        dib.extend_from_slice(&1u16.to_le_bytes());
        dib.extend_from_slice(&32u16.to_le_bytes());
        dib.extend_from_slice(&BI_BITFIELDS.to_le_bytes());
        dib.extend_from_slice(&[0; 20]);
        for m in [0x00FF_0000u32, 0x0000_FF00, 0x0000_00FF] {
            dib.extend_from_slice(&m.to_le_bytes());
        }
        dib.extend_from_slice(&[10, 20, 30, 0]);
        let (_, _, rgba) = decode_png(&dib_to_png(&dib).unwrap());
        // No alpha anywhere means opaque.
        assert_eq!(rgba, vec![30, 20, 10, 255]);
    }

    #[test]
    fn rejects_what_it_cannot_read() {
        assert!(dib_to_png(&[]).is_err());
        assert!(dib_to_png(&[40, 0, 0, 0]).is_err());
        let mut huge = Vec::new();
        huge.extend_from_slice(&40u32.to_le_bytes());
        huge.extend_from_slice(&100_000i32.to_le_bytes());
        huge.extend_from_slice(&100_000i32.to_le_bytes());
        huge.extend_from_slice(&1u16.to_le_bytes());
        huge.extend_from_slice(&32u16.to_le_bytes());
        huge.extend_from_slice(&[0; 24]);
        assert!(dib_to_png(&huge).is_err());
        assert!(png_to_dib(b"not a png").is_err());
    }
}
