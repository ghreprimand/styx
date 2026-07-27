use std::collections::HashSet;
use std::time::{Duration, Instant};

use core_foundation::base::TCFType;
use core_foundation::string::{CFString, CFStringRef};
use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventFlags, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use styx_keymap;

use crate::geometry::{
    self, DisplayBounds, Edge, EdgeSpan, clamp_to_displays, resolve_edge_hit, span_of_displays,
};

const K_IOPM_USER_ACTIVE_LOCAL: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionDeclareUserActivity(
        assertion_name: CFStringRef,
        user_type: u32,
        assertion_id: *mut u32,
    ) -> i32;
}

pub struct Injector {
    source: CGEventSource,
    held_keys: HashSet<u32>,
    button_state: ButtonState,
    cursor_pos: CGPoint,
    display_bounds: DisplayBounds,
    /// Every active display rectangle. Used to clamp the cursor to the
    /// *union* of displays (like macOS does for real HID input) rather than
    /// to the global bounding box, which can contain phantom regions that
    /// belong to no display when monitors are not flush.
    displays: Vec<DisplayBounds>,
    edge_displays: Vec<DisplayBounds>,
    edge_span: EdgeSpan,
    return_edge: Edge,
    swap_alt_cmd: bool,
    assertion_name: CFString,
    assertion_id: u32,
}

// macOS default double-click interval
const DOUBLE_CLICK_INTERVAL: Duration = Duration::from_millis(500);

struct ButtonTracker {
    pressed: bool,
    click_count: i64,
    last_press: Option<Instant>,
}

impl ButtonTracker {
    fn new() -> Self {
        ButtonTracker { pressed: false, click_count: 0, last_press: None }
    }

    /// Called on press. Updates click count and returns it.
    fn on_press(&mut self) -> i64 {
        let now = Instant::now();
        self.click_count = if self.last_press.map_or(false, |t| now.duration_since(t) <= DOUBLE_CLICK_INTERVAL) {
            self.click_count + 1
        } else {
            1
        };
        self.last_press = Some(now);
        self.pressed = true;
        self.click_count
    }

    fn on_release(&mut self) -> i64 {
        self.pressed = false;
        self.click_count
    }
}

struct ButtonState {
    left: ButtonTracker,
    right: ButtonTracker,
    middle: ButtonTracker,
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

impl Injector {
    pub fn new(return_edge: Edge, swap_alt_cmd: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| "failed to create CGEventSource")?;

        let bounds = compute_display_bounds();
        let displays = compute_all_displays();
        let edge_displays = compute_edge_displays(return_edge);
        let edge_span = span_of_displays(&edge_displays, return_edge);
        log::info!(
            "display bounds: x=[{}, {}] y=[{}, {}], edge displays: {}, edge span: [{}, {}]",
            bounds.min_x, bounds.max_x, bounds.min_y, bounds.max_y,
            edge_displays.len(),
            edge_span.min, edge_span.max
        );
        let mid = edge_span.min + 0.5 * (edge_span.max - edge_span.min);
        let cursor_pos = match return_edge {
            Edge::Right => CGPoint::new(bounds.max_x - 2.0, mid),
            Edge::Left => CGPoint::new(bounds.min_x + 2.0, mid),
            Edge::Bottom => CGPoint::new(mid, bounds.max_y - 2.0),
            Edge::Top => CGPoint::new(mid, bounds.min_y + 2.0),
        };

        Ok(Injector {
            source,
            held_keys: HashSet::new(),
            button_state: ButtonState {
                left: ButtonTracker::new(),
                right: ButtonTracker::new(),
                middle: ButtonTracker::new(),
            },
            cursor_pos,
            display_bounds: bounds,
            displays,
            edge_displays,
            edge_span,
            return_edge,
            swap_alt_cmd,
            assertion_name: CFString::new("styx-receiver"),
            assertion_id: 0,
        })
    }

