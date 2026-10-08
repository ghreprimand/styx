use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::task::{Context, Poll};

use evdev::{AttributeSet, Device, EventSummary, EventType, InputEvent, KeyCode};
use evdev::uinput::VirtualDevice;
use tokio::io::unix::AsyncFd;

use styx_proto::Event;

pub struct EvdevCapture {
    device: Device,
    synth: VirtualDevice,
    held_keys: HashSet<u32>,
    keys_at_grab: HashSet<u32>,
    grabbed: bool,
}

impl EvdevCapture {
    pub fn open(path: &Path) -> Result<Self, Box<dyn std::error::Error>> {
        let device = Device::open(path)?;
        log::info!(
            "opened evdev device: {} ({})",
            device.name().unwrap_or("unknown"),
            path.display()
        );

        let mut keys = AttributeSet::<KeyCode>::new();
        if let Some(supported) = device.supported_keys() {
            for key in supported.iter() {
                keys.insert(key);
            }
        }
        let synth = VirtualDevice::builder()?
            .name("styx-synth")
            .with_keys(&keys)?
            .build()?;

        Ok(EvdevCapture {
            device,
            synth,
            held_keys: HashSet::new(),
            keys_at_grab: HashSet::new(),
            grabbed: false,
        })
    }

    pub fn grab(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.grabbed {
            if let Ok(state) = self.device.get_key_state() {
                self.keys_at_grab = state.iter().map(|k| k.code() as u32).collect();
            }
            self.device.grab()?;
            self.grabbed = true;
            log::debug!("evdev grab acquired ({} keys held)", self.keys_at_grab.len());
        }
        Ok(())
    }

    pub fn ungrab(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.grabbed {
            self.device.ungrab()?;
            self.grabbed = false;

            // Keys the compositor saw go down before the grab but that were
            // released while grabbed need synthetic releases injected via
            // uinput, otherwise the compositor considers them stuck.
            let current = self.device.get_key_state().unwrap_or_default();
            let mut released = 0u32;
            for &code in &self.keys_at_grab {
                if !current.contains(KeyCode(code as u16)) {
                    let ev = InputEvent::new(EventType::KEY.0, code as u16, 0);
                    let _ = self.synth.emit(&[ev]);
                    released += 1;
                }
            }
            self.keys_at_grab.clear();
            if released > 0 {
                log::debug!("injected {released} synthetic key releases");
            }
            log::debug!("evdev grab released");
        }
        Ok(())
    }

    pub fn held_modifiers(&self) -> Vec<u32> {
        let Ok(state) = self.device.get_key_state() else {
            return vec![];
        };
        styx_keymap::MODIFIER_KEYS
            .iter()
            .copied()
            .filter(|&code| state.contains(KeyCode(code as u16)))
            .collect()
    }

    pub fn release_all(&mut self) -> Vec<Event> {
        let events: Vec<Event> = self
            .held_keys
            .iter()
            .map(|&code| Event::KeyRelease { code })
            .collect();
        self.held_keys.clear();
        events
    }

    pub fn raw_fd(&self) -> std::os::fd::RawFd {
        self.device.as_raw_fd()
    }

    pub fn read_events(&mut self) -> ReadResult {
        let events = match self.device.fetch_events() {
            Ok(events) => events,
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return ReadResult::WouldBlock,
            Err(e) => {
                log::warn!("evdev read failed: {e}");
                return ReadResult::Lost;
            }
        };

        let mut out = Vec::new();
        for ev in events {
            let summary: EventSummary = ev.into();
            if let EventSummary::Key(_key_ev, key_code, value) = summary {
                let code = key_code.0 as u32;
                match value {
                    1 => {
                        self.held_keys.insert(code);
                        out.push(Event::KeyPress { code });
                    }
                    0 => {
                        self.held_keys.remove(&code);
                        out.push(Event::KeyRelease { code });
                    }
                    2 => {
                        // Kernel auto-repeat. Forward as another key press
                        // since macOS doesn't repeat programmatically posted events.
                        // Suppress repeats for modifier keys -- they cause
                        // duplicate modifier-down events on macOS which triggers
                        // unintended shortcuts and special characters.
                        if !styx_keymap::is_modifier(code) {
                            out.push(Event::KeyPress { code });
                        }
                    }
                    _ => {}
                }
            }
        }
        ReadResult::Events(out)
    }
}

pub enum ReadResult {
    Events(Vec<Event>),
    WouldBlock,
    Lost,
}

pub struct AsyncEvdev {
    fd: AsyncFd<std::os::fd::OwnedFd>,
}

impl AsyncEvdev {
    pub fn new(capture: &EvdevCapture) -> Result<Self, std::io::Error> {
        let duped = dup_fd_nonblock(capture.raw_fd())?;
        Ok(AsyncEvdev {
            fd: AsyncFd::new(duped)?,
        })
    }
}

/// Where the set of captured keyboards comes from.
pub enum KeyboardSource {
    /// A single device from `keyboard_device` in the config.
    Explicit(PathBuf),
    /// Every keyboard in /dev/input/by-id/, rescanned for hotplug.
    Auto,
}

struct Keyboard {
    /// Canonical /dev/input/eventN path, used to avoid opening a device twice.
    node: PathBuf,
    path: PathBuf,
    capture: EvdevCapture,
    fd: AsyncEvdev,
}

