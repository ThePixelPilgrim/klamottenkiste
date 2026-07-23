// What:  Unit tests for the layout-aware character map.
// Why:   The whole point of D7 is that `us` and `de` disagree, so the tests compile BOTH
//        layouts and assert the disagreement concretely. No display, no seat, no client: just
//        libxkbcommon compiling RMLVO names in-process.
//
//        If libxkbcommon has no xkb data files installed, `build` returns None and these
//        tests fail loudly rather than silently passing — a machine that cannot compile a
//        keymap also cannot run the fixture correctly.

use super::LayoutKeymap;
use crate::xkb_host::HostXkb;

/// Build the map for a layout/variant pair, failing the test if it will not compile.
fn map_for(layout: &str, variant: &str) -> LayoutKeymap {
    let config = HostXkb {
        rules: String::new(),
        model: String::new(),
        layout: layout.to_string(),
        variant: variant.to_string(),
        options: String::new(),
    };
    LayoutKeymap::build(&config)
        .unwrap_or_else(|| panic!("xkb layout {layout:?}/{variant:?} failed to compile"))
}

#[test]
fn us_layout_maps_the_qwerty_letters() {
    let us = map_for("us", "");
    assert_eq!(us.char_to_key('y'), Some((21, false)));
    assert_eq!(us.char_to_key('z'), Some((44, false)));
    assert_eq!(us.char_to_key('a'), Some((30, false)));
    assert_eq!(us.char_to_key('-'), Some((12, false)));
}

#[test]
fn de_layout_swaps_y_and_z_against_us() {
    let us = map_for("us", "");
    let de = map_for("de", "nodeadkeys");

    // The QWERTZ swap: the physical key US calls `y` (evdev 21) types `z` on a German
    // keyboard and vice versa. This is exactly the bug D7 reported: `type yz` produced `zy`.
    assert_eq!(us.char_to_key('y'), Some((21, false)));
    assert_eq!(us.char_to_key('z'), Some((44, false)));
    assert_eq!(de.char_to_key('y'), Some((44, false)));
    assert_eq!(de.char_to_key('z'), Some((21, false)));

    // Letters that are not swapped must still agree, or something else is wrong.
    assert_eq!(de.char_to_key('a'), us.char_to_key('a'));
    assert_eq!(de.char_to_key('q'), us.char_to_key('q'));
}

#[test]
fn de_layout_puts_eszett_where_us_has_the_hyphen() {
    let us = map_for("us", "");
    let de = map_for("de", "nodeadkeys");

    // evdev 12 is KEY_MINUS. US types `-` there; German types `ß`.
    assert_eq!(us.char_to_key('-'), Some((12, false)));
    assert_eq!(de.char_to_key('\u{00df}'), Some((12, false)));

    // And German `-` moved: it is NOT on evdev 12 any more. (The reported symptom was
    // `type -` producing `ß`.)
    let (de_hyphen, de_hyphen_shift) = de
        .char_to_key('-')
        .expect("the de layout must be able to type a hyphen");
    assert_ne!(de_hyphen, 12, "de `-` must not be on the US hyphen key");
    assert!(!de_hyphen_shift, "de `-` is unshifted");
    assert_eq!(
        (de_hyphen, de_hyphen_shift),
        (53, false),
        "de `-` sits on the key US calls `/` (evdev 53)"
    );

    // `ß` is simply not typeable on US.
    assert_eq!(us.char_to_key('\u{00df}'), None);
}

#[test]
fn uppercase_needs_shift_in_both_layouts() {
    let us = map_for("us", "");
    let de = map_for("de", "nodeadkeys");

    assert_eq!(us.char_to_key('A'), Some((30, true)));
    assert_eq!(de.char_to_key('A'), Some((30, true)));

    // Uppercase sits on the same key as its lowercase form, with shift set.
    let (lower, lower_shift) = de.char_to_key('y').unwrap();
    let (upper, upper_shift) = de.char_to_key('Y').unwrap();
    assert_eq!(lower, upper);
    assert!(!lower_shift);
    assert!(upper_shift);
}

#[test]
fn de_umlauts_resolve_and_us_has_none() {
    let us = map_for("us", "");
    let de = map_for("de", "nodeadkeys");

    for umlaut in ['\u{00e4}', '\u{00f6}', '\u{00fc}'] {
        assert!(
            de.char_to_key(umlaut).is_some(),
            "de must be able to type {umlaut:?}"
        );
        assert_eq!(us.char_to_key(umlaut), None, "us must not have {umlaut:?}");
    }
}

#[test]
fn control_characters_are_not_in_the_map() {
    let us = map_for("us", "");
    // Return/Tab/Escape have character forms in xkb; mapping them would make `type` press
    // Enter mid-string. `key enter` is the supported way to do that.
    assert_eq!(us.char_to_key('\n'), None);
    assert_eq!(us.char_to_key('\r'), None);
    assert_eq!(us.char_to_key('\t'), None);
    assert_eq!(us.char_to_key('\u{1b}'), None);

    // Space, however, is a printable character and must be there.
    assert_eq!(us.char_to_key(' '), Some((57, false)));
}

#[test]
fn a_layout_that_does_not_exist_yields_no_map() {
    let config = HostXkb {
        layout: "definitely-not-a-layout".to_string(),
        ..HostXkb::default()
    };
    assert!(LayoutKeymap::build(&config).is_none());
}

#[test]
fn the_map_covers_a_realistic_alphabet() {
    let de = map_for("de", "nodeadkeys");
    for character in 'a'..='z' {
        assert!(
            de.char_to_key(character).is_some(),
            "de cannot type {character:?}"
        );
    }
    for character in '0'..='9' {
        assert!(
            de.char_to_key(character).is_some(),
            "de cannot type {character:?}"
        );
    }
    assert!(de.len() > 60, "suspiciously small map: {} entries", de.len());
    assert!(!de.is_empty());
}
