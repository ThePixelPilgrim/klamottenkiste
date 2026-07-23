//! Synthetic pointer and keyboard input injected through the compositor's own seat.
//!
//! Because the fixture owns the seat, injecting input is a direct in-process call into
//! the seat's pointer and keyboard handles: no `/dev/uinput`, no global input, and the
//! events reach only the hosted client. Coordinates are logical; keys are evdev codes
//! (translated to the xkb keycode system by adding the 8-offset winit also applies).

/// What:     Grouped `use` of the input state enums, keyboard focus/keycode types, the
///           pointer event structs, and the coordinate/serial utilities.
/// Why:      Everything the injection functions reference.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// import { ButtonState, KeyState, Keycode, MotionEvent, ButtonEvent, ... } from "smithay";
/// ```
use smithay::{
    backend::input::{Axis, AxisSource, ButtonState, KeyState},
    input::{
        keyboard::{FilterResult, Keycode},
        pointer::{AxisFrame, ButtonEvent, MotionEvent},
    },
    utils::{Logical, Point, SERIAL_COUNTER},
};

/// What:     `use crate::{keymap, protocol::{KeyAction, PointerButton}, state::Compositor};`.
/// Why:      Injection reads the keymap tables and the protocol's button/action enums,
///           and operates on the compositor state.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// import * as keymap from "./keymap";
/// import { KeyAction, PointerButton } from "./protocol";
/// import { Compositor } from "./state";
/// ```
use crate::{
    keymap, layout_keymap,
    protocol::{KeyAction, PointerButton},
    state::Compositor,
};

/// Milliseconds since program start, used as the event timestamp.
///
/// What:     `fn event_time(state: &Compositor) -> u32`. Read-only borrow; returns a
///           32-bit millisecond count. `.as_millis()` yields a 128-bit integer, cast to
///           `u32` (Wayland event times are 32-bit and wrap, which clients tolerate).
/// Why:      Every synthetic event needs a monotonic-ish timestamp.
fn event_time(state: &Compositor) -> u32 {
    // What:     `state.start_time.elapsed().as_millis() as u32`. Elapsed time, in ms,
    //           narrowed to `u32`. Tail expression.
    // Why:      Provide the event timestamp.
    state.start_time.elapsed().as_millis() as u32
}

/// Click a button at a logical point: move the pointer there, press, and release.
///
/// What:     `pub fn click(state: &mut Compositor, x: f64, y: f64, button:
///           PointerButton)`. Mutably borrows the state; `x`/`y` are logical
///           coordinates.
/// Why:      The `click` control command lands a full press+release at a spot.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// function click(state, x, y, button) { ... }
/// ```
///
/// @example
/// ```ts
/// click(state, 100, 40, PointerButton.Left);
/// ```
pub fn click(state: &mut Compositor, x: f64, y: f64, button: PointerButton) {
    // What:     Delegate both halves of the click to `pointer_button`, the same function
    //           the GTK pointer controller feeds.
    // Why:      `pointer_button` carries the click-to-focus rule (a press moves keyboard
    //           focus to the toplevel under the cursor). Open-coding the motion/press/
    //           release here meant a control-socket `click` focused nothing, so a
    //           subsequent `type` or `key` went to whatever had focus before — which is
    //           nothing at all once the host window has lost focus. One code path, one
    //           behaviour.
    let code = button.evdev_code();

    pointer_button(state, x, y, code, true);
    pointer_button(state, x, y, code, false);
}

pub fn key(state: &mut Compositor, evdev: u32, action: KeyAction) {
    // What:     `match action { ... }`. Press and release map to one key event each; tap
    //           is a press followed by a release.
    // Why:      Cover holding, releasing, and tapping.
    match action {
        KeyAction::Press => send_key(state, evdev, KeyState::Pressed),
        KeyAction::Release => send_key(state, evdev, KeyState::Released),
        KeyAction::Tap => {
            // What:     Two sequential calls: press then release.
            // Why:      A tap is a momentary key press.
            send_key(state, evdev, KeyState::Pressed);
            send_key(state, evdev, KeyState::Released);
        }
    }
}

