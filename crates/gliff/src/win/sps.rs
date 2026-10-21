//! Declare "no reordering" in H.264 sequence parameter sets.
//!
//! gliff's encoders send IDR and P pictures in display order, but their SPS
//! carries no VUI, so it does not say so. A decoder must then assume
//! pictures may be reordered and hold as many as the level's buffer allows
//! before showing the first: Media Foundation's decoder held 16, low-latency
//! mode or not. Appending a VUI whose bitstream restriction says
//! `max_num_reorder_frames = 0` lets it show each picture as soon as it is
//! decoded. Works on any server version; an SPS that already has a VUI is
//! left alone.

/// `au` with every SPS rewritten to declare no reordering, or `None` when
/// nothing changed (no SPS, or each already has a VUI).
pub fn declare_no_reordering(au: &[u8]) -> Option<Vec<u8>> {
    // Only SPS payloads are replaced; every other byte is copied as it was.
    let mut out = Vec::new();
    let mut copied = 0;
    for (start, end) in nal_units(au) {
        if start >= end || au[start] & 0x1f != 7 {
            continue;
        }
        if let Some(payload) = patch_sps(&au[start + 1..end]) {
            out.extend_from_slice(&au[copied..start + 1]);
            out.extend(escape(&payload));
            copied = end;
        }
    }
    if copied == 0 {
        return None;
    }
    out.extend_from_slice(&au[copied..]);
    Some(out)
}

/// The NAL units of an Annex B access unit, as byte ranges without start
/// codes.
fn nal_units(au: &[u8]) -> Vec<(usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = starts.get(k + 1).map_or(au.len(), |&n| n - 3);
        // A four-byte start code leaves its leading zero on the unit
        // before; trailing zeros are never part of a NAL unit.
        while e > s && au[e - 1] == 0 {
            e -= 1;
        }
        units.push((s, e));
    }
    units
}

