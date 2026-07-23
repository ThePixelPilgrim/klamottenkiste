//! Resolve the HOST's keyboard layout so the nested seat hands clients the same keymap.
//!
//! Smithay's `XkbConfig::default()` is a US layout. A nested compositor that advertises US
//! to its client while the user types on a `de` keyboard produces wrong characters for
//! everything that differs (y/z, umlauts, most symbols), which is a chronic papercut rather
//! than a bug you can point at.
//!
//! Resolution order, first match wins:
//!
//! 1. `KABELSALAT_XKB_{RULES,MODEL,LAYOUT,VARIANT,OPTIONS}` — explicit override.
//! 2. `XKB_DEFAULT_{RULES,MODEL,LAYOUT,VARIANT,OPTIONS}` — the standard libxkbcommon env.
//! 3. `/etc/X11/xorg.conf.d/00-keyboard.conf` — written by `systemd-localed(8)`; the
//!    canonical system layout on systemd distros, and the only one populated on the target
//!    machine (the `XKB_DEFAULT_*` vars are unset there, so relying on env alone silently
//!    leaves the seat on US).
//! 4. Empty strings — libxkbcommon then applies its own default.
//!
//! Not yet handled: the ideal fix is to forward the host compositor's keymap verbatim (our
//! GTK host is itself a Wayland client and receives one over `wl_keyboard.keymap`), which
//! would also carry IME/variant nuances. See the Phase 0 plan finding D6.

use std::{env, fs};

/// A resolved xkb configuration, owned so it can outlive the parsing step.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HostXkb {
    /// xkb rules set (usually empty → libxkbcommon default).
    pub rules: String,
    /// Keyboard model, e.g. `pc105`.
    pub model: String,
    /// Layout, e.g. `de`.
    pub layout: String,
    /// Variant, e.g. `nodeadkeys`.
    pub variant: String,
    /// Options, e.g. `terminate:ctrl_alt_bksp`. Empty means "none".
    pub options: String,
}

impl HostXkb {
    /// True when nothing was resolved and libxkbcommon should pick its own default.
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
            && self.model.is_empty()
            && self.layout.is_empty()
            && self.variant.is_empty()
            && self.options.is_empty()
    }
}

/// Path to the file `systemd-localed` writes the system keyboard layout into.
const LOCALED_KEYBOARD_CONF: &str = "/etc/X11/xorg.conf.d/00-keyboard.conf";

/// Resolve the host keyboard layout using the documented precedence.
pub fn resolve() -> HostXkb {
    let from_env = |kabelsalat: &str, standard: &str| -> String {
        env::var(kabelsalat)
            .or_else(|_| env::var(standard))
            .unwrap_or_default()
    };

    let mut config = HostXkb {
        rules: from_env("KABELSALAT_XKB_RULES", "XKB_DEFAULT_RULES"),
        model: from_env("KABELSALAT_XKB_MODEL", "XKB_DEFAULT_MODEL"),
        layout: from_env("KABELSALAT_XKB_LAYOUT", "XKB_DEFAULT_LAYOUT"),
        variant: from_env("KABELSALAT_XKB_VARIANT", "XKB_DEFAULT_VARIANT"),
        options: from_env("KABELSALAT_XKB_OPTIONS", "XKB_DEFAULT_OPTIONS"),
    };

    // Only consult the system file when the environment said nothing about the layout —
    // an explicit env layout should not be half-overridden by system defaults.
    if config.layout.is_empty() {
        if let Ok(text) = fs::read_to_string(LOCALED_KEYBOARD_CONF) {
            let parsed = parse_localed_keyboard_conf(&text);
            if config.rules.is_empty() {
                config.rules = parsed.rules;
            }
            if config.model.is_empty() {
                config.model = parsed.model;
            }
            config.layout = parsed.layout;
            if config.variant.is_empty() {
                config.variant = parsed.variant;
            }
            if config.options.is_empty() {
                config.options = parsed.options;
            }
        }
    }

    config
}

/// Parse the `Option "XkbLayout" "de"` lines out of a systemd-localed keyboard config.
///
/// The file is an Xorg `InputClass` section; only the `Xkb*` options matter here. Unknown
/// options and comments are ignored, and quoting is the simple two-token form systemd emits.
pub fn parse_localed_keyboard_conf(text: &str) -> HostXkb {
    let mut config = HostXkb::default();

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with("Option") {
            continue;
        }

        // Collect the quoted tokens: Option "XkbLayout" "de" → ["XkbLayout", "de"].
        let quoted: Vec<&str> = line.split('"').skip(1).step_by(2).collect();
        let [key, value] = quoted.as_slice() else {
            continue;
        };

        match key.to_ascii_lowercase().as_str() {
            "xkbrules" => config.rules = (*value).to_string(),
            "xkbmodel" => config.model = (*value).to_string(),
            "xkblayout" => config.layout = (*value).to_string(),
            "xkbvariant" => config.variant = (*value).to_string(),
            "xkboptions" => config.options = (*value).to_string(),
            _ => {}
        }
    }

    config
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact shape systemd-localed writes on the target machine.
    const SYSTEMD_SAMPLE: &str = r#"
# Written by systemd-localed(8), read by systemd-localed and Xorg. It's
# probably wise not to edit this file manually. Use localectl(1) to
# instruct systemd-localed to update it.
Section "InputClass"
        Identifier "system-keyboard"
        MatchIsKeyboard "on"
        Option "XkbLayout" "de"
        Option "XkbModel" "pc105"
        Option "XkbVariant" "nodeadkeys"
EndSection
"#;

    #[test]
    fn parses_the_systemd_localed_layout() {
        let config = parse_localed_keyboard_conf(SYSTEMD_SAMPLE);
        assert_eq!(config.layout, "de");
        assert_eq!(config.model, "pc105");
        assert_eq!(config.variant, "nodeadkeys");
        assert_eq!(config.options, "");
        assert_eq!(config.rules, "");
    }

    #[test]
    fn ignores_non_xkb_options_and_comments() {
        let config = parse_localed_keyboard_conf(
            "# Option \"XkbLayout\" \"fr\"\nOption \"SomethingElse\" \"x\"\n",
        );
        // The commented line still starts with '#', not "Option", so nothing is picked up.
        assert!(config.is_empty(), "unexpected {config:?}");
    }

    #[test]
    fn parses_options_when_present() {
        let config =
            parse_localed_keyboard_conf("Option \"XkbOptions\" \"terminate:ctrl_alt_bksp\"\n");
        assert_eq!(config.options, "terminate:ctrl_alt_bksp");
    }

    #[test]
    fn empty_input_is_empty_config() {
        assert!(parse_localed_keyboard_conf("").is_empty());
    }
}
