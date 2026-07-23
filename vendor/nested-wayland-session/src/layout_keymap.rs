//! Reverse character-to-keycode lookup built from the SAME xkb layout the seat advertises.
//!
//! `keymap::char_to_key` is a hardcoded US-QWERTY table. The seat, however, hands the client
//! the HOST's layout (see `xkb_host`), so on a `de` keyboard the control socket's
//! `type yz-QWERTZ` arrived as `zyßQWERTY`: the compositor pressed the US key for `y`, and
//! the client's `de` keymap dutifully turned that key into `z`.
//!
//! The fix is to ask the layout itself. libxkbcommon can compile the resolved RMLVO names
//! into a keymap and report, for every keycode and shift level, which keysyms that key
//! produces; inverting that mapping yields `char -> (evdev_code, needs_shift)` for whatever
//! layout is actually in force. Real hardware keys are unaffected: they already carry
//! hardware keycodes that the seat keymap interprets end-to-end.
//!
//! The map is built once (`host()`) and cached; compiling a keymap per typed character would
//! be absurd. `keymap::char_to_key` stays as the fallback for characters the layout cannot
//! produce at shift level 0 or 1.

use std::collections::HashMap;
use std::sync::OnceLock;

use smithay::input::keyboard::xkb;

use crate::xkb_host::{self, HostXkb};

/// Shift levels consulted per key: 0 is unmodified, 1 is Shift.
///
/// What:     `const LEVELS: [xkb::LevelIndex; 2] = [0, 1];`.
/// Why:      Synthetic typing can only hold Shift (see `input::type_text`); AltGr and the
///           higher levels would need modifier plumbing we do not have, so characters that
///           live up there are simply not in the map.
const LEVELS: [u32; 2] = [0, 1];

/// The keymap layout index the seat's fresh keyboard state uses.
///
/// What:     `const LAYOUT: xkb::LayoutIndex = 0;`.
/// Why:      A comma-separated `layout` list compiles to several groups, but the seat starts
///           in the first one and nothing in the fixture switches groups.
const LAYOUT: u32 = 0;

/// The offset between an xkb keycode and the Linux evdev code it wraps.
///
/// What:     `const EVDEV_OFFSET: u32 = 8;`.
/// Why:      xkb inherited X11's 8-key offset; `input::send_key` adds it back on the way out,
///           so the map must store the evdev code, not the xkb keycode.
const EVDEV_OFFSET: u32 = 8;

/// A character-to-key lookup derived from one compiled xkb keymap.
///
/// What:     `pub struct LayoutKeymap { by_char: HashMap<char, (u32, bool)> }`. Values are
///           `(evdev_code, needs_shift)`, the same shape `keymap::char_to_key` returns.
/// Why:      Lets `type_text` swap the US table for the live layout without changing its
///           press/release logic.
#[derive(Debug, Clone, Default)]
pub struct LayoutKeymap {
    /// Character to `(evdev_code, needs_shift)`.
    by_char: HashMap<char, (u32, bool)>,
}

