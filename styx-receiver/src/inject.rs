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

const K_IOPM_USER_ACTIVE_LOCAL: u32 = 0;

#[link(name = "IOKit", kind = "framework")]
unsafe extern "C" {
    fn IOPMAssertionDeclareUserActivity(
        assertion_name: CFStringRef,
        user_type: u32,
        assertion_id: *mut u32,
    ) -> i32;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

pub struct Injector {
    source: CGEventSource,
    held_keys: HashSet<u32>,
    button_state: ButtonState,
    cursor_pos: CGPoint,
    display_bounds: DisplayBounds,
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

#[derive(Clone)]
struct DisplayBounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

/// The span along the return edge (the monitor that owns that edge).
#[derive(Clone)]
struct EdgeSpan {
    min: f64,
    max: f64,
}

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

impl Injector {
    pub fn new(return_edge: Edge, swap_alt_cmd: bool) -> Result<Self, Box<dyn std::error::Error>> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| "failed to create CGEventSource")?;

        let bounds = compute_display_bounds();
        let edge_span = compute_edge_span(return_edge);
        log::info!(
            "display bounds: x=[{}, {}] y=[{}, {}], edge span: [{}, {}]",
            bounds.min_x, bounds.max_x, bounds.min_y, bounds.max_y,
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
        self.edge_span = compute_edge_span(self.return_edge);
        log::info!(
            "reinit: display bounds: x=[{}, {}] y=[{}, {}], edge span: [{}, {}]",
            self.display_bounds.min_x, self.display_bounds.max_x,
            self.display_bounds.min_y, self.display_bounds.max_y,
            self.edge_span.min, self.edge_span.max
        );
    }

    /// Returns true if the cursor hit the return edge.
    pub fn inject_mouse_motion(&mut self, dx: f64, dy: f64) -> bool {
        self.declare_user_activity();
        let new_x = (self.cursor_pos.x + dx).clamp(self.display_bounds.min_x, self.display_bounds.max_x - 1.0);
        let new_y = (self.cursor_pos.y + dy).clamp(self.display_bounds.min_y, self.display_bounds.max_y - 1.0);
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

        match self.return_edge {
            Edge::Right => new_x >= self.display_bounds.max_x - 1.0,
            Edge::Left => new_x <= self.display_bounds.min_x,
            Edge::Bottom => new_y >= self.display_bounds.max_y - 1.0,
            Edge::Top => new_y <= self.display_bounds.min_y,
        }
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
    /// the bottom of the edge monitor. Clamps to the edge span.
    pub fn place_cursor_from_bottom(&mut self, from_bottom: f64) {
        let pos = (self.edge_span.max - from_bottom).clamp(self.edge_span.min, self.edge_span.max);
        let x = match self.return_edge {
            Edge::Right => self.display_bounds.max_x - 2.0,
            Edge::Left => self.display_bounds.min_x + 2.0,
            Edge::Top | Edge::Bottom => pos,
        };
        let y = match self.return_edge {
            Edge::Left | Edge::Right => pos,
            Edge::Bottom => self.display_bounds.max_y - 2.0,
            Edge::Top => self.display_bounds.min_y + 2.0,
        };
        self.cursor_pos = CGPoint::new(x, y);
    }

    /// Returns the cursor's pixel distance from the bottom of the edge monitor
    /// and the edge monitor's total height.
    pub fn cursor_from_bottom(&self) -> (f64, f64) {
        let pos = match self.return_edge {
            Edge::Left | Edge::Right => self.cursor_pos.y,
            Edge::Top | Edge::Bottom => self.cursor_pos.x,
        };
        let from_bottom = (self.edge_span.max - pos).clamp(0.0, self.edge_span.max - self.edge_span.min);
        let height = self.edge_span.max - self.edge_span.min;
        (from_bottom, height)
    }
}

/// Find the monitor that owns the return edge and return its span along
/// the perpendicular axis. For a left/right edge this is the Y range of
/// the rightmost/leftmost monitor.
fn compute_edge_span(return_edge: Edge) -> EdgeSpan {
    if let Ok(displays) = CGDisplay::active_displays() {
        let mut best: Option<(f64, f64, f64)> = None; // (edge_coord, span_min, span_max)
        for id in displays {
            let display = CGDisplay::new(id);
            let b = display.bounds();
            let (edge_coord, span_min, span_max) = match return_edge {
                Edge::Right => (b.origin.x + b.size.width, b.origin.y, b.origin.y + b.size.height),
                Edge::Left => (-b.origin.x, b.origin.y, b.origin.y + b.size.height),
                Edge::Bottom => (b.origin.y + b.size.height, b.origin.x, b.origin.x + b.size.width),
                Edge::Top => (-b.origin.y, b.origin.x, b.origin.x + b.size.width),
            };
            if best.is_none() || edge_coord > best.unwrap().0 {
                best = Some((edge_coord, span_min, span_max));
            }
        }
        if let Some((_, min, max)) = best {
            return EdgeSpan { min, max };
        }
    }
    EdgeSpan { min: 0.0, max: 1080.0 }
}

fn compute_display_bounds() -> DisplayBounds {
    let mut min_x = f64::MAX;
    let mut min_y = f64::MAX;
    let mut max_x = f64::MIN;
    let mut max_y = f64::MIN;

    if let Ok(displays) = CGDisplay::active_displays() {
        for id in displays {
            let display = CGDisplay::new(id);
            let bounds = display.bounds();
            min_x = min_x.min(bounds.origin.x);
            min_y = min_y.min(bounds.origin.y);
            max_x = max_x.max(bounds.origin.x + bounds.size.width);
            max_y = max_y.max(bounds.origin.y + bounds.size.height);
        }
    }

    if min_x >= max_x {
        return DisplayBounds {
            min_x: 0.0,
            min_y: 0.0,
            max_x: 1920.0,
            max_y: 1080.0,
        };
    }

    DisplayBounds { min_x, min_y, max_x, max_y }
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
