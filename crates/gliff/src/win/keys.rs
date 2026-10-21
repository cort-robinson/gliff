//! Windows keys in the server's terms, and the release hotkey.
//!
//! The server types on an evdev keyboard, so each key is sent as the evdev
//! code of its physical position, from the PC/AT (set 1) scan code Windows
//! reports. That keeps every key where it is and leaves the meaning to the
//! XKB keymap sent alongside (see `layout`).
//!
//! The release hotkey is matched on virtual-key codes instead, which follow
//! the active layout the way GTK keyvals do on Linux.

use std::cell::RefCell;
use std::time::{Duration, Instant};

const VK_CANCEL: u32 = 0x03;
const VK_PAUSE: u32 = 0x13;
const VK_SNAPSHOT: u32 = 0x2C;
const VK_NUMLOCK: u32 = 0x90;

/// The evdev code for a key Windows reports as `scan` (with the E0
/// `extended` flag) and virtual key `vk`, or `None` for a key with no evdev
/// equivalent.
pub fn evdev_code(scan: u32, extended: bool, vk: u32) -> Option<u32> {
    // Pause and Num Lock share scan code 0x45 and report the extended flag
    // the opposite way round from every other key; the virtual key tells
    // them apart.
    match vk {
        VK_PAUSE | VK_CANCEL => return Some(119), // KEY_PAUSE (Ctrl+Pause is Break)
        VK_NUMLOCK => return Some(69),            // KEY_NUMLOCK
        VK_SNAPSHOT => return Some(99),           // KEY_SYSRQ (Print Screen)
        _ => {}
    }
    let scan = scan & 0xFF;
    let code = if extended {
        match scan {
            0x10 => 165, // KEY_PREVIOUSSONG
            0x19 => 163, // KEY_NEXTSONG
            0x1C => 96,  // KEY_KPENTER
            0x1D => 97,  // KEY_RIGHTCTRL
            0x20 => 113, // KEY_MUTE
            0x21 => 140, // KEY_CALC
            0x22 => 164, // KEY_PLAYPAUSE
            0x24 => 166, // KEY_STOPCD
            0x2E => 114, // KEY_VOLUMEDOWN
            0x30 => 115, // KEY_VOLUMEUP
            0x32 => 172, // KEY_HOMEPAGE
            0x35 => 98,  // KEY_KPSLASH
            // Right Shift has no E0 prefix, but the low-level hook reports
            // it extended anyway.
            0x36 => 54,  // KEY_RIGHTSHIFT
            0x37 => 99,  // KEY_SYSRQ
            0x38 => 100, // KEY_RIGHTALT
            0x47 => 102, // KEY_HOME
            0x48 => 103, // KEY_UP
            0x49 => 104, // KEY_PAGEUP
            0x4B => 105, // KEY_LEFT
            0x4D => 106, // KEY_RIGHT
            0x4F => 107, // KEY_END
            0x50 => 108, // KEY_DOWN
            0x51 => 109, // KEY_PAGEDOWN
            0x52 => 110, // KEY_INSERT
            0x53 => 111, // KEY_DELETE
            0x5B => 125, // KEY_LEFTMETA
            0x5C => 126, // KEY_RIGHTMETA
            0x5D => 127, // KEY_COMPOSE (Menu)
            0x5E => 116, // KEY_POWER
            0x5F => 142, // KEY_SLEEP
            0x63 => 143, // KEY_WAKEUP
            0x65 => 217, // KEY_SEARCH
            0x66 => 156, // KEY_BOOKMARKS
            0x67 => 173, // KEY_REFRESH
            0x68 => 128, // KEY_STOP
            0x69 => 159, // KEY_FORWARD
            0x6A => 158, // KEY_BACK
            0x6B => 157, // KEY_COMPUTER
            0x6C => 155, // KEY_MAIL
            0x6D => 226, // KEY_MEDIA
            _ => return None,
        }
    } else {
        match scan {
            // Escape through the keypad: evdev numbers these keys exactly
            // as set 1 does.
            0x01..=0x53 => scan,
            0x54 => 99,                         // KEY_SYSRQ (Alt+Print Screen)
            0x56 => 86,                         // KEY_102ND (ISO key left of Z)
            0x57 => 87,                         // KEY_F11
            0x58 => 88,                         // KEY_F12
            0x59 => 117,                        // KEY_KPEQUAL
            0x64..=0x6E => 183 + (scan - 0x64), // KEY_F13..KEY_F23
            0x70 => 93,                         // KEY_KATAKANAHIRAGANA
            0x71 => 123,                        // KEY_HANJA
            0x72 => 122,                        // KEY_HANGEUL
            0x73 => 89,                         // KEY_RO (ABNT C1, JIS ro)
            0x76 => 194,                        // KEY_F24
            0x79 => 92,                         // KEY_HENKAN
            0x7B => 94,                         // KEY_MUHENKAN
            0x7D => 124,                        // KEY_YEN
            0x7E => 121,                        // KEY_KPCOMMA (ABNT C2)
            _ => return None,
        }
    };
    Some(code)
}

