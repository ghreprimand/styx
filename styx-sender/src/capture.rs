use std::{
    collections::VecDeque,
    env,
    fs::File,
    io::{self, BufWriter, ErrorKind, Write},
    os::fd::{AsFd, AsRawFd, RawFd},
    sync::Arc,
    task::{Context, Poll, ready},
};

use tokio::io::unix::AsyncFd;

use wayland_client::{
    Connection, Dispatch, DispatchError, EventQueue, QueueHandle, WEnum,
    backend::{ReadEventsGuard, WaylandError},
    delegate_noop,
    globals::{GlobalListContents, registry_queue_init},
    protocol::{
        wl_buffer, wl_compositor,
        wl_keyboard::{self, WlKeyboard},
        wl_output::{self, WlOutput},
        wl_pointer::{self, WlPointer},
        wl_region, wl_registry, wl_seat, wl_shm, wl_shm_pool,
        wl_surface::WlSurface,
    },
};

use wayland_protocols::wp::{
    pointer_constraints::zv1::client::{
        zwp_locked_pointer_v1::ZwpLockedPointerV1,
        zwp_pointer_constraints_v1::{Lifetime, ZwpPointerConstraintsV1},
    },
    relative_pointer::zv1::client::{
        zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1,
        zwp_relative_pointer_v1::{self, ZwpRelativePointerV1},
    },
    keyboard_shortcuts_inhibit::zv1::client::{
        zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
        zwp_keyboard_shortcuts_inhibitor_v1::ZwpKeyboardShortcutsInhibitorV1,
    },
};

use wayland_protocols::xdg::xdg_output::zv1::client::{
    zxdg_output_manager_v1::ZxdgOutputManagerV1,
    zxdg_output_v1::{self, ZxdgOutputV1},
};

use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, KeyboardInteractivity, ZwlrLayerSurfaceV1},
};

use styx_proto::Event;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}

impl Edge {
    fn anchor(&self) -> Anchor {
        match self {
            Edge::Left => Anchor::Left | Anchor::Top | Anchor::Bottom,
            Edge::Right => Anchor::Right | Anchor::Top | Anchor::Bottom,
            Edge::Top => Anchor::Top | Anchor::Left | Anchor::Right,
            Edge::Bottom => Anchor::Bottom | Anchor::Left | Anchor::Right,
        }
    }
}

#[derive(Debug)]
pub enum CaptureEvent {
    Begin { edge_fraction: f64 },
    Input(Event),
}

struct Globals {
    compositor: wl_compositor::WlCompositor,
    pointer_constraints: ZwpPointerConstraintsV1,
    relative_pointer_manager: ZwpRelativePointerManagerV1,
    shortcut_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1>,
    seat: wl_seat::WlSeat,
    shm: wl_shm::WlShm,
    layer_shell: ZwlrLayerShellV1,
    xdg_output_manager: ZxdgOutputManagerV1,
}

#[derive(Debug, Clone)]
struct OutputInfo {
    name: String,
    #[allow(dead_code)]
    position: (i32, i32),
    size: (i32, i32),
}

struct Window {
    buffer: wl_buffer::WlBuffer,
    surface: WlSurface,
    layer_surface: ZwlrLayerSurfaceV1,
}

impl Drop for Window {
    fn drop(&mut self) {
        self.layer_surface.destroy();
        self.surface.destroy();
        self.buffer.destroy();
    }
}

struct State {
    pointer: Option<WlPointer>,
    keyboard: Option<WlKeyboard>,
    pointer_lock: Option<ZwpLockedPointerV1>,
    rel_pointer: Option<ZwpRelativePointerV1>,
    shortcut_inhibitor: Option<ZwpKeyboardShortcutsInhibitorV1>,
    window: Option<Arc<Window>>,
    focused: bool,
    g: Globals,
    wayland_fd: RawFd,
    read_guard: Option<ReadEventsGuard>,
    qh: QueueHandle<Self>,
    pending_events: VecDeque<CaptureEvent>,
    output_info: Vec<(WlOutput, OutputInfo)>,
    scroll_discrete_pending: bool,
    edge: Edge,
    monitor_size: (i32, i32),
}