/// Type a run of text as a sequence of key taps, holding Shift where needed.
///
/// What:     `pub fn type_text(state: &mut Compositor, text: &str)`. Iterates the
///           characters and taps each; characters the layout cannot reach are skipped.
/// Why:      The `type` control command feeds a string into the focused input.
///
/// Each character is resolved against the SAME xkb layout the seat advertises to the client
/// (`layout_keymap::host()`), not the hardcoded US table — finding D7. Pressing the US key
/// for `y` on a seat holding a German keymap made the client type `z`; asking the layout
/// which key produces `y` fixes that for every layout at once. `keymap::char_to_key` stays as
/// the fallback, so a character the layout cannot produce behaves exactly as before.
///
/// In TS you'd write (pseudocode):
/// ```ts
/// function typeText(state, text) { ... }
/// ```
///
/// @example
/// ```ts
/// typeText(state, "Hello!");
/// ```
pub fn type_text(state: &mut Compositor, text: &str) {
    // What:     `let layout = layout_keymap::host();`. The cached character map for the
    //           resolved host layout, or `None` if that layout would not compile.
    // Why:      Looked up once for the whole string; building it per character would compile
    //           an xkb keymap per keystroke.
    let layout = layout_keymap::host();

    // What:     `for character in text.chars() { ... }`. Iterate Unicode scalar values.
    // Why:      Type one character at a time.
    for character in text.chars() {
        // What:     Try the layout map first, then the US table via `or`; skip the character
        //           when neither knows it.
        // Why:      The layout map is the truth for the keymap the client holds; the US table
        //           only fills gaps, so behaviour can never get worse than before D7.
        let from_layout = match layout {
            Some(map) => map.char_to_key(character),
            None => None,
        };
        let Some((evdev, shift)) = from_layout.or(keymap::char_to_key(character)) else {
            continue;
        };

        // What:     `if shift { send_key(state, keymap::LEFT_SHIFT, KeyState::Pressed); }`.
        //           Hold Shift for shifted characters.
        // Why:      Uppercase and symbol characters need Shift down.
        if shift {
            send_key(state, keymap::LEFT_SHIFT, KeyState::Pressed);
        }

        // What:     `send_key(state, evdev, KeyState::Pressed);` then `...Released`. Tap the
        //           character key.
        // Why:      Produce the character.
        send_key(state, evdev, KeyState::Pressed);
        send_key(state, evdev, KeyState::Released);

        // What:     `if shift { send_key(state, keymap::LEFT_SHIFT, KeyState::Released); }`.
        //           Release Shift after the shifted character.
        // Why:      Do not leave Shift stuck down.
        if shift {
            send_key(state, keymap::LEFT_SHIFT, KeyState::Released);
        }
    }
}

/// Send one keyboard key event (press or release) through the seat's keyboard.
///
/// What:     `fn send_key(state: &mut Compositor, evdev: u32, key_state: KeyState)`.
///           Private helper.
/// Why:      Both `key` and `type_text` funnel through one place that does the 8-offset
///           and the seat call.
pub fn send_key(state: &mut Compositor, evdev: u32, key_state: KeyState) {
    // What:     `let keyboard = state.seat.get_keyboard().unwrap();`. The keyboard handle
    //           (a reference-counted clone, not a borrow of the seat).
    // Why:      Need it to inject the key.
    let keyboard = state.seat.get_keyboard().unwrap();

    // What:     `let time = event_time(state);`. Timestamp before the mutable seat call.
    // Why:      The event needs a time and this borrow must end before the `&mut state`
    //           call below.
    let time = event_time(state);

    // What:     `let keycode: Keycode = (evdev + 8).into();`. Convert the evdev code to the
    //           xkb keycode by adding 8 (the X11 keycode offset winit's backend also
    //           applies) and `into()`-ing it to `Keycode`.
    // Why:      Smithay's keyboard state is keyed by xkb keycodes, not raw evdev codes.
    let keycode: Keycode = (evdev + 8).into();

    // What:     `keyboard.input::<(), _>(state, keycode, key_state,
    //           SERIAL_COUNTER.next_serial(), time, |_, _, _| FilterResult::Forward);`. Feed
    //           the event. The turbofish `::<(), _>` sets the filter's return payload type
    //           to `()` and infers the closure type. The filter `|_, _, _|
    //           FilterResult::Forward` always forwards the key to the focused client (no
    //           compositor shortcut handling).
    // Why:      Deliver the synthetic key to the hosted app.
    keyboard.input::<(), _>(
        state,
        keycode,
        key_state,
        SERIAL_COUNTER.next_serial(),
        time,
        |_, _, _| FilterResult::Forward,
    );
}

