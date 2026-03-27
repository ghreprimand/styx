use std::collections::HashSet;
use std::os::fd::AsRawFd;
use std::path::Path;

use evdev::{Device, EventSummary, KeyCode};
use tokio::io::unix::AsyncFd;

use styx_proto::Event;

pub struct EvdevCapture {
    device: Device,
    held_keys: HashSet<u32>,
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
        Ok(EvdevCapture {
            device,
            held_keys: HashSet::new(),
            grabbed: false,
        })
    }

    pub fn grab(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if !self.grabbed {
            self.device.grab()?;
            self.grabbed = true;
            log::debug!("evdev grab acquired");
        }
        Ok(())
    }

    pub fn ungrab(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        if self.grabbed {
            self.device.ungrab()?;
            self.grabbed = false;
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

    pub fn read_events(&mut self) -> Vec<Event> {
        let Ok(events) = self.device.fetch_events() else {
            return vec![];
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
                    _ => {} // repeat (2) ignored
                }
            }
        }
        out
    }
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

    pub async fn readable(&self) -> Result<(), std::io::Error> {
        let mut guard = self.fd.readable().await?;
        guard.retain_ready();
        Ok(())
    }
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