struct Inner {
    state: State,
    queue: EventQueue<State>,
}

impl AsRawFd for Inner {
    fn as_raw_fd(&self) -> RawFd {
        self.state.wayland_fd
    }
}

pub struct Capture {
    inner: AsyncFd<Inner>,
}

impl Capture {
    pub fn new(monitor: &str, edge: Edge) -> Result<Self, Box<dyn std::error::Error>> {
        let conn = Connection::connect_to_env()?;
        let (g, mut queue) = registry_queue_init::<State>(&conn)?;
        let qh = queue.handle();

        let compositor: wl_compositor::WlCompositor = g.bind(&qh, 4..=6, ())?;
        let xdg_output_manager: ZxdgOutputManagerV1 = g.bind(&qh, 1..=3, ())?;
        let shm: wl_shm::WlShm = g.bind(&qh, 1..=1, ())?;
        let layer_shell: ZwlrLayerShellV1 = g.bind(&qh, 3..=4, ())?;
        let seat: wl_seat::WlSeat = g.bind(&qh, 7..=9, ())?;
        let pointer_constraints: ZwpPointerConstraintsV1 = g.bind(&qh, 1..=1, ())?;
        let relative_pointer_manager: ZwpRelativePointerManagerV1 = g.bind(&qh, 1..=1, ())?;

        let shortcut_inhibit_manager: Option<ZwpKeyboardShortcutsInhibitManagerV1> =
            g.bind(&qh, 1..=1, ()).ok();
        if shortcut_inhibit_manager.is_none() {
            log::warn!("shortcut_inhibit_manager not available; compositor keybinds will not be captured");
        }

        let globals = Globals {
            compositor,
            shm,
            layer_shell,
            seat,
            pointer_constraints,
            relative_pointer_manager,
            shortcut_inhibit_manager,
            xdg_output_manager,
        };

        queue.flush()?;
        let wayland_fd = queue.as_fd().as_raw_fd();

        let mut state = State {
            pointer: None,
            keyboard: None,
            g: globals,
            pointer_lock: None,
            rel_pointer: None,
            shortcut_inhibitor: None,
            window: None,
            focused: false,
            qh,
            wayland_fd,
            read_guard: None,
            pending_events: VecDeque::new(),
            output_info: vec![],
            scroll_discrete_pending: false,
            edge,
            monitor_size: (0, 0),
        };

        // Read wl_output globals.
        conn.display().get_registry(&state.qh, ());
        queue.roundtrip(&mut state)?;

        // Query xdg_output info for each output.
        for (output, _) in state.output_info.iter() {
            state.g.xdg_output_manager.get_xdg_output(output, &state.qh, output.clone());
        }
        queue.roundtrip(&mut state)?;

        // Find the target monitor.
        let (target_output, target_info) = state
            .output_info
            .iter()
            .find(|(_, info)| info.name == monitor)
            .ok_or_else(|| format!("monitor '{}' not found", monitor))?
            .clone();

        log::info!("target monitor: {} ({}x{})", target_info.name, target_info.size.0, target_info.size.1);
        state.monitor_size = target_info.size;

        // Create the edge surface.
        let (width, height) = match edge {
            Edge::Left | Edge::Right => (1u32, target_info.size.1 as u32),
            Edge::Top | Edge::Bottom => (target_info.size.0 as u32, 1u32),
        };

        let mut file = tempfile::tempfile()?;
        draw_surface(&mut file, width, height);

        let pool = state.g.shm.create_pool(
            file.as_fd(),
            (width * height * 4) as i32,
            &state.qh,
            (),
        );
        let buffer = pool.create_buffer(
            0,
            width as i32,
            height as i32,
            (width * 4) as i32,
            wl_shm::Format::Argb8888,
            &state.qh,
            (),
        );
        let surface = state.g.compositor.create_surface(&state.qh, ());
        let layer_surface = state.g.layer_shell.get_layer_surface(
            &surface,
            Some(&target_output),
            Layer::Overlay,
            "styx".into(),
            &state.qh,
            (),
        );

        layer_surface.set_anchor(edge.anchor());
        layer_surface.set_size(width, height);
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_margin(0, 0, 0, 0);
        surface.set_input_region(None);
        surface.commit();

        state.window = Some(Arc::new(Window {
            buffer,
            surface,
            layer_surface,
        }));

        queue.flush()?;

        let read_guard = loop {
            match queue.prepare_read() {
                Some(r) => break r,
                None => {
                    queue.dispatch_pending(&mut state)?;
                }
            }
        };
        state.read_guard = Some(read_guard);

        let inner = AsyncFd::new(Inner { queue, state })?;
        Ok(Capture { inner })
    }