/// One real GTK input event, translated into compositor-output coordinates / evdev codes.
///
/// What:     `pub enum SpikeInput { Motion, Button, Scroll, Key, Text, Focus }`. A tagged
///           union carried over a `crossbeam-channel` from the GTK main thread to the
///           compositor thread. `Motion`/`Button` coordinates are already mapped into
///           compositor OUTPUT pixels (the GTK host inverts its `ContentFit::Contain`
///           letterbox before enqueuing); `button`/`evdev` are raw Linux evdev codes.
/// Why:      The GTK controllers cannot touch the seat directly (it lives on the
///           compositor thread), so each event is queued and applied inside the loop via
///           the same `input.rs` seat-synthesis path the control socket uses.
#[derive(Debug, Clone)]
pub enum SpikeInput {
    /// Pointer moved to an output-pixel position (no button).
    Motion {
        /// Output-pixel x.
        x: f64,
        /// Output-pixel y.
        y: f64,
    },
    /// Pointer button pressed or released at an output-pixel position.
    Button {
        /// Output-pixel x.
        x: f64,
        /// Output-pixel y.
        y: f64,
        /// Raw evdev button code (`BTN_LEFT`=0x110, `BTN_RIGHT`=0x111, `BTN_MIDDLE`=0x112).
        button: u32,
        /// `true` = press, `false` = release.
        pressed: bool,
    },
    /// Scroll wheel / trackpad axis delta (GTK units; positive dy scrolls down).
    Scroll {
        /// Horizontal delta.
        dx: f64,
        /// Vertical delta.
        dy: f64,
    },
    /// Keyboard key pressed or released, as a raw evdev keycode.
    Key {
        /// Raw evdev keycode (xkb keycode minus 8).
        evdev: u32,
        /// `true` = press, `false` = release.
        pressed: bool,
    },
    /// Type a run of text as individual key taps (resolved against the seat's xkb layout).
    Text(
        /// Text to type.
        String,
    ),
    /// Keyboard focus entered (`true`) or left (`false`) the GTK pane.
    Focus(
        /// Whether the pane now holds focus.
        bool,
    ),
    /// Resize the nested output to this many DEVICE pixels.
    ///
    /// Sent by the embedding host when its pane allocation (or scale factor) changed, so
    /// the hosted app re-lays-out for the real pane instead of being scaled from a fixed
    /// resolution. Applied through `control::resize_output`, the same function the
    /// control socket's `Resize` command uses.
    Resize {
        /// New output width in device pixels.
        width: i32,
        /// New output height in device pixels.
        height: i32,
    },
}

/// Rough logical-pixel travel per unit of GTK scroll delta (one wheel notch ≈ 1.0).
///
/// What:     `const SCROLL_STEP: f64 = 15.0;`.
/// Why:      GTK reports one wheel notch as a delta of ~1.0; clients that read the
///           continuous axis value expect a pixels-ish magnitude, so scale up.
const SCROLL_STEP: f64 = 15.0;