impl LayoutKeymap {
    /// Compile the layout named by `config` and invert it into a character lookup.
    ///
    /// What:     `pub fn build(config: &HostXkb) -> Option<Self>`. `None` when libxkbcommon
    ///           cannot compile the names (unknown layout, missing xkb data files).
    /// Why:      The caller then keeps the old US table rather than typing nothing.
    pub fn build(config: &HostXkb) -> Option<Self> {
        // What:     A fresh xkb context, then `Keymap::new_from_names` with the resolved
        //           RMLVO names — exactly the call `smithay::input::keyboard::XkbConfig`
        //           makes internally when the seat's keyboard is created.
        // Why:      Same inputs, same compiler, same keymap: the map cannot drift from what
        //           the client was handed.
        let context = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let options = if config.options.is_empty() {
            None
        } else {
            Some(config.options.clone())
        };
        let keymap = xkb::Keymap::new_from_names(
            &context,
            &config.rules,
            &config.model,
            &config.layout,
            &config.variant,
            options,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )?;

        let mut by_char: HashMap<char, (u32, bool)> = HashMap::new();

        // What:     Walk every keycode the keymap defines, low to high, and within each key
        //           the unshifted level before the shifted one. `entry().or_insert()` keeps
        //           the first hit.
        // Why:      Several keys can produce the same character (the keypad duplicates the
        //           digits and the arithmetic symbols). Lowest keycode first makes the choice
        //           deterministic and picks the main block, which is where a human would type
        //           it.
        for raw in keymap.min_keycode().raw()..=keymap.max_keycode().raw() {
            // What:     `if raw < EVDEV_OFFSET { continue; }`.
            // Why:      Keycodes below the offset have no evdev counterpart; subtracting
            //           would wrap.
            if raw < EVDEV_OFFSET {
                continue;
            }
            let evdev = raw - EVDEV_OFFSET;
            let keycode = xkb::Keycode::new(raw);

            for level in LEVELS {
                // What:     `keymap.key_get_syms_by_level(keycode, LAYOUT, level)` returns the
                //           keysyms that key emits at that level, without needing a live
                //           keyboard state; take the first.
                // Why:      Multi-keysym levels are exotic (they exist for some IME setups)
                //           and the first symbol is the character the key stands for.
                let Some(keysym) = keymap
                    .key_get_syms_by_level(keycode, LAYOUT, level)
                    .first()
                    .copied()
                else {
                    continue;
                };

                // What:     `keysym.key_char()` applies the `xkb_keysym_to_utf32` mapping;
                //           control characters are dropped.
                // Why:      Return/Tab/Escape also have character forms (`\r`, `\t`, `\u{1b}`)
                //           and mapping them here would make `type` press Enter mid-string —
                //           a behaviour change nobody asked for. `key enter` remains the way
                //           to do that.
                let Some(character) = keysym.key_char() else {
                    continue;
                };
                if character.is_control() {
                    continue;
                }

                by_char.entry(character).or_insert((evdev, level == 1));
            }
        }

        // What:     `if by_char.is_empty() { return None; }`.
        // Why:      A keymap that yields no characters at all is not a usable layout; treat
        //           it like a compile failure so the US fallback takes over.
        if by_char.is_empty() {
            return None;
        }

        return Some(Self { by_char });
    }

    /// Look a character up, returning `(evdev_code, needs_shift)`.
    ///
    /// What:     `pub fn char_to_key(&self, character: char) -> Option<(u32, bool)>`. Mirrors
    ///           `keymap::char_to_key`'s signature.
    /// Why:      So `type_text` can try this first and fall through to the US table.
    pub fn char_to_key(&self, character: char) -> Option<(u32, bool)> {
        return self.by_char.get(&character).copied();
    }

    /// How many distinct characters the layout can produce with at most Shift.
    ///
    /// What:     `pub fn len(&self) -> usize`.
    /// Why:      Tests assert the map is non-trivial; also handy in a log line.
    pub fn len(&self) -> usize {
        return self.by_char.len();
    }

    /// Whether the map holds no characters.
    ///
    /// What:     `pub fn is_empty(&self) -> bool`.
    /// Why:      Clippy asks for it next to `len`, and `build` never returns an empty map.
    pub fn is_empty(&self) -> bool {
        return self.by_char.is_empty();
    }
}

/// Process-wide cache of the map for the HOST layout.
///
/// What:     `static HOST: OnceLock<Option<LayoutKeymap>>`. `None` records a compile failure
///           so it is not retried per character.
/// Why:      Compiling a keymap costs milliseconds and allocates; typing a paragraph must not
///           pay that per character.
static HOST: OnceLock<Option<LayoutKeymap>> = OnceLock::new();

/// The character map for the layout the seat advertises, or `None` if it would not compile.
///
/// What:     `pub fn host() -> Option<&'static LayoutKeymap>`. Resolves `xkb_host::resolve()`
///           on first call and caches the result forever.
/// Why:      `state::Compositor::new` builds the seat's keyboard from the very same
///           `xkb_host::resolve()` values, so this is the layout the client actually has.
pub fn host() -> Option<&'static LayoutKeymap> {
    return HOST.get_or_init(|| {
        let config = xkb_host::resolve();
        let built = LayoutKeymap::build(&config);
        if built.is_none() {
            tracing::warn!(
                "host xkb layout {:?} produced no character map; \
                 synthetic typing falls back to the US table",
                config.layout
            );
        }
        return built;
    })
    .as_ref();
}

/// What:     `#[cfg(test)] #[path = "layout_keymap_tests.rs"] mod tests;`. Declares the unit
///           tests from the sibling file.
/// Why:      Keep the layout assertions beside the code that builds them, as `keymap` does.
#[cfg(test)]
#[path = "layout_keymap_tests.rs"]
mod tests;