    pub fn release(&mut self) {
        let inner = self.inner.get_mut();
        inner.state.ungrab();
        let _ = inner.flush_events();
    }

    pub fn poll_event(&mut self, cx: &mut Context<'_>) -> Poll<Option<CaptureEvent>> {
        if let Some(event) = self.inner.get_mut().state.pending_events.pop_front() {
            return Poll::Ready(Some(event));
        }

        loop {
            let mut guard = match ready!(self.inner.poll_read_ready_mut(cx)) {
                Ok(guard) => guard,
                Err(e) => {
                    log::error!("wayland fd error: {e}");
                    return Poll::Ready(None);
                }
            };

            {
                let inner = guard.get_inner_mut();
                while inner.read() {
                    if let Err(e) = inner.prepare_read() {
                        log::error!("wayland prepare_read error: {e}");
                        return Poll::Ready(None);
                    }
                }
                inner.dispatch_events();
                if let Err(e) = inner.flush_events() {
                    if e.kind() != ErrorKind::WouldBlock {
                        log::error!("wayland flush error: {e}");
                        return Poll::Ready(None);
                    }
                }
                if let Err(e) = inner.prepare_read() {
                    log::error!("wayland prepare_read error: {e}");
                    return Poll::Ready(None);
                }
            }

            guard.clear_ready();

            if let Some(event) = guard.get_inner_mut().state.pending_events.pop_front() {
                return Poll::Ready(Some(event));
            }
        }
    }
}

fn draw_surface(f: &mut File, width: u32, height: u32) {
    let mut buf = BufWriter::new(f);
    let debug = env::var("STYX_DEBUG_SURFACE").is_ok();
    let pixel = if debug { 0xff11d116u32 } else { 0x00000000u32 };
    for _ in 0..(width * height) {
        buf.write_all(&pixel.to_ne_bytes()).unwrap();
    }
}

// -- State methods --

impl State {
    fn grab(&mut self, surface: &WlSurface, pointer: &WlPointer, serial: u32) {
        pointer.set_cursor(serial, None, 0, 0);

        if let Some(window) = &self.window {
            window.layer_surface.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            window.surface.commit();
        }

        if self.pointer_lock.is_none() {
            self.pointer_lock = Some(self.g.pointer_constraints.lock_pointer(
                surface,
                pointer,
                None,
                Lifetime::Persistent,
                &self.qh,
                (),
            ));
        }

        if self.rel_pointer.is_none() {
            self.rel_pointer = Some(
                self.g.relative_pointer_manager.get_relative_pointer(pointer, &self.qh, ()),
            );
        }

        if let Some(manager) = &self.g.shortcut_inhibit_manager {
            if self.shortcut_inhibitor.is_none() {
                self.shortcut_inhibitor =
                    Some(manager.inhibit_shortcuts(surface, &self.g.seat, &self.qh, ()));
            }
        }

        self.focused = true;
    }

