//! Translation from Slint key events (which only carry text) to xkb keycodes.
//!
//! Wayland clients want keycodes plus a keymap, Slint gives the text a key press produced.
//! So the keymap sent to clients is searched for a key that produces that text.

use std::collections::HashMap;

use smithay::input::keyboard::{xkb, Keycode};

/// Where a keysym lives in the keymap.
#[derive(Clone, Copy, Debug)]
pub struct KeyLocation {
    pub keycode: Keycode,
    /// Shift level: 0 is the plain key, 1 is the key with Shift.
    pub level: u32,
}

pub struct Keymap {
    by_keysym: HashMap<u32, KeyLocation>,
}

/// A Slint key event resolved against the keymap.
pub struct ResolvedKey {
    pub location: KeyLocation,
    /// The key produces a character, as opposed to keys like Return or the arrows.
    /// Only for those the shift level of the keymap has to be enforced.
    pub printable: bool,
}

impl Keymap {
    /// Compiles the same default keymap smithay sends to clients for `XkbConfig::default()`.
    pub fn new() -> Option<Self> {
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_names(&context, "", "", "", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)?;

        let mut by_keysym = HashMap::new();
        let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
        // Lower levels first, so that a keysym reachable without Shift wins.
        for level in 0..4 {
            for raw in min..=max {
                let keycode = Keycode::new(raw);
                if level >= keymap.num_levels_for_key(keycode, 0) {
                    continue;
                }
                for sym in keymap.key_get_syms_by_level(keycode, 0, level) {
                    by_keysym.entry(sym.raw()).or_insert(KeyLocation { keycode, level });
                }
            }
        }
        Some(Self { by_keysym })
    }

    pub fn is_shift(&self, keycode: Keycode) -> bool {
        [xkb::keysyms::KEY_Shift_L, xkb::keysyms::KEY_Shift_R]
            .iter()
            .any(|sym| self.by_keysym.get(sym).is_some_and(|l| l.keycode == keycode))
    }

    pub fn shift(&self) -> Option<Keycode> {
        self.by_keysym.get(&xkb::keysyms::KEY_Shift_L).map(|l| l.keycode)
    }

    /// Resolves the `text` of a Slint `KeyEvent`.
    pub fn resolve(&self, text: &str) -> Option<ResolvedKey> {
        let mut chars = text.chars();
        let ch = chars.next()?;
        if chars.next().is_some() {
            // Composed text (for example from an input method) has no key.
            return None;
        }
        let (keysym, printable) = match special_keysym(ch) {
            Some(keysym) => (keysym, false),
            // Control+letter arrives as a control character on some backends.
            None if (ch as u32) < 0x20 => (xkb::utf32_to_keysym(ch as u32 + 0x60).raw(), false),
            None => (xkb::utf32_to_keysym(ch as u32).raw(), true),
        };
        let location = *self.by_keysym.get(&keysym)?;
        Some(ResolvedKey { location, printable })
    }
}

/// Keysyms of the keys Slint reports as special characters (see `slint::platform::Key`).
fn special_keysym(ch: char) -> Option<u32> {
    use xkb::keysyms::*;
    Some(match ch {
        '\u{0008}' => KEY_BackSpace,
        '\u{0009}' => KEY_Tab,
        '\u{000a}' | '\u{000d}' => KEY_Return,
        '\u{001b}' => KEY_Escape,
        '\u{0019}' => KEY_ISO_Left_Tab,
        '\u{007f}' => KEY_Delete,
        '\u{0010}' => KEY_Shift_L,
        '\u{0011}' => KEY_Control_L,
        '\u{0012}' => KEY_Alt_L,
        '\u{0013}' => KEY_ISO_Level3_Shift,
        '\u{0014}' => KEY_Caps_Lock,
        '\u{0015}' => KEY_Shift_R,
        '\u{0016}' => KEY_Control_R,
        '\u{0017}' => KEY_Super_L,
        '\u{0018}' => KEY_Super_R,
        '\u{F700}' => KEY_Up,
        '\u{F701}' => KEY_Down,
        '\u{F702}' => KEY_Left,
        '\u{F703}' => KEY_Right,
        '\u{F704}'..='\u{F71B}' => KEY_F1 + (ch as u32 - 0xF704),
        '\u{F727}' => KEY_Insert,
        '\u{F729}' => KEY_Home,
        '\u{F72B}' => KEY_End,
        '\u{F72C}' => KEY_Page_Up,
        '\u{F72D}' => KEY_Page_Down,
        '\u{F72F}' => KEY_Scroll_Lock,
        '\u{F730}' => KEY_Pause,
        '\u{F731}' => KEY_Print,
        '\u{F735}' => KEY_Menu,
        _ => return None,
    })
}
