use std::collections::HashSet;

use core_graphics::display::CGDisplay;
use core_graphics::event::{
    CGEvent, CGEventTapLocation, CGEventType, CGKeyCode, CGMouseButton, EventField,
    ScrollEventUnit,
};
use core_graphics::event_source::{CGEventSource, CGEventSourceStateID};
use core_graphics::geometry::CGPoint;

use styx_keymap;

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
}

struct ButtonState {
    left: bool,
    right: bool,
    middle: bool,
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
    pub fn new(return_edge: Edge) -> Result<Self, Box<dyn std::error::Error>> {
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
                left: false,
                right: false,
                middle: false,
            },
            cursor_pos,
            display_bounds: bounds,
            edge_span,
            return_edge,
        })
    }

    /// Returns true if the cursor hit the return edge.
    pub fn inject_mouse_motion(&mut self, dx: f64, dy: f64) -> bool {
        let new_x = (self.cursor_pos.x + dx).clamp(self.display_bounds.min_x, self.display_bounds.max_x - 1.0);
        let new_y = (self.cursor_pos.y + dy).clamp(self.display_bounds.min_y, self.display_bounds.max_y - 1.0);
        self.cursor_pos = CGPoint::new(new_x, new_y);

        let event_type = if self.button_state.left {
            CGEventType::LeftMouseDragged
        } else if self.button_state.right {
            CGEventType::RightMouseDragged
        } else if self.button_state.middle {
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
        let pressed = state == 1;
        let (event_type, cg_button) = match button {
            BTN_LEFT => {
                self.button_state.left = pressed;
                if pressed {
                    (CGEventType::LeftMouseDown, CGMouseButton::Left)
                } else {
                    (CGEventType::LeftMouseUp, CGMouseButton::Left)
                }
            }
            BTN_RIGHT => {
                self.button_state.right = pressed;
                if pressed {
                    (CGEventType::RightMouseDown, CGMouseButton::Right)
                } else {
                    (CGEventType::RightMouseUp, CGMouseButton::Right)
                }
            }
            BTN_MIDDLE => {
                self.button_state.middle = pressed;
                if pressed {
                    (CGEventType::OtherMouseDown, CGMouseButton::Center)
                } else {
                    (CGEventType::OtherMouseUp, CGMouseButton::Center)
                }
            }
            _ => return,
        };

        if let Ok(event) = CGEvent::new_mouse_event(
            self.source.clone(),
            event_type,
            self.cursor_pos,
            cg_button,
        ) {
            event.post(CGEventTapLocation::HID);
        }
    }

    pub fn inject_key(&mut self, code: u32, pressed: bool) {
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
            event.post(CGEventTapLocation::HID);
        }
    }

    pub fn inject_scroll(&mut self, axis: u8, value: f64) {
        let (v, h) = if axis == 0 {
            (value as i32, 0i32)
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

        if self.button_state.left {
            self.inject_mouse_button(BTN_LEFT, 0);
        }
        if self.button_state.right {
            self.inject_mouse_button(BTN_RIGHT, 0);
        }
        if self.button_state.middle {
            self.inject_mouse_button(BTN_MIDDLE, 0);
        }
    }

    pub fn reset_cursor_to_entry(&mut self, edge_fraction: f64) {
        // Map the fraction to the edge span (the monitor at the return edge),
        // not the full multi-monitor bounding box.
        let pos = self.edge_span.min + edge_fraction * (self.edge_span.max - self.edge_span.min);
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

    /// Returns the normalized position (0.0–1.0) along the return edge,
    /// relative to the monitor that owns that edge.
    pub fn edge_fraction(&self) -> f64 {
        let pos = match self.return_edge {
            Edge::Left | Edge::Right => self.cursor_pos.y,
            Edge::Top | Edge::Bottom => self.cursor_pos.x,
        };
        let span = self.edge_span.max - self.edge_span.min;
        if span > 0.0 { ((pos - self.edge_span.min) / span).clamp(0.0, 1.0) } else { 0.5 }
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