    fn ungrab(&mut self) {
        if let Some(window) = &self.window {
            window.layer_surface.set_keyboard_interactivity(KeyboardInteractivity::None);
            window.surface.commit();
        }

        if let Some(lock) = self.pointer_lock.take() {
            lock.destroy();
        }
        if let Some(rel) = self.rel_pointer.take() {
            rel.destroy();
        }
        if let Some(inhibitor) = self.shortcut_inhibitor.take() {
            inhibitor.destroy();
        }

        self.focused = false;
    }
}

impl Inner {
    fn read(&mut self) -> bool {
        match self.state.read_guard.take().unwrap().read() {
            Ok(_) => true,
            Err(WaylandError::Io(e)) if e.kind() == ErrorKind::WouldBlock => false,
            Err(WaylandError::Io(e)) => {
                log::error!("wayland socket read error: {e}");
                false
            }
            Err(WaylandError::Protocol(e)) => {
                panic!("wayland protocol violation: {e}");
            }
        }
    }

    fn prepare_read(&mut self) -> io::Result<()> {
        loop {
            match self.queue.prepare_read() {
                None => match self.queue.dispatch_pending(&mut self.state) {
                    Ok(_) => continue,
                    Err(DispatchError::Backend(WaylandError::Io(e))) => return Err(e),
                    Err(e) => panic!("wayland dispatch error: {e}"),
                },
                Some(r) => {
                    self.state.read_guard = Some(r);
                    return Ok(());
                }
            }
        }
    }

    fn dispatch_events(&mut self) {
        match self.queue.dispatch_pending(&mut self.state) {
            Ok(_) => {}
            Err(DispatchError::Backend(WaylandError::Io(e))) => {
                log::error!("wayland dispatch error: {e}");
            }
            Err(e) => panic!("wayland dispatch error: {e}"),
        }
    }

    fn flush_events(&mut self) -> io::Result<()> {
        match self.queue.flush() {
            Ok(_) => Ok(()),
            Err(WaylandError::Io(e)) => Err(e),
            Err(WaylandError::Protocol(e)) => panic!("wayland protocol violation: {e}"),
        }
    }
}

// -- Wayland dispatch implementations --

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities {
            capabilities: WEnum::Value(caps),
        } = event
        {
            if caps.contains(wl_seat::Capability::Pointer) {
                if let Some(p) = state.pointer.take() {
                    p.release();
                }
                state.pointer = Some(seat.get_pointer(qh, ()));
            }
            if caps.contains(wl_seat::Capability::Keyboard) {
                if let Some(k) = state.keyboard.take() {
                    k.release();
                }
                seat.get_keyboard(qh, ());
            }
        }
    }
}