/// The SPS payload (after the NAL header, escaped) with the VUI appended,
/// unescaped; `None` to keep it as it is.
fn patch_sps(escaped: &[u8]) -> Option<Vec<u8>> {
    let rbsp = unescape(escaped);
    let mut r = BitReader::new(&rbsp);
    let profile_idc = r.u(8)?;
    r.u(16)?; // constraint flags, level_idc
    r.ue()?; // seq_parameter_set_id
    if matches!(
        profile_idc,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        if r.ue()? == 3 {
            r.u(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.u(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.u(1)? == 1 {
            // Scaling matrices: not ours, leave it.
            return None;
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?;
        }
        1 => {
            r.u(1)?;
            r.ue()?; // offset_for_non_ref_pic (se, same length)
            r.ue()?; // offset_for_top_to_bottom_field
            for _ in 0..r.ue()? {
                r.ue()?;
            }
        }
        _ => {}
    }
    let max_num_ref_frames = r.ue()?;
    r.u(1)?; // gaps_in_frame_num_value_allowed_flag
    r.ue()?; // pic_width_in_mbs_minus1
    r.ue()?; // pic_height_in_map_units_minus1
    if r.u(1)? == 0 {
        r.u(1)?; // mb_adaptive_frame_field_flag
    }
    r.u(1)?; // direct_8x8_inference_flag
    if r.u(1)? == 1 {
        for _ in 0..4 {
            r.ue()?; // frame_crop_*_offset
        }
    }
    let vui_flag = r.pos;
    if r.u(1)? == 1 {
        return None;
    }
    // Only the stop bit may follow; anything else is not an SPS we know.
    if last_set_bit(&rbsp)? != vui_flag + 1 {
        return None;
    }
    let mut w = BitWriter::default();
    for i in 0..vui_flag {
        w.bit(bit_at(&rbsp, i));
    }
    w.bit(true); // vui_parameters_present_flag
    w.bit(false); // aspect_ratio_info_present_flag
    w.bit(false); // overscan_info_present_flag
    w.bit(false); // video_signal_type_present_flag
    w.bit(false); // chroma_loc_info_present_flag
    w.bit(false); // timing_info_present_flag
    w.bit(false); // nal_hrd_parameters_present_flag
    w.bit(false); // vcl_hrd_parameters_present_flag
    w.bit(false); // pic_struct_present_flag
    w.bit(true); // bitstream_restriction_flag
    w.bit(true); // motion_vectors_over_pic_boundaries_flag
    w.ue(2); // max_bytes_per_pic_denom (the inferred value)
    w.ue(1); // max_bits_per_mb_denom (the inferred value)
    w.ue(16); // log2_max_mv_length_horizontal (the inferred value)
    w.ue(16); // log2_max_mv_length_vertical
    w.ue(0); // max_num_reorder_frames
    w.ue(max_num_ref_frames.max(1)); // max_dec_frame_buffering
    w.bit(true); // rbsp_stop_one_bit
    Some(w.bytes)
}

fn bit_at(bytes: &[u8], i: usize) -> bool {
    bytes[i / 8] >> (7 - i % 8) & 1 == 1
}

fn last_set_bit(bytes: &[u8]) -> Option<usize> {
    let (i, b) = bytes.iter().enumerate().rev().find(|(_, b)| **b != 0)?;
    Some(i * 8 + 7 - b.trailing_zeros() as usize)
}

/// Remove emulation-prevention bytes.
fn unescape(escaped: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(escaped.len());
    let mut zeros = 0;
    for &b in escaped {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// Insert emulation-prevention bytes so no `00 00 0x` (x <= 3) appears.
fn escape(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len() + 4);
    let mut zeros = 0;
    for &b in raw {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

struct BitReader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn u(&mut self, n: u32) -> Option<u32> {
        let mut v = 0;
        for _ in 0..n {
            if self.pos >= self.bytes.len() * 8 {
                return None;
            }
            v = v << 1 | u32::from(bit_at(self.bytes, self.pos));
            self.pos += 1;
        }
        Some(v)
    }

    /// `ue(v)`; `se(v)` has the same length, so it skips those too.
    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.u(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.u(zeros)?)
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    len: usize,
}

impl BitWriter {
    fn bit(&mut self, v: bool) {
        if self.len.is_multiple_of(8) {
            self.bytes.push(0);
        }
        if v {
            let last = self.bytes.len() - 1;
            self.bytes[last] |= 1 << (7 - self.len % 8);
        }
        self.len += 1;
    }

    fn ue(&mut self, v: u32) {
        let x = u64::from(v) + 1;
        let n = 64 - x.leading_zeros();
        for _ in 1..n {
            self.bit(false);
        }
        for i in (0..n).rev() {
            self.bit(x >> i & 1 == 1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A High-profile SPS like gliff's writer makes: POC type 0, and
    /// cropping when the height is not a whole number of macroblocks.
    fn sps(max_num_ref_frames: u32, vui: bool, crop: bool) -> Vec<u8> {
        let mut w = BitWriter::default();
        for (n, v) in [(8, 100), (8, 0), (8, 51)] {
            for i in (0..n).rev() {
                w.bit(v >> i & 1 == 1);
            }
        }
        w.ue(0); // sps_id
        w.ue(1); // chroma_format_idc
        w.ue(0);
        w.ue(0);
        w.bit(false);
        w.bit(false); // no scaling matrix
        w.ue(4); // log2_max_frame_num_minus4
        w.ue(0); // poc type
        w.ue(4);
        w.ue(max_num_ref_frames);
        w.bit(false); // gaps
        w.ue(159); // 2560 wide
        w.ue(89); // 1440 high
        w.bit(true); // frame_mbs_only
        w.bit(true); // direct_8x8
        w.bit(crop);
        if crop {
            // 1368 lines coded as 1376: 4 chroma rows off the bottom.
            for v in [0, 0, 0, 4] {
                w.ue(v);
            }
        }
        w.bit(vui);
        if vui {
            for _ in 0..9 {
                w.bit(false);
            }
        }
        w.bit(true); // stop bit
        let mut nal = vec![0, 0, 0, 1, 0x67];
        nal.extend(escape(&w.bytes));
        nal
    }

    fn vui_fields(au: &[u8]) -> (u32, u32) {
        let (s, e) = nal_units(au)[0];
        let rbsp = unescape(&au[s + 1..e]);
        let mut r = BitReader::new(&rbsp);
        r.u(24).unwrap();
        for _ in 0..4 {
            r.ue().unwrap();
        }
        r.u(2).unwrap();
        r.ue().unwrap();
        r.ue().unwrap();
        r.ue().unwrap();
        r.ue().unwrap(); // max_num_ref_frames
        r.u(1).unwrap();
        r.ue().unwrap();
        r.ue().unwrap();
        r.u(2).unwrap(); // frame_mbs_only, direct_8x8
        if r.u(1).unwrap() == 1 {
            for _ in 0..4 {
                r.ue().unwrap();
            }
        }
        assert_eq!(r.u(1), Some(1), "vui_parameters_present_flag");
        assert_eq!(r.u(9), Some(1), "only the bitstream restriction");
        assert_eq!(r.u(1), Some(1));
        for _ in 0..4 {
            r.ue().unwrap();
        }
        let reorder = r.ue().unwrap();
        let buffering = r.ue().unwrap();
        assert_eq!(r.u(1), Some(1), "stop bit");
        (reorder, buffering)
    }

    #[test]
    fn appends_a_no_reordering_vui() {
        let mut au = sps(1, false, false);
        let idr = [0, 0, 0, 1, 0x65, 0x88, 0x84, 0x00, 0x33];
        au.extend_from_slice(&idr);
        let patched = declare_no_reordering(&au).expect("the SPS has no VUI");
        assert_eq!(vui_fields(&patched), (0, 1));
        assert!(patched.ends_with(&idr), "other units pass through");
    }

    #[test]
    fn buffering_covers_the_reference_frames() {
        let patched = declare_no_reordering(&sps(4, false, false)).unwrap();
        assert_eq!(vui_fields(&patched), (0, 4));
    }

    #[test]
    fn finds_the_vui_flag_after_cropping() {
        let patched = declare_no_reordering(&sps(1, false, true)).unwrap();
        assert_eq!(vui_fields(&patched), (0, 1));
    }

    #[test]
    fn leaves_a_stream_that_already_has_a_vui() {
        assert_eq!(declare_no_reordering(&sps(1, true, false)), None);
        assert_eq!(declare_no_reordering(&sps(1, true, true)), None);
        let p_frame = [0, 0, 1, 0x41, 0x9a, 0x00, 0x00];
        assert_eq!(declare_no_reordering(&p_frame), None);
    }

    #[test]
    fn escaping_round_trips() {
        let raw = [0, 0, 0, 0, 1, 0, 0, 2, 0, 0, 3, 7];
        assert_eq!(unescape(&escape(&raw)), raw);
    }
}