    /// Tell macOS a user just did something. Wakes a slept external display
    /// (CGEvent injection alone does not) and resets the idle timer.
    fn declare_user_activity(&mut self) {
        unsafe {
            IOPMAssertionDeclareUserActivity(
                self.assertion_name.as_concrete_TypeRef(),
                K_IOPM_USER_ACTIVE_LOCAL,
                &mut self.assertion_id,
            );
        }
    }

    /// Recreate the CGEventSource and recompute display geometry.
    /// Fixes stale event injection after macOS sleep/wake cycles.
    pub fn reinit(&mut self) {
        match CGEventSource::new(CGEventSourceStateID::CombinedSessionState) {
            Ok(source) => self.source = source,
            Err(_) => {
                log::error!("reinit: failed to create CGEventSource");
                return;
            }
        }
        self.display_bounds = compute_display_bounds();
        self.displays = compute_all_displays();
        self.edge_displays = compute_edge_displays(self.return_edge);
        self.edge_span = span_of_displays(&self.edge_displays, self.return_edge);
        log::info!(
            "reinit: display bounds: x=[{}, {}] y=[{}, {}], edge displays: {}, edge span: [{}, {}]",
            self.display_bounds.min_x, self.display_bounds.max_x,
            self.display_bounds.min_y, self.display_bounds.max_y,
            self.edge_displays.len(),
            self.edge_span.min, self.edge_span.max
        );
    }

    /// Returns true if the cursor hit the return edge.
    pub fn inject_mouse_motion(&mut self, dx: f64, dy: f64) -> bool {
        self.declare_user_activity();
        // Clamp against the *union* of displays, not the global bounding box.
        // The bounding box can include phantom regions that belong to no
        // display (e.g. directly below a short built-in panel when a taller
        // monitor sits beside it). A cursor parked in such a region never
        // satisfies the Dock's per-display bottom-edge reveal test, because it
        // is below the panel's real bottom edge rather than on it. Snapping to
        // the nearest on-display point reproduces what macOS does for real HID
        // input. Falls back to the global box only if the display list is empty.
        let target_x = self.cursor_pos.x + dx;
        let target_y = self.cursor_pos.y + dy;
        let (clamped_x, clamped_y) = if self.displays.is_empty() {
            (
                target_x.clamp(self.display_bounds.min_x, self.display_bounds.max_x - 1.0),
                target_y.clamp(self.display_bounds.min_y, self.display_bounds.max_y - 1.0),
            )
        } else {
            clamp_to_displays(&self.displays, target_x, target_y)
        };

        // Decide whether the cursor has reached the outer return edge, and
        // pin it to that edge if so. See `resolve_edge_hit` for why this is
        // done against the specific edge display rather than the global
        // bounding box.
        let (new_x, new_y, hit) =
            resolve_edge_hit(&self.edge_displays, self.return_edge, clamped_x, clamped_y);

        self.cursor_pos = CGPoint::new(new_x, new_y);

        let event_type = if self.button_state.left.pressed {
            CGEventType::LeftMouseDragged
        } else if self.button_state.right.pressed {
            CGEventType::RightMouseDragged
        } else if self.button_state.middle.pressed {
            CGEventType::OtherMouseDragged
        } else {
            CGEventType::MouseMoved
        };

        if let Ok(event) = CGEvent::new_mouse_event(
            self.source.clone(),
            event_type,
            self.cursor_pos,
            CGMouseButton::Left,
        ) {
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_X, dx as i64);
            event.set_integer_value_field(EventField::MOUSE_EVENT_DELTA_Y, dy as i64);
            event.post(CGEventTapLocation::HID);
        }