impl Dispatch<WlPointer, ()> for State {
    fn event(
        state: &mut Self,
        pointer: &WlPointer,
        event: wl_pointer::Event,
        _: &(),
        _: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, surface, surface_x, surface_y, .. } => {
                if let Some(window) = &state.window {
                    if window.surface == surface {
                        let edge_fraction = match state.edge {
                            Edge::Left | Edge::Right => {
                                let h = state.monitor_size.1 as f64;
                                if h > 0.0 { (surface_y / h).clamp(0.0, 1.0) } else { 0.5 }
                            }
                            Edge::Top | Edge::Bottom => {
                                let w = state.monitor_size.0 as f64;
                                if w > 0.0 { (surface_x / w).clamp(0.0, 1.0) } else { 0.5 }
                            }
                        };
                        state.grab(&surface, pointer, serial);
                        state.pending_events.push_back(CaptureEvent::Begin { edge_fraction });
                    }
                }
            }
            wl_pointer::Event::Leave { .. } => {
                if state.pointer_lock.is_some() {
                    log::warn!("compositor released pointer unexpectedly");
                }
                state.ungrab();
            }
            wl_pointer::Event::Button { time: _, button, state: btn_state, .. } => {
                if state.focused {
                    state.pending_events.push_back(CaptureEvent::Input(
                        Event::MouseButton {
                            button,
                            state: u32::from(btn_state) as u8,
                        },
                    ));
                }
            }
            wl_pointer::Event::Axis { time: _, axis, value } => {
                if state.focused && !state.scroll_discrete_pending {
                    state.pending_events.push_back(CaptureEvent::Input(
                        Event::MouseScroll {
                            axis: u32::from(axis) as u8,
                            value,
                        },
                    ));
                } else {
                    state.scroll_discrete_pending = false;
                }
            }
            wl_pointer::Event::AxisValue120 { axis, value120 } => {
                if state.focused {
                    state.scroll_discrete_pending = true;
                    state.pending_events.push_back(CaptureEvent::Input(
                        Event::MouseScroll {
                            axis: u32::from(axis) as u8,
                            value: value120 as f64,
                        },
                    ));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        _state: &mut Self,
        _: &WlKeyboard,
        _event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // Keyboard events from layer-shell are intentionally ignored.
        // Styx uses evdev for keyboard capture to avoid the stuck-key
        // issues that plague layer-shell keyboard forwarding.
    }
}

impl Dispatch<ZwpRelativePointerV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ZwpRelativePointerV1,
        event: zwp_relative_pointer_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwp_relative_pointer_v1::Event::RelativeMotion {
            dx_unaccel: dx,
            dy_unaccel: dy,
            ..
        } = event
        {
            if state.focused {
                state.pending_events.push_back(CaptureEvent::Input(
                    Event::MouseMotion { dx, dy },
                ));
            }
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for State {
    fn event(
        state: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: zwlr_layer_surface_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_layer_surface_v1::Event::Configure { serial, .. } = event {
            if let Some(window) = &state.window {
                if &window.layer_surface == layer_surface {
                    window.surface.attach(Some(&window.buffer), 0, 0);
                    layer_surface.ack_configure(serial);
                    window.surface.commit();
                }
            }
        }
    }
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(
        _: &mut Self,
        _: &wl_registry::WlRegistry,
        _: wl_registry::Event,
        _: &GlobalListContents,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        _registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == "wl_output" {
                let output: WlOutput = _registry.bind(name, version.min(4), qh, ());
                state.output_info.push((
                    output,
                    OutputInfo {
                        name: String::new(),
                        position: (0, 0),
                        size: (0, 0),
                    },
                ));
            }
        }
    }
}

impl Dispatch<WlOutput, ()> for State {
    fn event(
        _: &mut Self,
        _: &WlOutput,
        _: wl_output::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZxdgOutputV1, WlOutput> for State {
    fn event(
        state: &mut Self,
        _: &ZxdgOutputV1,
        event: zxdg_output_v1::Event,
        output: &WlOutput,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let entry = state.output_info.iter_mut().find(|(o, _)| o == output);
        let Some((_, info)) = entry else { return };

        match event {
            zxdg_output_v1::Event::Name { name } => {
                info.name = name;
            }
            zxdg_output_v1::Event::LogicalPosition { x, y } => {
                info.position = (x, y);
            }
            zxdg_output_v1::Event::LogicalSize { width, height } => {
                info.size = (width, height);
            }
            _ => {}
        }
    }
}

impl Dispatch<ZxdgOutputManagerV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &ZxdgOutputManagerV1,
        _: <ZxdgOutputManagerV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore WlSurface);
delegate_noop!(State: ignore wl_shm::WlShm);
delegate_noop!(State: ignore wl_shm_pool::WlShmPool);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore ZwlrLayerShellV1);
delegate_noop!(State: ignore ZwpPointerConstraintsV1);
delegate_noop!(State: ignore ZwpLockedPointerV1);
delegate_noop!(State: ignore ZwpRelativePointerManagerV1);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitManagerV1);
delegate_noop!(State: ignore ZwpKeyboardShortcutsInhibitorV1);
