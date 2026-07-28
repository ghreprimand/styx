//! Linux input injection via `/dev/uinput`.
//!
//! The macOS backend posts `CGEvent`s at absolute screen coordinates. Wayland
//! has no equivalent "put the cursor here" API that works without compositor
//! cooperation, so this backend creates a virtual **absolute** pointing device
//! instead: it reports positions rather than deltas, exactly like the tablet
//! devices QEMU and VirtualBox expose to guests. That keeps the backend
//! compositor-agnostic -- no Hyprland IPC, no wlr protocol -- and means the
//! cursor can be placed precisely on crossover, which relative motion cannot
//! do.
//!
//! Two virtual devices are created rather than one. A single device carrying
//! both `ABS_X`/`ABS_Y` and the full key range invites misclassification by
//! libinput; splitting them into a pointer and a keyboard matches how real
//! hardware presents itself.
//!
//! All cursor math is shared with the macOS backend via `crate::geometry`, so
//! edge resolution, union clamping, and crossover placement behave identically
//! on both platforms.

use std::collections::HashSet;

use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, EventType, InputEvent, KeyCode, RelativeAxisCode,
    UinputAbsSetup,
    uinput::VirtualDevice,
};

use crate::geometry::{
    self, DisplayBounds, Edge, EdgeConfig, EdgeHit, EdgeSpan, span_of_displays,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const BTN_SIDE: u32 = 0x113;
const BTN_EXTRA: u32 = 0x114;

/// Resolution of the virtual absolute axes. The compositor maps this range
/// onto the desktop extent, so a large value keeps quantisation well below
/// one pixel on any realistic layout.
const ABS_MAX: i32 = 65535;

/// evdev high-resolution scroll: 120 units == one detent. The sender prefers
/// Wayland's `AxisValue120` events, which use exactly this scale, so hi-res
/// values pass through untouched.
const HI_RES_PER_DETENT: f64 = 120.0;

/// Highest key code claimed by the virtual keyboard. Stops below the `BTN_*`
/// range (which starts at 0x100) so the keyboard is never mistaken for a
/// pointing device.
const MAX_KEY_CODE: u16 = 248;

pub struct Injector {
    pointer: VirtualDevice,
    keyboard: VirtualDevice,
    held_keys: HashSet<u32>,
    held_buttons: HashSet<u32>,
    cursor_pos: (f64, f64),
    display_bounds: DisplayBounds,
    displays: Vec<DisplayBounds>,
    edge_displays: Vec<DisplayBounds>,
    edge_span: EdgeSpan,
    return_edge: Edge,
    /// Edge facing the downstream peer, when this node relays. `None` on a
    /// terminal receiver.
    forward: Option<EdgeConfig>,
    /// Whether the downstream link is currently healthy. While false the
    /// forward edge is not passed to `resolve_motion` at all, so behaviour is
    /// identical to a node with no relay configured.
    forward_armed: bool,
    /// Residual hi-res scroll per axis, so sub-detent deltas accumulate into
    /// whole `REL_WHEEL` clicks instead of being truncated away.
    scroll_residual: (f64, f64),
}

impl Injector {
    /// Build both virtual devices and seat the cursor at the middle of the
    /// return edge.
    ///
    /// `displays` is the compositor's layout as declared in the config. This
    /// backend has no display-server connection to enumerate outputs, and the
    /// declared extent must match the compositor's real one -- the mapping
    /// from the virtual absolute axes onto the desktop depends on it.
    pub fn new(
        return_edge: Edge,
        displays: Vec<DisplayBounds>,
        forward_edge: Option<Edge>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        if displays.is_empty() {
            return Err("no displays configured; set [[receiver.display]] in the config".into());
        }

        let bounds = geometry::bounding_box(&displays);
        let edge_displays = geometry::edge_displays_of(&displays, return_edge);
        let edge_span = span_of_displays(&edge_displays, return_edge);

        let forward = match forward_edge {
            Some(e) if e == return_edge => {
                return Err(format!(
                    "forward_edge and return_edge are both '{e:?}'; they must be different sides"
                )
                .into());
            }
            Some(e) => Some(EdgeConfig::build(&displays, e)),
            None => None,
        };

        let pointer = build_pointer_device()?;
        let keyboard = build_keyboard_device()?;

        log::info!(
            "uinput devices created; desktop extent x=[{}, {}] y=[{}, {}], \
             edge displays: {}, edge span: [{}, {}]",
            bounds.min_x, bounds.max_x, bounds.min_y, bounds.max_y,
            edge_displays.len(),
            edge_span.min, edge_span.max,
        );
        log::info!(
            "absolute axes are mapped onto that extent; if it does not match the \
             compositor's actual layout the cursor will land in the wrong place",
        );

        let mid = edge_span.min + 0.5 * (edge_span.max - edge_span.min);
        let cursor_pos = match return_edge {
            Edge::Right => (bounds.max_x - 2.0, mid),
            Edge::Left => (bounds.min_x + 2.0, mid),
            Edge::Bottom => (mid, bounds.max_y - 2.0),
            Edge::Top => (mid, bounds.min_y + 2.0),
        };

        Ok(Injector {
            pointer,
            keyboard,
            held_keys: HashSet::new(),
            held_buttons: HashSet::new(),
            cursor_pos,
            display_bounds: bounds,
            displays,
            edge_displays,
            edge_span,
            return_edge,
            forward,
            forward_armed: false,
            scroll_residual: (0.0, 0.0),
        })
    }

    /// No-op on Linux. The macOS backend rebuilds its `CGEventSource` here to
    /// survive sleep/wake, and re-enumerates displays; uinput devices persist
    /// across suspend and the display layout comes from config, so there is
    /// nothing to refresh.
    pub fn reinit(&mut self) {}

    /// Map a desktop coordinate onto the virtual device's absolute axis range.
    fn to_abs(&self, x: f64, y: f64) -> (i32, i32) {
        to_abs_in(self.display_bounds, x, y)
    }

    fn emit_cursor(&mut self) {
        let (ax, ay) = self.to_abs(self.cursor_pos.0, self.cursor_pos.1);
        let events = [
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_X.0, ax),
            InputEvent::new(EventType::ABSOLUTE.0, AbsoluteAxisCode::ABS_Y.0, ay),
        ];
        if let Err(e) = self.pointer.emit(&events) {
            log::error!("uinput pointer emit failed: {e}");
        }
    }

    /// Returns true if the cursor hit the return edge.
    ///
    /// Mirrors the macOS backend exactly: accumulate the delta, clamp to the
    /// union of displays, then resolve the edge against the specific display
    /// that owns the cursor's position.
    pub fn inject_mouse_motion(&mut self, dx: f64, dy: f64) -> EdgeHit {
        let ret = EdgeConfig {
            edge: self.return_edge,
            displays: self.edge_displays.clone(),
            span: self.edge_span,
        };
        let forward = if self.forward_armed { self.forward.as_ref() } else { None };

        let (pos, hit) = geometry::resolve_motion(
            &self.displays,
            self.display_bounds,
            &ret,
            forward,
            self.cursor_pos,
            (dx, dy),
        );

        self.cursor_pos = pos;
        self.emit_cursor();
        hit
    }

    /// Arm or disarm the forward edge. Driven by downstream link health.
    pub fn set_forward_armed(&mut self, armed: bool) {
        self.forward_armed = armed;
    }

    /// Keys currently held, for seeding state on the downstream peer when the
    /// cursor crosses forward mid-chord.
    pub fn held_keys(&self) -> Vec<u32> {
        self.held_keys.iter().copied().collect()
    }

    /// Place the cursor at the forward edge, `from_bottom` up from the bottom
    /// of that edge's span. Used when the downstream peer hands the cursor
    /// back.
    pub fn place_cursor_at_forward_edge(&mut self, from_bottom: f64) {
        let Some(f) = self.forward.as_ref() else { return };
        if let Some((x, y)) =
            geometry::place_from_bottom(&f.displays, f.span, f.edge, from_bottom)
        {
            self.cursor_pos = (x, y);
            self.emit_cursor();
        }
    }

    /// Cursor offset along the forward edge, for the `CaptureBegin` sent
    /// downstream.
    pub fn cursor_from_forward_edge(&self) -> (f64, f64) {
        match self.forward.as_ref() {
            Some(f) => {
                geometry::from_bottom_of(f.span, f.edge, self.cursor_pos.0, self.cursor_pos.1)
            }
            None => (0.0, 0.0),
        }
    }

    pub fn inject_mouse_button(&mut self, button: u32, state: u8) {
        let code = match button {
            BTN_LEFT | BTN_RIGHT | BTN_MIDDLE | BTN_SIDE | BTN_EXTRA => button as u16,
            other => {
                log::warn!("unmapped mouse button: {other}");
                return;
            }
        };
        let pressed = state == 1;
        if pressed {
            self.held_buttons.insert(button);
        } else {
            self.held_buttons.remove(&button);
        }
        let value = if pressed { 1 } else { 0 };
        if let Err(e) = self
            .pointer
            .emit(&[InputEvent::new(EventType::KEY.0, code, value)])
        {
            log::error!("uinput button emit failed: {e}");
        }
    }

    pub fn inject_key(&mut self, code: u32, pressed: bool) {
        // The wire protocol carries evdev scancodes and this is an evdev
        // device, so no translation is needed -- the macOS backend's
        // styx_keymap round trip exists only to reach macOS virtual keycodes.
        if code == 0 || code > MAX_KEY_CODE as u32 {
            log::warn!("key code {code} outside the virtual keyboard's range; dropping");
            return;
        }
        if pressed {
            self.held_keys.insert(code);
        } else {
            self.held_keys.remove(&code);
        }
        let value = if pressed { 1 } else { 0 };
        if let Err(e) = self
            .keyboard
            .emit(&[InputEvent::new(EventType::KEY.0, code as u16, value)])
        {
            log::error!("uinput key emit failed: {e}");
        }
    }

    /// Inject scroll. `axis` is 0 for vertical, 1 for horizontal.
    ///
    /// Incoming values are treated as evdev hi-res units (120 per detent),
    /// which is what the sender produces from Wayland `AxisValue120` events.
    /// Both a hi-res and a whole-detent event are emitted: clients that
    /// understand `REL_WHEEL_HI_RES` get smooth scrolling, everything else
    /// falls back to `REL_WHEEL`. Sub-detent remainders accumulate rather
    /// than being truncated, so slow trackpad scrolling still moves.
    ///
    /// No sign flip here, unlike the macOS backend: both ends are evdev, and
    /// evdev's convention (positive is up/right) already matches.
    pub fn inject_scroll(&mut self, axis: u8, value: f64) {
        let vertical = axis == 0;
        let (hi_res_code, wheel_code) = if vertical {
            (RelativeAxisCode::REL_WHEEL_HI_RES, RelativeAxisCode::REL_WHEEL)
        } else {
            (RelativeAxisCode::REL_HWHEEL_HI_RES, RelativeAxisCode::REL_HWHEEL)
        };

        let residual = if vertical {
            &mut self.scroll_residual.0
        } else {
            &mut self.scroll_residual.1
        };
        *residual += value;
        let detents = (*residual / HI_RES_PER_DETENT).trunc();
        *residual -= detents * HI_RES_PER_DETENT;

        let mut events = vec![InputEvent::new(
            EventType::RELATIVE.0,
            hi_res_code.0,
            value.round() as i32,
        )];
        if detents != 0.0 {
            events.push(InputEvent::new(
                EventType::RELATIVE.0,
                wheel_code.0,
                detents as i32,
            ));
        }
        if let Err(e) = self.pointer.emit(&events) {
            log::error!("uinput scroll emit failed: {e}");
        }
    }

    /// Release every key and button currently held.
    ///
    /// This is the stuck-key guard: it runs on crossover away, on disconnect,
    /// and on shutdown. Without it a modifier held at the moment the cursor
    /// leaves would stay latched on this machine with no way to clear it,
    /// which is the exact failure that motivated styx over lan-mouse.
    pub fn release_all_keys(&mut self) {
        let keys: Vec<u32> = self.held_keys.drain().collect();
        if !keys.is_empty() {
            let events: Vec<InputEvent> = keys
                .iter()
                .map(|&c| InputEvent::new(EventType::KEY.0, c as u16, 0))
                .collect();
            if let Err(e) = self.keyboard.emit(&events) {
                log::error!("uinput key release failed: {e}");
            }
        }

        let buttons: Vec<u32> = self.held_buttons.drain().collect();
        if !buttons.is_empty() {
            let events: Vec<InputEvent> = buttons
                .iter()
                .map(|&b| InputEvent::new(EventType::KEY.0, b as u16, 0))
                .collect();
            if let Err(e) = self.pointer.emit(&events) {
                log::error!("uinput button release failed: {e}");
            }
        }
    }

    /// Place the cursor at the entry edge, `from_bottom` pixels up from the
    /// bottom of the combined edge span.
    pub fn place_cursor_from_bottom(&mut self, from_bottom: f64) {
        if let Some((x, y)) = geometry::place_from_bottom(
            &self.edge_displays,
            self.edge_span,
            self.return_edge,
            from_bottom,
        ) {
            self.cursor_pos = (x, y);
            self.emit_cursor();
        }
    }

    /// The cursor's pixel distance from the bottom of the edge span, and the
    /// span's height.
    pub fn cursor_from_bottom(&self) -> (f64, f64) {
        geometry::from_bottom_of(
            self.edge_span,
            self.return_edge,
            self.cursor_pos.0,
            self.cursor_pos.1,
        )
    }
}