/// Modifier bits for hotkey chords.
pub const CTRL: u8 = 1;
pub const ALT: u8 = 2;
pub const SHIFT: u8 = 4;
pub const WIN: u8 = 8;

/// The modifier a virtual key is, if any. The low-level hook reports the
/// left and right variants.
pub fn modifier_of(vk: u32) -> u8 {
    match vk {
        0x10 | 0xA0 | 0xA1 => SHIFT,
        0x11 | 0xA2 | 0xA3 => CTRL,
        0x12 | 0xA4 | 0xA5 => ALT,
        0x5B | 0x5C => WIN,
        _ => 0,
    }
}

/// How the user releases captured shortcuts back to Windows.
#[derive(Clone, Debug, PartialEq)]
pub enum Hotkey {
    None,
    DoubleTap { vk: u32, within: Duration },
    Chord { mods: u8, vk: u32 },
}

impl Hotkey {
    /// Same forms as the Linux client: `shift+escape`, `ctrl+alt+q`,
    /// `double-escape`, or `none`.
    pub fn parse(spec: &str) -> Result<Self, String> {
        let s = spec.trim();
        if s.is_empty() || s.eq_ignore_ascii_case("none") {
            return Ok(Self::None);
        }
        let lower = s.to_ascii_lowercase();
        if let Some(rest) = lower
            .strip_prefix("double-")
            .or_else(|| lower.strip_prefix("double:"))
        {
            return Ok(Self::DoubleTap {
                vk: vk_from_name(rest)?,
                within: Duration::from_millis(400),
            });
        }
        let mut mods = 0;
        let mut vk = None;
        for tok in lower.split('+') {
            match tok.trim() {
                "" => {}
                "ctrl" | "control" => mods |= CTRL,
                "alt" => mods |= ALT,
                "shift" => mods |= SHIFT,
                "super" | "logo" | "win" | "meta" => mods |= WIN,
                other => vk = Some(vk_from_name(other)?),
            }
        }
        match vk {
            Some(vk) => Ok(Self::Chord { mods, vk }),
            None => Err(format!("no key in release hotkey '{spec}'")),
        }
    }

    /// True if pressing `vk` with `mods` held is the release trigger. For a
    /// double-tap it records the tap and is true only on the quick second
    /// press, so the first tap still reaches the remote.
    pub fn matches(&self, vk: u32, mods: u8, last_tap: &RefCell<Option<Instant>>) -> bool {
        match self {
            Self::None => false,
            Self::Chord { mods: m, vk: k } => vk == *k && mods == *m,
            Self::DoubleTap { vk: k, within } => {
                if vk != *k {
                    return false;
                }
                let now = Instant::now();
                let mut lt = last_tap.borrow_mut();
                match *lt {
                    Some(prev) if now.duration_since(prev) <= *within => {
                        *lt = None;
                        true
                    }
                    _ => {
                        *lt = Some(now);
                        false
                    }
                }
            }
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::None => "release disabled".into(),
            Self::DoubleTap { vk, .. } => format!("double-tap {}", vk_name(*vk)),
            Self::Chord { mods, vk } => {
                let mut parts = Vec::new();
                for (bit, name) in [(CTRL, "Ctrl"), (ALT, "Alt"), (SHIFT, "Shift"), (WIN, "Win")] {
                    if mods & bit != 0 {
                        parts.push(name.to_string());
                    }
                }
                parts.push(vk_name(*vk));
                parts.join("+")
            }
        }
    }
}