/// Move the pointer to a logical point (setting pointer focus) without any button.
///
/// What:     `pub fn pointer_motion(state: &mut Compositor, x: f64, y: f64)`. A plain
///           motion helper — `click` inlined the same three lines but always followed
///           them with a press/release; this exposes the motion on its own.
/// Why:      `EventControllerMotion` needs to drive the seat pointer without clicking.
pub fn pointer_motion(state: &mut Compositor, x: f64, y: f64) {
    let pointer = state.seat.get_pointer().unwrap();
    let location: Point<f64, Logical> = (x, y).into();
    let under = state.surface_under(location);
    let time = event_time(state);
    pointer.motion(
        state,
        under,
        &MotionEvent {
            location,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    pointer.frame(state);
}

/// Move the pointer to a point and press or release one evdev button there.
///
/// What:     `pub fn pointer_button(state: &mut Compositor, x: f64, y: f64, evdev_code:
///           u32, pressed: bool)`. Unlike `click` (a full press+release), this sends a
///           single button transition so GTK's separate press/release edges map through
///           faithfully.
/// Why:      `GestureClick` reports press and release as distinct events; drags and
///           held buttons need each edge delivered separately.
pub fn pointer_button(state: &mut Compositor, x: f64, y: f64, evdev_code: u32, pressed: bool) {
    // What:     Re-home the pointer (and its focus) at the button location first.
    // Why:      The button event must land on whatever surface is under that point.
    pointer_motion(state, x, y);

    // Click-to-focus: a press hands keyboard focus to the toplevel under the cursor.
    //
    // Why:      This is the standard compositor behaviour and the robust source of truth
    //           for keyboard focus. Relying on GTK focus-enter alone races the client's
    //           map (the pane grabs GTK focus before the browser maps, so the enter sets
    //           an empty target and never re-fires on a plain click). Anchoring focus to
    //           the click means "click the field, then type" works regardless of timing.
    if pressed {
        let location: Point<f64, Logical> = (x, y).into();
        let target = state
            .space
            .element_under(location)
            .and_then(|(window, _)| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone());
        if target.is_some() {
            let keyboard = state.seat.get_keyboard().unwrap();
            let serial = SERIAL_COUNTER.next_serial();
            keyboard.set_focus(state, target, serial);
        }
    }

    let pointer = state.seat.get_pointer().unwrap();
    let time = event_time(state);
    let button_state = if pressed {
        ButtonState::Pressed
    } else {
        ButtonState::Released
    };
    pointer.button(
        state,
        &ButtonEvent {
            button: evdev_code,
            state: button_state,
            serial: SERIAL_COUNTER.next_serial(),
            time,
        },
    );
    pointer.frame(state);
}

/// Send a pointer axis (scroll) event with the given GTK deltas.
///
/// What:     `pub fn pointer_axis(state: &mut Compositor, dx: f64, dy: f64)`. Builds one
///           `AxisFrame` (source `Wheel`), filling in whichever axes are non-zero with
///           both a continuous value and a discrete `v120` step, then frames it.
/// Why:      `EventControllerScroll` yields dx/dy deltas that must reach the client as
///           `wl_pointer.axis` events.
pub fn pointer_axis(state: &mut Compositor, dx: f64, dy: f64) {
    let pointer = state.seat.get_pointer().unwrap();
    let time = event_time(state);
    let mut frame = AxisFrame::new(time).source(AxisSource::Wheel);
    if dx != 0.0 {
        frame = frame
            .value(Axis::Horizontal, dx * SCROLL_STEP)
            .v120(Axis::Horizontal, (dx * 120.0) as i32);
    }
    if dy != 0.0 {
        frame = frame
            .value(Axis::Vertical, dy * SCROLL_STEP)
            .v120(Axis::Vertical, (dy * 120.0) as i32);
    }
    pointer.axis(state, frame);
    pointer.frame(state);
}

/// Give or drop keyboard focus on the hosted client's top-level surface.
///
/// What:     `pub fn set_keyboard_focus(state: &mut Compositor, focused: bool)`. On
///           `true`, focuses the top-most mapped window's `wl_surface`; on `false`,
///           clears focus (`None`).
/// Why:      `EventControllerFocus` enter/leave on the GTK pane must hand keyboard focus
///           to / from the nested client so keystrokes only flow while the pane is
///           focused.
pub fn set_keyboard_focus(state: &mut Compositor, focused: bool) {
    let keyboard = state.seat.get_keyboard().unwrap();
    let serial = SERIAL_COUNTER.next_serial();

    // What:     Resolve the focus target before the mutable `set_focus` borrow.
    // Why:      `state.space` is borrowed read-only only for this expression, which
    //           clones an owned surface, so `state` is free for the `&mut` call below.
    let target = if focused {
        state
            .space
            .elements()
            .last()
            .and_then(|window| window.toplevel())
            .map(|toplevel| toplevel.wl_surface().clone())
    } else {
        None
    };

    keyboard.set_focus(state, target, serial);
}

/// Apply one queued `SpikeInput` to the live seat (runs on the compositor thread).
///
/// What:     `pub fn apply(state: &mut Compositor, event: SpikeInput)`. Dispatches each
///           variant to the matching seat-synthesis helper.
/// Why:      One place that maps the GTK-sourced event enum onto the seat, shared by the
///           drain loop.
pub fn apply(state: &mut Compositor, event: SpikeInput) {
    match event {
        SpikeInput::Motion { x, y } => pointer_motion(state, x, y),
        SpikeInput::Button {
            x,
            y,
            button,
            pressed,
        } => pointer_button(state, x, y, button, pressed),
        SpikeInput::Scroll { dx, dy } => pointer_axis(state, dx, dy),
        SpikeInput::Key { evdev, pressed } => {
            let key_state = if pressed {
                KeyState::Pressed
            } else {
                KeyState::Released
            };
            send_key(state, evdev, key_state);
        }
        SpikeInput::Text(text) => type_text(state, &text),
        SpikeInput::Focus(on) => set_keyboard_focus(state, on),
        SpikeInput::Resize { width, height } => {
            crate::control::resize_output(state, width, height)
        }
    }
}

/// Drain every pending `SpikeInput` from the state's channel and apply it.
///
/// What:     `pub fn drain_input(state: &mut Compositor)`. Clones the receiver (crossbeam
///           receivers are cheap to clone) so no borrow of `state` is held across the
///           `apply` calls, then applies each queued event non-blockingly.
/// Why:      Called once per event-loop iteration (from the post-dispatch callback) to
///           flush GTK input into the seat with sub-frame latency.
pub fn drain_input(state: &mut Compositor) {
    let receiver = state.input_rx.clone();
    while let Ok(event) = receiver.try_recv() {
        apply(state, event);
    }
}
