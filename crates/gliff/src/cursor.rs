//! Rules for the remote cursor image that both UIs share.

/// Whether a remote cursor image (BGRA) shows anything: it needs some alpha
/// and more than one colour. Hyprland sends a flat or fully transparent
/// image when it has no cursor image to share; those fall back to the
/// default pointer.
pub fn has_visible_shape(argb: &[u8]) -> bool {
    let mut pixels = argb.chunks_exact(4);
    let Some(first) = pixels.next() else {
        return false;
    };
    let opaque = first[3] != 0 || pixels.clone().any(|p| p[3] != 0);
    let uniform = pixels.all(|p| p == first);
    opaque && !uniform
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn visible_shape_needs_alpha_and_contrast() {
        let transparent = [0u8; 16];
        let black = [0, 0, 0, 255].repeat(4);
        let mut arrow = [0u8; 16];
        arrow[3] = 255;
        assert!(!has_visible_shape(&transparent));
        assert!(!has_visible_shape(&black));
        assert!(has_visible_shape(&arrow));
    }
}