impl Drop for Injector {
    fn drop(&mut self) {
        // Best effort: never leave a modifier latched on the way out.
        self.release_all_keys();
    }
}

/// Map a desktop coordinate onto the virtual device's absolute axis range.
/// Free function so it can be tested without opening `/dev/uinput`.
fn to_abs_in(bounds: DisplayBounds, x: f64, y: f64) -> (i32, i32) {
    let w = (bounds.max_x - bounds.min_x).max(1.0);
    let h = (bounds.max_y - bounds.min_y).max(1.0);
    let nx = ((x - bounds.min_x) / w).clamp(0.0, 1.0);
    let ny = ((y - bounds.min_y) / h).clamp(0.0, 1.0);
    (
        (nx * ABS_MAX as f64).round() as i32,
        (ny * ABS_MAX as f64).round() as i32,
    )
}

/// Virtual absolute pointing device: position, buttons, and scroll.
fn build_pointer_device() -> Result<VirtualDevice, Box<dyn std::error::Error>> {
    let mut buttons = AttributeSet::<KeyCode>::new();
    for b in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA] {
        buttons.insert(KeyCode::new(b as u16));
    }

    let mut rel = AttributeSet::<RelativeAxisCode>::new();
    rel.insert(RelativeAxisCode::REL_WHEEL);
    rel.insert(RelativeAxisCode::REL_HWHEEL);
    rel.insert(RelativeAxisCode::REL_WHEEL_HI_RES);
    rel.insert(RelativeAxisCode::REL_HWHEEL_HI_RES);

    // value, minimum, maximum, fuzz, flat, resolution. Fuzz and flat stay at
    // zero: this is a synthetic device with no jitter to filter, and any
    // deadzone would quantise cursor placement.
    let abs = AbsInfo::new(0, 0, ABS_MAX, 0, 0, 0);

    let device = VirtualDevice::builder()
        .map_err(uinput_error)?
        .name("styx-receiver pointer")
        .with_keys(&buttons)?
        .with_relative_axes(&rel)?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_X, abs))?
        .with_absolute_axis(&UinputAbsSetup::new(AbsoluteAxisCode::ABS_Y, abs))?
        .build()?;
    Ok(device)
}