        // The hit decision (and the accompanying pin) was computed above by
        // `resolve_edge_hit`, before the cursor was moved.
        hit
    }

    pub fn inject_mouse_button(&mut self, button: u32, state: u8) {
        self.declare_user_activity();
        let pressed = state == 1;
        let (event_type, cg_button, click_count) = match button {
            BTN_LEFT => {
                let count = if pressed {
                    self.button_state.left.on_press()
                } else {
                    self.button_state.left.on_release()
                };
                let event_type = if pressed { CGEventType::LeftMouseDown } else { CGEventType::LeftMouseUp };
                (event_type, CGMouseButton::Left, count)
            }
            BTN_RIGHT => {
                let count = if pressed {
                    self.button_state.right.on_press()
                } else {
                    self.button_state.right.on_release()
                };
                let event_type = if pressed { CGEventType::RightMouseDown } else { CGEventType::RightMouseUp };
                (event_type, CGMouseButton::Right, count)
            }
            BTN_MIDDLE => {
                let count = if pressed {
                    self.button_state.middle.on_press()
                } else {
                    self.button_state.middle.on_release()
                };
                let event_type = if pressed { CGEventType::OtherMouseDown } else { CGEventType::OtherMouseUp };
                (event_type, CGMouseButton::Center, count)
            }
            _ => return,
        };

        if let Ok(event) = CGEvent::new_mouse_event(
            self.source.clone(),
            event_type,
            self.cursor_pos,
            cg_button,
        ) {
            event.set_integer_value_field(EventField::MOUSE_EVENT_CLICK_STATE, click_count);
            event.post(CGEventTapLocation::HID);
        }
    }

    pub fn inject_key(&mut self, code: u32, pressed: bool) {
        self.declare_user_activity();
        let code = if self.swap_alt_cmd { swap_alt_meta(code) } else { code };
        let Some(mac_code) = styx_keymap::evdev_to_macos(code as u16) else {
            log::warn!("unmapped evdev key: {code}");
            return;
        };

        if pressed {
            self.held_keys.insert(code);
        } else {
            self.held_keys.remove(&code);
        }

        if let Ok(event) = CGEvent::new_keyboard_event(
            self.source.clone(),
            mac_code as CGKeyCode,
            pressed,
        ) {
            // Explicitly set modifier flags from our tracked state to prevent
            // stale flags (e.g. Fn from Home/End keys) leaking into subsequent
            // events via the CGEventSource.
            event.set_flags(self.current_flags() | key_flags(mac_code));
            event.post(CGEventTapLocation::HID);
        }
    }

    /// Build CGEventFlags from the currently held modifier keys.
    fn current_flags(&self) -> CGEventFlags {
        let mut flags = CGEventFlags::CGEventFlagNull;
        for &code in &self.held_keys {
            flags |= match code {
                styx_keymap::KEY_LEFT_SHIFT | styx_keymap::KEY_RIGHT_SHIFT => {
                    CGEventFlags::CGEventFlagShift
                }
                styx_keymap::KEY_LEFT_CTRL | styx_keymap::KEY_RIGHT_CTRL => {
                    CGEventFlags::CGEventFlagControl
                }
                styx_keymap::KEY_LEFT_ALT | styx_keymap::KEY_RIGHT_ALT => {
                    CGEventFlags::CGEventFlagAlternate
                }
                styx_keymap::KEY_LEFT_META | styx_keymap::KEY_RIGHT_META => {
                    CGEventFlags::CGEventFlagCommand
                }
                _ => CGEventFlags::CGEventFlagNull,
            };
        }
        flags
    }

    pub fn inject_scroll(&mut self, axis: u8, value: f64) {
        self.declare_user_activity();
        let (v, h) = if axis == 0 {
            // Negate vertical scroll: Linux/Wayland and macOS use opposite
            // sign conventions for scroll direction.
            (-(value as i32), 0i32)
        } else {
            (0i32, value as i32)
        };

        if let Ok(event) = CGEvent::new_scroll_event(
            self.source.clone(),
            ScrollEventUnit::PIXEL,
            2,
            v,
            h,
            0,
        ) {
            event.post(CGEventTapLocation::HID);
        }
    }

    pub fn release_all_keys(&mut self) {
        let codes: Vec<u32> = self.held_keys.drain().collect();
        for code in codes {
            if let Some(mac_code) = styx_keymap::evdev_to_macos(code as u16) {
                if let Ok(event) = CGEvent::new_keyboard_event(
                    self.source.clone(),
                    mac_code as CGKeyCode,
                    false,
                ) {
                    event.post(CGEventTapLocation::HID);
                }
            }
        }

        if self.button_state.left.pressed {
            self.inject_mouse_button(BTN_LEFT, 0);
        }
        if self.button_state.right.pressed {
            self.inject_mouse_button(BTN_RIGHT, 0);
        }
        if self.button_state.middle.pressed {
            self.inject_mouse_button(BTN_MIDDLE, 0);
        }
    }

    /// Place the cursor at the entry edge, at the given pixel distance from
    /// the bottom of the combined edge span. Delegates the geometry to
    /// `geometry::place_from_bottom`; leaves the cursor untouched when there
    /// are no edge displays to place it on.
    pub fn place_cursor_from_bottom(&mut self, from_bottom: f64) {
        if let Some((x, y)) = geometry::place_from_bottom(
            &self.edge_displays,
            self.edge_span,
            self.return_edge,
            from_bottom,
        ) {
            self.cursor_pos = CGPoint::new(x, y);
        }
    }

    /// Returns the cursor's pixel distance from the bottom of the edge monitor
    /// and the edge monitor's total height.
    pub fn cursor_from_bottom(&self) -> (f64, f64) {
        geometry::from_bottom_of(
            self.edge_span,
            self.return_edge,
            self.cursor_pos.x,
            self.cursor_pos.y,
        )
    }
}