const NAMED_KEYS: &[(&str, u32)] = &[
    ("esc", 0x1B),
    ("escape", 0x1B),
    ("enter", 0x0D),
    ("return", 0x0D),
    ("space", 0x20),
    ("tab", 0x09),
    ("backspace", 0x08),
    ("insert", 0x2D),
    ("delete", 0x2E),
    ("home", 0x24),
    ("end", 0x23),
    ("pageup", 0x21),
    ("pagedown", 0x22),
    ("left", 0x25),
    ("up", 0x26),
    ("right", 0x27),
    ("down", 0x28),
    ("pause", 0x13),
    ("scrolllock", 0x91),
    ("grave", 0xC0),
];

fn vk_from_name(name: &str) -> Result<u32, String> {
    let n = name.trim().to_ascii_lowercase();
    if let Some((_, vk)) = NAMED_KEYS.iter().find(|(k, _)| *k == n) {
        return Ok(*vk);
    }
    let mut chars = n.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphanumeric() {
            return Ok(c.to_ascii_uppercase() as u32);
        }
    }
    if let Some(f) = n.strip_prefix('f').and_then(|d| d.parse::<u32>().ok()) {
        if (1..=24).contains(&f) {
            return Ok(0x70 + f - 1);
        }
    }
    Err(format!("unknown key '{name}'"))
}

