//! Fit the remote view into the window, all in physical pixels: Windows
//! hands a per-monitor-DPI-aware window its client area in device pixels,
//! so no logical/device split is needed here (compare `paintable::layout`
//! on Linux, which this follows).

/// Where the frame is drawn inside the client area, in device pixels.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
    /// Device pixels per view pixel.
    pub factor: f64,
}

impl Fit {
    /// Map a client-area point to the remote output's logical coordinates:
    /// invert the fit to view pixels, scale to stream pixels (a reduced
    /// stream is stretched to the view), then divide by the remote scale.
    pub fn to_remote(
        self,
        point: (i32, i32),
        view: (u32, u32),
        stream: (u32, u32),
        scale: f64,
    ) -> (f64, f64) {
        if view.0 == 0 || view.1 == 0 || self.factor <= 0.0 {
            return (0.0, 0.0);
        }
        let vx = f64::from(point.0 - self.x) / self.factor;
        let vy = f64::from(point.1 - self.y) / self.factor;
        let px = (vx * f64::from(stream.0) / f64::from(view.0)).clamp(0.0, f64::from(stream.0));
        let py = (vy * f64::from(stream.1) / f64::from(view.1)).clamp(0.0, f64::from(stream.1));
        let scale = scale.max(0.01);
        (px / scale, py / scale)
    }
}

/// The step Hyprland rounds a headless mode to at `scale`, so the remote
/// view can be up to one step smaller than the window that asked for it
/// (the same rule as `hypr_ipc::clean_mode`).
pub fn mode_step(scale: f64) -> u32 {
    let Some(q) = (1..=8u32).find(|q| {
        let numerator = scale * f64::from(*q);
        (numerator - numerator.round()).abs() < 1e-4
    }) else {
        return 2;
    };
    let p = (scale * f64::from(q)).round() as u32;
    if p.is_multiple_of(2) {
        p
    } else {
        2 * p
    }
}

/// Fit `view` into a `client` area on a display of `scale` (DPI / 96). An
/// integer enlargement is kept when the slack is within the remote's mode
/// alignment, so a headless output that is a few pixels smaller than the
/// window stays pixel-exact; otherwise the view fills the window.
pub fn fit(view: (u32, u32), client: (u32, u32), scale: f64) -> Option<Fit> {
    let (vw, vh) = (f64::from(view.0), f64::from(view.1));
    let (cw, ch) = (f64::from(client.0), f64::from(client.1));
    if vw <= 0.0 || vh <= 0.0 || cw <= 0.0 || ch <= 0.0 || !scale.is_finite() {
        return None;
    }
    let fit = (cw / vw).min(ch / vh);
    let integer = fit.floor();
    let step = f64::from(mode_step(scale.clamp(0.5, 4.0)));
    let slack = (cw - vw * integer).min(ch - vh * integer);
    let factor = if integer >= 1.0 && slack < step + 1.0 {
        integer
    } else {
        fit
    };
    let (w, h) = ((vw * factor).round(), (vh * factor).round());
    Some(Fit {
        x: ((cw - w) / 2.0).floor() as i32,
        y: ((ch - h) / 2.0).floor() as i32,
        width: w as i32,
        height: h as i32,
        factor,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_fit_is_one_to_one() {
        let f = fit((1920, 1080), (1920, 1080), 1.0).unwrap();
        assert_eq!((f.x, f.y, f.width, f.height), (0, 0, 1920, 1080));
        assert_eq!(f.factor, 1.0);
    }

    #[test]
    fn a_slightly_larger_window_keeps_pixels_exact() {
        // A 1.25 display rounds headless modes to multiples of 10.
        let f = fit((1910, 1070), (1918, 1078), 1.25).unwrap();
        assert_eq!(f.factor, 1.0);
        assert_eq!((f.x, f.y, f.width, f.height), (4, 4, 1910, 1070));
    }

    #[test]
    fn a_small_mirror_fills_the_window() {
        let f = fit((1280, 720), (1600, 900), 1.0).unwrap();
        assert_eq!((f.width, f.height), (1600, 900));
        assert_eq!(f.factor, 1.25);
    }

    #[test]
    fn doubles_when_there_is_room() {
        let f = fit((1280, 720), (2560, 1440), 2.0).unwrap();
        assert_eq!(f.factor, 2.0);
        assert_eq!((f.width, f.height), (2560, 1440));
    }

    #[test]
    fn letterboxes_and_shrinks() {
        let f = fit((2560, 1440), (1280, 1000), 1.0).unwrap();
        assert_eq!(f.factor, 0.5);
        assert_eq!((f.x, f.y, f.width, f.height), (0, 140, 1280, 720));
    }

    #[test]
    fn mode_step_matches_hyprland_rounding() {
        assert_eq!(mode_step(1.0), 2);
        assert_eq!(mode_step(1.25), 10);
        assert_eq!(mode_step(1.5), 6);
        assert_eq!(mode_step(2.0), 2);
        assert_eq!(mode_step(1.0 / 3.0), 2);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn mode_step_is_hypr_ipc_clean_mode() {
        for s in [0.5, 1.0, 1.125, 1.25, 1.5, 1.75, 2.0, 2.25, 2.5, 3.0, 3.875] {
            assert_eq!(mode_step(s), hypr_ipc::clean_mode(2, 2, s as f32).0, "{s}");
        }
    }

    #[test]
    fn maps_points_through_the_fit_and_remote_scale() {
        let f = fit((2560, 1440), (1280, 1000), 1.0).unwrap();
        // The centre of the drawn picture is the centre of the remote output.
        let (x, y) = f.to_remote((640, 500), (2560, 1440), (2560, 1440), 2.0);
        assert_eq!((x, y), (640.0, 360.0));
        // A reduced stream maps into its own pixels.
        let (x, y) = f.to_remote((640, 500), (2560, 1440), (1280, 720), 1.0);
        assert_eq!((x, y), (640.0, 360.0));
        // Points on the letterbox clamp to the edge.
        assert_eq!(
            f.to_remote((0, 0), (2560, 1440), (2560, 1440), 1.0),
            (0.0, 0.0)
        );
    }

    #[test]
    fn empty_inputs_have_no_fit() {
        assert!(fit((0, 0), (100, 100), 1.0).is_none());
        assert!(fit((10, 10), (0, 100), 1.0).is_none());
    }
}