/// Snapshot every active display rectangle in CG global coordinates.
fn compute_all_displays() -> Vec<DisplayBounds> {
    let Ok(ids) = CGDisplay::active_displays() else {
        return Vec::new();
    };
    ids.into_iter()
        .map(|id| {
            let b = CGDisplay::new(id).bounds();
            DisplayBounds {
                min_x: b.origin.x,
                min_y: b.origin.y,
                max_x: b.origin.x + b.size.width,
                max_y: b.origin.y + b.size.height,
            }
        })
        .collect()
}

/// Return the bounds of every active display that sits at the return edge.
/// Enumeration is macOS-specific; the selection rule is shared.
fn compute_edge_displays(return_edge: Edge) -> Vec<DisplayBounds> {
    geometry::edge_displays_of(&compute_all_displays(), return_edge)
}

fn compute_display_bounds() -> DisplayBounds {
    geometry::bounding_box(&compute_all_displays())
}

/// Extra flags macOS expects on certain keys. Arrow keys carry SecondaryFn
/// and NumericPad; function and navigation keys carry SecondaryFn.
fn key_flags(mac_code: u16) -> CGEventFlags {
    match mac_code {
        // Arrow keys
        0x7B | 0x7C | 0x7D | 0x7E => {
            CGEventFlags::CGEventFlagSecondaryFn | CGEventFlags::CGEventFlagNumericPad
        }
        // F1-F12
        0x7A | 0x78 | 0x63 | 0x76 | 0x60 | 0x61 | 0x62 | 0x64 | 0x65 | 0x6D | 0x67
        | 0x6F => CGEventFlags::CGEventFlagSecondaryFn,
        // Home, End, Page Up, Page Down, Forward Delete
        0x73 | 0x77 | 0x74 | 0x79 | 0x75 => CGEventFlags::CGEventFlagSecondaryFn,
        _ => CGEventFlags::CGEventFlagNull,
    }
}

/// Swap Alt and Super/Meta evdev codes so physical key positions match macOS
/// layout: PC Super (position 2) becomes Option, PC Alt (position 3) becomes
/// Command.
fn swap_alt_meta(code: u32) -> u32 {
    match code {
        styx_keymap::KEY_LEFT_ALT => styx_keymap::KEY_LEFT_META,
        styx_keymap::KEY_RIGHT_ALT => styx_keymap::KEY_RIGHT_META,
        styx_keymap::KEY_LEFT_META => styx_keymap::KEY_LEFT_ALT,
        styx_keymap::KEY_RIGHT_META => styx_keymap::KEY_RIGHT_ALT,
        other => other,
    }
}