/// Virtual keyboard covering the standard key range.
fn build_keyboard_device() -> Result<VirtualDevice, Box<dyn std::error::Error>> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for code in 1..=MAX_KEY_CODE {
        keys.insert(KeyCode::new(code));
    }

    let device = VirtualDevice::builder()
        .map_err(uinput_error)?
        .name("styx-receiver keyboard")
        .with_keys(&keys)?
        .build()?;
    Ok(device)
}

/// Turn the usual `/dev/uinput` failure into an actionable message. Permission
/// denied here is by far the most common setup problem.
fn uinput_error(e: std::io::Error) -> Box<dyn std::error::Error> {
    if e.kind() == std::io::ErrorKind::PermissionDenied {
        format!(
            "cannot open /dev/uinput: {e}. Add your user to a group with write \
             access (commonly `input`) and install a udev rule granting it, or \
             run as root. See docs/linux-receiver.md."
        )
        .into()
    } else if e.kind() == std::io::ErrorKind::NotFound {
        format!(
            "/dev/uinput does not exist: {e}. Load the module with \
             `sudo modprobe uinput` and persist it via /etc/modules-load.d/."
        )
        .into()
    } else {
        format!("failed to open /dev/uinput: {e}").into()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db(min_x: f64, min_y: f64, max_x: f64, max_y: f64) -> DisplayBounds {
        DisplayBounds { min_x, min_y, max_x, max_y }
    }

    #[test]
    fn origin_maps_to_zero() {
        assert_eq!(to_abs_in(db(0.0, 0.0, 1920.0, 1080.0), 0.0, 0.0), (0, 0));
    }

    #[test]
    fn far_corner_maps_to_max() {
        assert_eq!(
            to_abs_in(db(0.0, 0.0, 1920.0, 1080.0), 1920.0, 1080.0),
            (ABS_MAX, ABS_MAX),
        );
    }

    #[test]
    fn centre_maps_to_half() {
        let (x, y) = to_abs_in(db(0.0, 0.0, 1920.0, 1080.0), 960.0, 540.0);
        assert!((x - ABS_MAX / 2).abs() <= 1, "x={x}");
        assert!((y - ABS_MAX / 2).abs() <= 1, "y={y}");
    }

    // A layout whose origin is negative (a display placed left of the primary)
    // must still map onto the full axis range rather than clipping everything
    // left of zero.
    #[test]
    fn negative_origin_layout_maps_across_full_range() {
        let b = db(-1920.0, 0.0, 1920.0, 1080.0);
        assert_eq!(to_abs_in(b, -1920.0, 0.0).0, 0);
        assert_eq!(to_abs_in(b, 1920.0, 0.0).0, ABS_MAX);
        let mid = to_abs_in(b, 0.0, 0.0).0;
        assert!((mid - ABS_MAX / 2).abs() <= 1, "origin should sit mid-range, got {mid}");
    }

    // Out-of-range input is clamped, never wrapped into a bogus axis value.
    #[test]
    fn out_of_range_is_clamped() {
        let b = db(0.0, 0.0, 1920.0, 1080.0);
        assert_eq!(to_abs_in(b, -500.0, -500.0), (0, 0));
        assert_eq!(to_abs_in(b, 99999.0, 99999.0), (ABS_MAX, ABS_MAX));
    }

    // A degenerate zero-extent layout must not divide by zero.
    #[test]
    fn degenerate_extent_does_not_panic() {
        let (x, y) = to_abs_in(db(5.0, 5.0, 5.0, 5.0), 5.0, 5.0);
        assert!((0..=ABS_MAX).contains(&x));
        assert!((0..=ABS_MAX).contains(&y));
    }

    // The keyboard must never claim a BTN_* code, or libinput may classify it
    // as a pointing device and route its events oddly.
    #[test]
    fn keyboard_range_excludes_button_codes() {
        assert!((MAX_KEY_CODE as u32) < 0x100, "BTN_MISC starts at 0x100");
        assert!(BTN_LEFT > MAX_KEY_CODE as u32);
    }

    // Sub-detent scroll must accumulate: six 20-unit deltas make exactly one
    // 120-unit detent, with nothing left over.
    #[test]
    fn scroll_residual_accumulates_to_one_detent() {
        let mut residual = 0.0f64;
        let mut detents_total = 0.0;
        for _ in 0..6 {
            residual += 20.0;
            let d = (residual / HI_RES_PER_DETENT).trunc();
            residual -= d * HI_RES_PER_DETENT;
            detents_total += d;
        }
        assert_eq!(detents_total, 1.0);
        assert!(residual.abs() < 1e-9, "residual should be spent, got {residual}");
    }
}
