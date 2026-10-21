//! The XKB keymap that matches the active Windows keyboard layout, so the
//! remote types what the keycaps here say.
//!
//! Windows names a layout by its KLID (`00000407` is German). Each known one
//! maps to an XKB layout and variant; the keymap text sent to the server
//! only includes them by name, so the server's own XKB data (the same data
//! `setxkbmap` uses) resolves them. A layout without an entry falls back to
//! its language's base layout, then to US.

/// An XKB layout and variant, as `setxkbmap` takes them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct XkbLayout {
    pub layout: &'static str,
    pub variant: &'static str,
}

const US: XkbLayout = XkbLayout {
    layout: "us",
    variant: "",
};

/// Windows KLIDs (lower case) and their XKB equivalents.
const LAYOUTS: &[(&str, &str, &str)] = &[
    ("00000409", "us", ""),
    ("00020409", "us", "intl"),
    ("00010409", "us", "dvorak"),
    ("00000809", "gb", ""),
    ("00000452", "gb", "extd"),
    ("00001809", "ie", ""),
    ("00001009", "ca", ""),
    ("00000c0c", "ca", "fr-legacy"),
    ("00011009", "ca", "multix"),
    ("00000407", "de", ""),
    ("00000807", "ch", ""),
    ("0000100c", "ch", "fr"),
    ("0000040c", "fr", ""),
    ("0001040c", "fr", "afnor"),
    ("0002040c", "fr", "bepo_afnor"),
    ("0000080c", "be", ""),
    ("00000813", "be", ""),
    ("00000413", "nl", ""),
    ("0000040a", "es", ""),
    ("0001040a", "es", ""),
    ("0000080a", "latam", ""),
    ("00000410", "it", ""),
    ("00010410", "it", ""),
    ("00000816", "pt", ""),
    ("00000416", "br", ""),
    ("00010416", "br", ""),
    ("0000041d", "se", ""),
    ("00000414", "no", ""),
    ("00000406", "dk", ""),
    ("0000040b", "fi", ""),
    ("0000040f", "is", ""),
    ("00000425", "ee", ""),
    ("00000426", "lv", ""),
    ("00000427", "lt", ""),
    ("00000415", "pl", "qwertz"),
    ("00010415", "pl", ""),
    ("00000405", "cz", ""),
    ("00010405", "cz", "qwerty"),
    ("0000041b", "sk", ""),
    ("0001041b", "sk", "qwerty"),
    ("0000040e", "hu", ""),
    ("00000424", "si", ""),
    ("0000041a", "hr", ""),
    ("0000081a", "rs", "latin"),
    ("00000c1a", "rs", ""),
    ("00000418", "ro", "winkeys"),
    ("00010418", "ro", "std"),
    ("00000402", "bg", ""),
    ("00000408", "gr", ""),
    ("00000419", "ru", ""),
    ("00000422", "ua", ""),
    ("00020422", "ua", ""),
    ("00000423", "by", ""),
    ("0000041f", "tr", ""),
    ("0001041f", "tr", "f"),
    ("0000040d", "il", ""),
    ("00000401", "ara", ""),
    ("00000429", "ir", ""),
    ("0000041e", "th", ""),
    ("0000042a", "vn", ""),
    ("00000411", "jp", ""),
    ("00000412", "kr", ""),
    // Chinese input methods type through the US layout.
    ("00000804", "us", ""),
    ("00000404", "us", ""),
];

/// The XKB layout for a Windows KLID: the exact layout when known, else the
/// base layout of its language (the low four digits), else US.
pub fn xkb_layout(klid: &str) -> XkbLayout {
    let klid = klid.trim().to_ascii_lowercase();
    let find = |id: &str| {
        LAYOUTS
            .iter()
            .find(|(k, _, _)| *k == id)
            .map(|&(_, layout, variant)| XkbLayout { layout, variant })
    };
    if let Some(l) = find(&klid) {
        return l;
    }
    if klid.len() == 8 {
        if let Some(l) = find(&format!("0000{}", &klid[4..])) {
            return l;
        }
    }
    US
}

/// XKB keymap text (format v1) for `l`, built from includes the way
/// `setxkbmap -print` writes it.
pub fn keymap_text(l: XkbLayout) -> String {
    let symbols = if l.variant.is_empty() {
        l.layout.to_string()
    } else {
        format!("{}({})", l.layout, l.variant)
    };
    format!(
        "xkb_keymap {{\n\
         \txkb_keycodes  {{ include \"evdev+aliases(qwerty)\" }};\n\
         \txkb_types     {{ include \"complete\" }};\n\
         \txkb_compat    {{ include \"complete\" }};\n\
         \txkb_symbols   {{ include \"pc+{symbols}+inet(evdev)\" }};\n\
         \txkb_geometry  {{ include \"pc(pc105)\" }};\n\
         }};\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_layouts_map_exactly() {
        assert_eq!(xkb_layout("00000409"), US);
        assert_eq!(
            xkb_layout("00000407"),
            XkbLayout {
                layout: "de",
                variant: ""
            }
        );
        assert_eq!(
            xkb_layout("00010409"),
            XkbLayout {
                layout: "us",
                variant: "dvorak"
            }
        );
        // KLIDs come back from Windows in upper case.
        assert_eq!(xkb_layout("0000040C").layout, "fr");
    }

    #[test]
    fn unknown_variants_fall_back_to_their_language() {
        // A German variant without an entry is still German.
        assert_eq!(xkb_layout("00030407").layout, "de");
        // An unknown language is US, the server's own default.
        assert_eq!(xkb_layout("00000999"), US);
        assert_eq!(xkb_layout(""), US);
    }

    #[test]
    fn keymap_includes_the_layout_and_variant() {
        let text = keymap_text(XkbLayout {
            layout: "de",
            variant: "nodeadkeys",
        });
        assert!(text.starts_with("xkb_keymap {\n"));
        assert!(text.contains("include \"pc+de(nodeadkeys)+inet(evdev)\""));
        assert!(keymap_text(US).contains("include \"pc+us+inet(evdev)\""));
    }

    #[test]
    fn klids_are_unique_and_lower_case() {
        let mut seen = std::collections::HashSet::new();
        for (klid, _, _) in LAYOUTS {
            assert_eq!(klid.len(), 8);
            assert_eq!(*klid, klid.to_ascii_lowercase());
            assert!(seen.insert(klid), "{klid} listed twice");
        }
    }

    /// Every table entry must compile with the XKB data the server will use;
    /// a keymap that does not compile would end the session.
    #[cfg(target_os = "linux")]
    #[test]
    fn every_layout_compiles_with_xkbcommon() {
        for &(_, layout, variant) in LAYOUTS {
            let l = XkbLayout { layout, variant };
            let text = keymap_text(l);
            let escape = hypr_input::key_name(&text, 1);
            assert_eq!(escape.as_deref().ok(), Some("Escape"), "{l:?}");
        }
        // The layout really applies: the key right of T is Z on German keys.
        let de = keymap_text(xkb_layout("00000407"));
        assert_eq!(hypr_input::key_name(&de, 21).unwrap(), "z");
    }
}