pub enum KeyboardRead {
    Events(Vec<Event>),
    /// A device went away. Carries releases for keys it had held.
    Lost(Vec<Event>),
}

/// All keyboards being captured. Grabs, reads, and releases act on every
/// device at once, so a keyboard that switches connection mode (e.g. a
/// tri-mode keyboard moving between its 2.4G dongle and a USB cable)
/// keeps working without reconfiguration.
pub struct KeyboardSet {
    source: KeyboardSource,
    keyboards: Vec<Keyboard>,
    /// Paths that failed to open, so the failure is only logged once.
    failed: HashSet<PathBuf>,
}

impl KeyboardSet {
    pub fn new(source: KeyboardSource) -> Self {
        KeyboardSet { source, keyboards: Vec::new(), failed: HashSet::new() }
    }

    pub fn is_empty(&self) -> bool {
        self.keyboards.is_empty()
    }

    /// Open any wanted keyboards that are not open yet. When `grab` is set,
    /// new devices are grabbed immediately so a keyboard that appears
    /// mid-capture is captured too.
    pub fn rescan(&mut self, grab: bool) {
        let wanted = match &self.source {
            KeyboardSource::Explicit(path) => vec![path.clone()],
            KeyboardSource::Auto => detect_keyboards(),
        };
        for path in wanted {
            let Ok(node) = std::fs::canonicalize(&path) else { continue };
            if self.keyboards.iter().any(|k| k.node == node) {
                continue;
            }
            let opened = EvdevCapture::open(&path)
                .and_then(|capture| Ok((AsyncEvdev::new(&capture)?, capture)));
            let (fd, mut capture) = match opened {
                Ok(v) => v,
                Err(e) => {
                    if self.failed.insert(path.clone()) {
                        log::warn!("failed to open keyboard {}: {e}", path.display());
                    }
                    continue;
                }
            };
            if grab {
                if let Err(e) = capture.grab() {
                    log::warn!("evdev grab failed for {}: {e}", path.display());
                    continue;
                }
            }
            self.failed.remove(&path);
            log::info!("capturing keyboard: {}", path.display());
            self.keyboards.push(Keyboard { node, path, capture, fd });
        }
    }

    /// Grab every keyboard. Devices that fail to grab are dropped; returns
    /// false if none could be grabbed.
    pub fn grab(&mut self) -> bool {
        self.keyboards.retain_mut(|k| match k.capture.grab() {
            Ok(()) => true,
            Err(e) => {
                log::warn!("evdev grab failed for {}, dropping it: {e}", k.path.display());
                false
            }
        });
        !self.keyboards.is_empty()
    }

    pub fn ungrab(&mut self) {
        for k in &mut self.keyboards {
            let _ = k.capture.ungrab();
        }
    }

    pub fn held_modifiers(&self) -> Vec<u32> {
        let mut codes: Vec<u32> = self
            .keyboards
            .iter()
            .flat_map(|k| k.capture.held_modifiers())
            .collect();
        codes.sort_unstable();
        codes.dedup();
        codes
    }

    pub fn release_all(&mut self) -> Vec<Event> {
        self.keyboards
            .iter_mut()
            .flat_map(|k| k.capture.release_all())
            .collect()
    }

    /// Wait for key events from any keyboard.
    pub fn poll_read(&mut self, cx: &mut Context<'_>) -> Poll<KeyboardRead> {
        for i in 0..self.keyboards.len() {
            let k = &mut self.keyboards[i];
            let lost = loop {
                let mut guard = match k.fd.fd.poll_read_ready(cx) {
                    Poll::Pending => break false,
                    Poll::Ready(Err(_)) => break true,
                    Poll::Ready(Ok(guard)) => guard,
                };
                match k.capture.read_events() {
                    ReadResult::WouldBlock => guard.clear_ready(),
                    ReadResult::Events(events) if events.is_empty() => {}
                    ReadResult::Events(events) => return Poll::Ready(KeyboardRead::Events(events)),
                    ReadResult::Lost => break true,
                }
            };
            if lost {
                let mut k = self.keyboards.remove(i);
                log::warn!("keyboard device lost: {}", k.path.display());
                return Poll::Ready(KeyboardRead::Lost(k.capture.release_all()));
            }
        }
        Poll::Pending
    }
}

/// Every keyboard in /dev/input/by-id/. Secondary USB interfaces (`-if0N-`)
/// are skipped: they are typically the keyboard half of a gaming mouse or
/// a receiver's extra HID endpoint rather than a real keyboard.
fn detect_keyboards() -> Vec<PathBuf> {
    let Ok(by_id) = std::fs::read_dir("/dev/input/by-id/") else {
        return Vec::new();
    };
    let mut paths: Vec<PathBuf> = by_id
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.contains("kbd") && name.contains("event") && !name.contains("if0")
        })
        .collect();
    paths.sort();
    paths
}

fn dup_fd_nonblock(raw: std::os::fd::RawFd) -> Result<std::os::fd::OwnedFd, std::io::Error> {
    use std::os::fd::FromRawFd;
    let new_fd = unsafe { libc::dup(raw) };
    if new_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let flags = unsafe { libc::fcntl(new_fd, libc::F_GETFL) };
    unsafe { libc::fcntl(new_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(new_fd) })
}
