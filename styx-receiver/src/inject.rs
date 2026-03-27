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

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;

impl Injector {
    pub fn new(return_edge: Edge) -> Result<Self, Box<dyn std::error::Error>> {
        let source = CGEventSource::new(CGEventSourceStateID::CombinedSessionState)
            .map_err(|_| "failed to create CGEventSource")?;

        let bounds = compute_display_bounds();
        let cursor_pos = entry_point(&bounds, return_edge);

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

    pub fn reset_cursor_to_entry(&mut self) {
        self.cursor_pos = entry_point(&self.display_bounds, self.return_edge);
    }
}

fn entry_point(bounds: &DisplayBounds, return_edge: Edge) -> CGPoint {
    let cx = (bounds.min_x + bounds.max_x) / 2.0;
    let cy = (bounds.min_y + bounds.max_y) / 2.0;
    match return_edge {
        Edge::Right => CGPoint::new(bounds.max_x - 2.0, cy),
        Edge::Left => CGPoint::new(bounds.min_x + 2.0, cy),
        Edge::Bottom => CGPoint::new(cx, bounds.max_y - 2.0),
        Edge::Top => CGPoint::new(cx, bounds.min_y + 2.0),
    }
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
