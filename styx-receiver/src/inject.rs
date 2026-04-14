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

        // Only treat the cursor as having hit the return edge if it is on
        // one of the displays that owns that edge AND at that display's own
        // outer boundary. This prevents a monitor that happens to be at the
        // rightmost x (but that is not part of the return edge) from falsely
        // triggering return, and lets either of multiple stacked edge
        // displays send the return signal.
        self.edge_displays.iter().any(|d| {
            let inside = new_x >= d.min_x && new_x < d.max_x
                && new_y >= d.min_y && new_y < d.max_y;
            if !inside { return false; }
            match self.return_edge {
                Edge::Right => new_x >= d.max_x - 1.0,
                Edge::Left => new_x <= d.min_x,
                Edge::Bottom => new_y >= d.max_y - 1.0,
                Edge::Top => new_y <= d.min_y,
            }
        })
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
    /// the bottom of the combined edge span. Clamps to the span and picks
    /// the specific edge display that contains the target position (or the
    /// nearest one if it falls in a gap between stacked displays).
    pub fn place_cursor_from_bottom(&mut self, from_bottom: f64) {
        if self.edge_displays.is_empty() {
            return;
        }
        let pos = (self.edge_span.max - from_bottom).clamp(self.edge_span.min, self.edge_span.max);
        let target = self.edge_displays.iter().find(|d| {
            match self.return_edge {
                Edge::Left | Edge::Right => pos >= d.min_y && pos < d.max_y,
                Edge::Top | Edge::Bottom => pos >= d.min_x && pos < d.max_x,
            }
        }).or_else(|| self.edge_displays.iter().min_by(|a, b| {
            let ma = match self.return_edge {
                Edge::Left | Edge::Right => (a.min_y + a.max_y) * 0.5,
                Edge::Top | Edge::Bottom => (a.min_x + a.max_x) * 0.5,
            };
            let mb = match self.return_edge {
                Edge::Left | Edge::Right => (b.min_y + b.max_y) * 0.5,
                Edge::Top | Edge::Bottom => (b.min_x + b.max_x) * 0.5,
            };
            (ma - pos).abs().partial_cmp(&(mb - pos).abs()).unwrap()
        })).unwrap();
        let (x, y) = match self.return_edge {
            Edge::Right => (target.max_x - 2.0, pos.clamp(target.min_y, target.max_y - 1.0)),
            Edge::Left  => (target.min_x + 2.0, pos.clamp(target.min_y, target.max_y - 1.0)),
            Edge::Bottom => (pos.clamp(target.min_x, target.max_x - 1.0), target.max_y - 2.0),
            Edge::Top    => (pos.clamp(target.min_x, target.max_x - 1.0), target.min_y + 2.0),
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

/// How close two monitor edges must be (in points) to count as occupying
/// the same return-edge column. Handles displays whose outer edges do not
/// line up exactly -- e.g. a portrait monitor stacked above a laptop
/// display where both face the sender on their right side.
const EDGE_ALIGN_TOLERANCE: f64 = 64.0;

/// Return the bounds of every active display that sits at the return
/// edge. For `Edge::Right` that is every display whose right edge is
/// within `EDGE_ALIGN_TOLERANCE` of the overall rightmost x; analogously
/// for the other edges.
fn compute_edge_displays(return_edge: Edge) -> Vec<DisplayBounds> {
    let Ok(ids) = CGDisplay::active_displays() else {
        return Vec::new();
    };
    let mut all: Vec<DisplayBounds> = Vec::new();
    for id in ids {
        let b = CGDisplay::new(id).bounds();
        all.push(DisplayBounds {
            min_x: b.origin.x,
            min_y: b.origin.y,
            max_x: b.origin.x + b.size.width,
            max_y: b.origin.y + b.size.height,
        });
    }
    if all.is_empty() {
        return all;
    }
    let extreme = match return_edge {
        Edge::Right => all.iter().map(|d| d.max_x).fold(f64::MIN, f64::max),
        Edge::Left => all.iter().map(|d| d.min_x).fold(f64::MAX, f64::min),
        Edge::Bottom => all.iter().map(|d| d.max_y).fold(f64::MIN, f64::max),
        Edge::Top => all.iter().map(|d| d.min_y).fold(f64::MAX, f64::min),
    };
    all.into_iter()
        .filter(|d| {
            let own = match return_edge {
                Edge::Right => d.max_x,
                Edge::Left => d.min_x,
                Edge::Bottom => d.max_y,
                Edge::Top => d.min_y,
            };
            (own - extreme).abs() <= EDGE_ALIGN_TOLERANCE
        })
        .collect()
}

/// Union the Y span (left/right edges) or X span (top/bottom edges) over
/// a set of edge-owning displays.
fn span_of_displays(displays: &[DisplayBounds], return_edge: Edge) -> EdgeSpan {
    if displays.is_empty() {
        return EdgeSpan { min: 0.0, max: 1080.0 };
    }
    let mut min = f64::MAX;
    let mut max = f64::MIN;
    for d in displays {
        let (a, b) = match return_edge {
            Edge::Left | Edge::Right => (d.min_y, d.max_y),
            Edge::Top | Edge::Bottom => (d.min_x, d.max_x),
        };
        if a < min { min = a; }
        if b > max { max = b; }
    }
    EdgeSpan { min, max }
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