fn vk_name(vk: u32) -> String {
    match vk {
        0x1B => "Esc".into(),
        0x41..=0x5A | 0x30..=0x39 => char::from_u32(vk).map_or("?".into(), String::from),
        0x70..=0x87 => format!("F{}", vk - 0x70 + 1),
        _ => NAMED_KEYS
            .iter()
            .find(|(_, v)| *v == vk)
            .map(|(k, _)| {
                let mut c = k.chars();
                c.next()
                    .map(|f| f.to_uppercase().collect::<String>() + c.as_str())
                    .unwrap_or_default()
            })
            .unwrap_or_else(|| format!("0x{vk:02X}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn main_block_keeps_set_one_numbering() {
        assert_eq!(evdev_code(0x01, false, 0x1B), Some(1)); // Esc
        assert_eq!(evdev_code(0x10, false, 0x51), Some(16)); // Q
        assert_eq!(evdev_code(0x1D, false, 0xA2), Some(29)); // left Ctrl
        assert_eq!(evdev_code(0x2A, false, 0xA0), Some(42)); // left Shift
        assert_eq!(evdev_code(0x36, false, 0xA1), Some(54)); // right Shift
        assert_eq!(evdev_code(0x38, false, 0xA4), Some(56)); // left Alt
        assert_eq!(evdev_code(0x57, false, 0x7A), Some(87)); // F11
        assert_eq!(evdev_code(0x56, false, 0xE2), Some(86)); // ISO <>
    }

    #[test]
    fn extended_keys_use_their_own_codes() {
        assert_eq!(evdev_code(0x1D, true, 0xA3), Some(97)); // right Ctrl
        assert_eq!(evdev_code(0x36, true, 0xA1), Some(54)); // right Shift, as the hook reports it
        assert_eq!(evdev_code(0x38, true, 0xA5), Some(100)); // right Alt / AltGr
        assert_eq!(evdev_code(0x5B, true, 0x5B), Some(125)); // left Win
        assert_eq!(evdev_code(0x48, true, 0x26), Some(103)); // Up
        assert_eq!(evdev_code(0x53, true, 0x2E), Some(111)); // Delete
        assert_eq!(evdev_code(0x1C, true, 0x0D), Some(96)); // keypad Enter
                                                            // The same scan codes without E0 are the keypad.
        assert_eq!(evdev_code(0x48, false, 0x68), Some(72));
        assert_eq!(evdev_code(0x53, false, 0x6E), Some(83));
    }

    #[test]
    fn pause_and_num_lock_follow_the_virtual_key() {
        assert_eq!(evdev_code(0x45, false, VK_PAUSE), Some(119));
        assert_eq!(evdev_code(0x45, true, VK_NUMLOCK), Some(69));
        assert_eq!(evdev_code(0x37, true, VK_SNAPSHOT), Some(99));
    }

    #[test]
    fn function_keys_past_twelve() {
        assert_eq!(evdev_code(0x64, false, 0x7C), Some(183)); // F13
        assert_eq!(evdev_code(0x6E, false, 0x86), Some(193)); // F23
        assert_eq!(evdev_code(0x76, false, 0x87), Some(194)); // F24
    }

    #[test]
    fn unknown_scan_codes_are_dropped() {
        assert_eq!(evdev_code(0x00, false, 0), None);
        assert_eq!(evdev_code(0x7F, true, 0), None);
    }

    #[test]
    fn parses_the_linux_client_forms() {
        assert_eq!(Hotkey::parse("none").unwrap(), Hotkey::None);
        assert_eq!(Hotkey::parse("").unwrap(), Hotkey::None);
        assert_eq!(
            Hotkey::parse("shift+escape").unwrap(),
            Hotkey::Chord {
                mods: SHIFT,
                vk: 0x1B
            }
        );
        assert_eq!(
            Hotkey::parse("Ctrl+Alt+q").unwrap(),
            Hotkey::Chord {
                mods: CTRL | ALT,
                vk: 0x51
            }
        );
        assert_eq!(
            Hotkey::parse("super+F12").unwrap(),
            Hotkey::Chord {
                mods: WIN,
                vk: 0x7B
            }
        );
        assert!(matches!(
            Hotkey::parse("double-escape").unwrap(),
            Hotkey::DoubleTap { vk: 0x1B, .. }
        ));
        assert!(Hotkey::parse("ctrl+alt").is_err());
        assert!(Hotkey::parse("ctrl+nosuchkey").is_err());
    }

    #[test]
    fn chord_needs_exact_modifiers() {
        let hk = Hotkey::parse("shift+escape").unwrap();
        let lt = RefCell::new(None);
        assert!(hk.matches(0x1B, SHIFT, &lt));
        assert!(!hk.matches(0x1B, 0, &lt));
        assert!(!hk.matches(0x1B, SHIFT | CTRL, &lt));
    }

    #[test]
    fn double_tap_needs_two_quick_presses() {
        let hk = Hotkey::parse("double-escape").unwrap();
        let lt = RefCell::new(None);
        assert!(!hk.matches(0x1B, 0, &lt));
        assert!(hk.matches(0x1B, 0, &lt));
        assert!(!hk.matches(0x1B, 0, &lt));
    }

    #[test]
    fn describes_hotkeys_for_the_hint() {
        assert_eq!(
            Hotkey::parse("shift+escape").unwrap().describe(),
            "Shift+Esc"
        );
        assert_eq!(
            Hotkey::parse("ctrl+alt+q").unwrap().describe(),
            "Ctrl+Alt+Q"
        );
        assert_eq!(
            Hotkey::parse("double-escape").unwrap().describe(),
            "double-tap Esc"
        );
        assert_eq!(
            Hotkey::parse("ctrl+pagedown").unwrap().describe(),
            "Ctrl+Pagedown"
        );
    }

    #[test]
    fn modifiers_cover_both_sides() {
        assert_eq!(modifier_of(0xA0), SHIFT);
        assert_eq!(modifier_of(0xA3), CTRL);
        assert_eq!(modifier_of(0xA5), ALT);
        assert_eq!(modifier_of(0x5C), WIN);
        assert_eq!(modifier_of(0x41), 0);
    }
}
