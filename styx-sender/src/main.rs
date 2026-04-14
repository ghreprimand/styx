mod capture;
mod clipboard;
mod evdev;
mod hyprland;
mod transport;

use std::future::poll_fn;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time;

use styx_proto::Event;

use capture::{CaptureEvent, Edge};
use evdev::{AsyncEvdev, EvdevCapture};
use transport::SenderTransport;

#[derive(Parser)]
#[command(name = "styx-sender", about = "Styx software KVM sender", version)]
struct Cli {
    #[arg(short, long, default_value = "~/.config/styx/config.toml")]
    config: String,
}

#[derive(Deserialize)]
struct Config {
    sender: SenderConfig,
}

#[derive(Deserialize)]
struct SenderConfig {
    /// Single receiver address (kept for backwards compatibility).
    #[serde(default)]
    receiver_host: Option<String>,
    /// Multiple receiver addresses; the sender tries each in order.
    #[serde(default)]
    receiver_hosts: Option<Vec<String>>,
    receiver_port: u16,
    /// Single monitor name (kept for backwards compatibility).
    #[serde(default)]
    monitor: Option<String>,
    /// Multiple monitor names; the sender creates a layer surface on
    /// each one's configured edge and treats the union as one virtual
    /// edge for cursor mapping.
    #[serde(default)]
    monitors: Option<Vec<String>>,
    edge: String,
    #[serde(default)]
    keyboard_device: Option<String>,
    #[serde(default)]
    heartbeat: HeartbeatConfig,
}

#[derive(Deserialize)]
struct HeartbeatConfig {
    #[serde(default = "default_active_ms")]
    active_interval_ms: u64,
    #[serde(default = "default_idle_ms")]
    idle_interval_ms: u64,
    #[serde(default = "default_miss_threshold")]
    miss_threshold: u32,
}

impl Default for HeartbeatConfig {
    fn default() -> Self {
        HeartbeatConfig {
            active_interval_ms: default_active_ms(),
            idle_interval_ms: default_idle_ms(),
            miss_threshold: default_miss_threshold(),
        }
    }
}

fn default_active_ms() -> u64 { 1000 }
fn default_idle_ms() -> u64 { 5000 }
fn default_miss_threshold() -> u32 { 3 }

fn parse_edge(s: &str) -> Result<Edge, String> {
    match s.to_lowercase().as_str() {
        "left" => Ok(Edge::Left),
        "right" => Ok(Edge::Right),
        "top" => Ok(Edge::Top),
        "bottom" => Ok(Edge::Bottom),
        _ => Err(format!("invalid edge: '{}' (expected left/right/top/bottom)", s)),
    }
}

fn expand_path(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
        #[cfg(unix)]
        unsafe {
            let uid = libc::getuid();
            let pw = libc::getpwuid(uid);
            if !pw.is_null() {
                let dir = std::ffi::CStr::from_ptr((*pw).pw_dir);
                if let Ok(s) = dir.to_str() {
                    return PathBuf::from(s).join(rest);
                }
            }
        }
    }
    PathBuf::from(path)
}

fn load_config(path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let path = expand_path(path);
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read config at {}: {e}", path.display()))?;
    let config: Config = toml::from_str(&contents)?;
    Ok(config)
}

fn detect_keyboard() -> Result<PathBuf, Box<dyn std::error::Error>> {
    let by_id = std::fs::read_dir("/dev/input/by-id/")?;
    let mut candidates: Vec<PathBuf> = by_id
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            let name = p.file_name().unwrap_or_default().to_string_lossy();
            name.contains("kbd") && name.contains("event") && !name.contains("if0")
        })
        .collect();
    candidates.sort();
    candidates
        .into_iter()
        .next()
        .ok_or_else(|| "no keyboard found in /dev/input/by-id/; set keyboard_device in config".into())
}

fn resolve_monitors(cfg: &SenderConfig) -> Result<Vec<String>, String> {
    if let Some(list) = &cfg.monitors {
        if list.is_empty() {
            return Err("`monitors` is empty; list at least one monitor".into());
        }
        return Ok(list.clone());
    }
    if let Some(m) = &cfg.monitor {
        return Ok(vec![m.clone()]);
    }
    Err("config must set either `monitor` or `monitors` in [sender]".into())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let cli = Cli::parse();
    let config = load_config(&cli.config)?;
    let edge = parse_edge(&config.sender.edge)?;
    let monitors = resolve_monitors(&config.sender)?;

    let kbd_path = match &config.sender.keyboard_device {
        Some(path) => PathBuf::from(path),
        None => {
            let detected = detect_keyboard()?;
            log::info!("auto-detected keyboard: {}", detected.display());
            detected
        }
    };

    let mut hosts: Vec<String> = Vec::new();
    if let Some(host) = &config.sender.receiver_host {
        hosts.push(host.clone());
    }
    if let Some(extra) = &config.sender.receiver_hosts {
        for h in extra {
            if !hosts.contains(h) {
                hosts.push(h.clone());
            }
        }
    }
    if hosts.is_empty() {
        return Err("config must set receiver_host or receiver_hosts".into());
    }

    let addrs: Vec<SocketAddr> = hosts
        .iter()
        .map(|h| format!("{h}:{}", config.sender.receiver_port).parse())
        .collect::<Result<_, _>>()?;

    let mut transport = SenderTransport::new(addrs);
    let mut wayland_capture = capture::Capture::new(&monitors, edge)?;
    let mut evdev_capture = EvdevCapture::open(&kbd_path)?;
    let mut async_evdev = AsyncEvdev::new(&evdev_capture)?;
    let mut kbd_available = true;
    let mut kbd_recover_interval = time::interval(Duration::from_secs(2));
    kbd_recover_interval.tick().await;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let mut capturing = false;
    let mut return_cooldown: Option<time::Instant>;
    let mut last_clip_hash: u64 = 0;
    let mut clean_exit = false;

    clipboard::check_tools();
    log::info!("styx-sender running (monitors={:?}, edge={:?})", monitors, edge);

    // Outer loop: connect, run event loop, reconnect on failure.
    'outer: loop {
        transport.connect().await?;
        // Brief settle time so the receiver's event loop can start
        // processing before we fire recv().
        time::sleep(Duration::from_millis(50)).await;

        // Block capture for a short window after connecting. Queued
        // Wayland pointer-enter events from the edge surface can fire
        // immediately and grab the keyboard while the user is typing
        // locally.
        return_cooldown = Some(time::Instant::now() + Duration::from_millis(500));

        let mut missed_heartbeats: u32 = 0;
        let mut heartbeat_interval = time::interval(Duration::from_millis(
            config.sender.heartbeat.idle_interval_ms,
        ));
        // Consume the first immediate tick so it doesn't fire right away.
        heartbeat_interval.tick().await;

        // Inner loop: process events on a live connection.
        loop {
            tokio::select! {
                event = poll_fn(|cx| wayland_capture.poll_event(cx)) => {
                    let Some(event) = event else {
                        log::error!("wayland capture ended");
                        break 'outer;
                    };
                    match event {
                        CaptureEvent::Begin { from_bottom, source_height } => {
                            if capturing || !kbd_available {
                                if !kbd_available {
                                    wayland_capture.release();
                                }
                                continue;
                            }
                            if let Some(cooldown_until) = return_cooldown {
                                if time::Instant::now() < cooldown_until {
                                    wayland_capture.release();
                                    continue;
                                }
                            }
                            return_cooldown = None;
                            capturing = true;
                            missed_heartbeats = 0;
                            heartbeat_interval = time::interval(Duration::from_millis(
                                config.sender.heartbeat.active_interval_ms,
                            ));
                            heartbeat_interval.tick().await;

                            if let Err(e) = evdev_capture.grab() {
                                log::error!("evdev grab failed: {e}");
                                capturing = false;
                                kbd_available = false;
                                wayland_capture.release();
                                log::warn!("keyboard device lost (grab failed)");
                                continue;
                            }

                            for code in evdev_capture.held_modifiers() {
                                let _ = transport.send(&Event::KeyPress { code }).await;
                            }
                            let _ = transport.send(&Event::CaptureBegin { from_bottom, source_height }).await;
                            log::info!("capture active");

                            if let Some(text) = clipboard::read_clipboard().await {
                                let h = clipboard::hash_text(&text);
                                if h != last_clip_hash {
                                    last_clip_hash = h;
                                    let _ = transport.send(&Event::ClipboardData { text }).await;
                                    log::debug!("sent clipboard to receiver");
                                }
                            }
                        }
                        CaptureEvent::Released => {
                            if capturing {
                                log::warn!("compositor forced pointer release, ending capture");
                                release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;
                            }
                        }
                        CaptureEvent::Input(event) => {
                            if capturing {
                                if let Err(e) = transport.send(&event).await {
                                    log::error!("send error: {e}");
                                    release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;
                                    break; // reconnect
                                }
                            }
                        }
                    }
                }

                _ = async_evdev.readable(), if capturing && kbd_available => {
                    match evdev_capture.read_events() {
                        Some(events) => {
                            for event in events {
                                if let Err(e) = transport.send(&event).await {
                                    log::error!("send error: {e}");
                                    release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;
                                    break; // reconnect
                                }
                            }
                        }
                        None => {
                            log::warn!("keyboard device lost");
                            release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;
                            kbd_available = false;
                        }
                    }
                }

                _ = kbd_recover_interval.tick(), if !kbd_available => {
                    match EvdevCapture::open(&kbd_path) {
                        Ok(capture) => match AsyncEvdev::new(&capture) {
                            Ok(ae) => {
                                evdev_capture = capture;
                                async_evdev = ae;
                                kbd_available = true;
                                log::info!("keyboard device recovered");
                            }
                            Err(e) => log::debug!("keyboard async fd failed: {e}"),
                        },
                        Err(_) => {}
                    }
                }

                result = transport.recv(), if transport.is_connected() => {
                    match result {
                        Ok(Event::ReturnToSender { from_bottom, source_height }) => {
                            wayland_capture.set_max_from_bottom(source_height);
                            log::info!("return signal received (from_bottom={from_bottom:.0})");
                            release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;

                            let mut geoms: Vec<hyprland::MonitorGeometry> = Vec::new();
                            for name in &monitors {
                                if let Ok(g) = hyprland::get_monitor(name).await {
                                    geoms.push(g);
                                }
                            }
                            if !geoms.is_empty() {
                                // Compute the combined Y span across all configured monitors
                                // and map from_bottom to a global Y, then find the monitor
                                // whose Y range contains that point (clamping to the nearest
                                // if the target falls in a gap).
                                let combined_bottom = geoms.iter().map(|g| g.y + g.height).max().unwrap();
                                let target_y = combined_bottom - from_bottom.round() as i32;
                                let target = geoms.iter()
                                    .find(|g| target_y >= g.y && target_y < g.y + g.height)
                                    .or_else(|| geoms.iter().min_by_key(|g| {
                                        let mid = g.y + g.height / 2;
                                        (mid - target_y).abs()
                                    }))
                                    .unwrap();
                                let y = target_y.clamp(target.y, target.y + target.height - 1);
                                let x = match edge {
                                    capture::Edge::Left => target.x + 2,
                                    capture::Edge::Right => target.x + target.width - 2,
                                    capture::Edge::Top | capture::Edge::Bottom => target.x + target.width / 2,
                                };
                                let _ = hyprland::warp_cursor(x, y).await;
                            }

                            return_cooldown = Some(time::Instant::now() + Duration::from_millis(100));
                            missed_heartbeats = 0;
                            heartbeat_interval = time::interval(Duration::from_millis(
                                config.sender.heartbeat.idle_interval_ms,
                            ));
                            heartbeat_interval.tick().await;
                        }
                        Ok(Event::HeartbeatAck) => {
                            missed_heartbeats = 0;
                        }
                        Ok(Event::ClipboardData { text }) => {
                            log::debug!("received clipboard from receiver");
                            clipboard::write_clipboard(&text).await;
                        }
                        Ok(_) => {}
                        Err(e) => {
                            log::error!("recv error: {e}");
                            release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;
                            transport.disconnect();
                            time::sleep(Duration::from_secs(1)).await;
                            break; // reconnect
                        }
                    }
                }

                _ = heartbeat_interval.tick(), if transport.is_connected() => {
                    if missed_heartbeats >= config.sender.heartbeat.miss_threshold {
                        log::warn!("heartbeat timeout, connection dead");
                        release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;
                        transport.disconnect();
                        time::sleep(Duration::from_secs(1)).await;
                        break; // reconnect
                    } else {
                        let _ = transport.send(&Event::Heartbeat).await;
                        missed_heartbeats += 1;
                    }
                }

                _ = sigterm.recv() => {
                    log::info!("SIGTERM received, shutting down");
                    clean_exit = true;
                    break 'outer;
                }
                _ = sigint.recv() => {
                    log::info!("SIGINT received, shutting down");
                    clean_exit = true;
                    break 'outer;
                }
            }
        }
    }

    // Graceful shutdown.
    if capturing {
        let release_events = evdev_capture.release_all();
        for event in &release_events {
            let _ = transport.send(event).await;
        }
        let _ = transport.send(&Event::CaptureEnd).await;
        let _ = evdev_capture.ungrab();
    }
    transport.disconnect();
    log::info!("shutdown complete");

    if clean_exit {
        Ok(())
    } else {
        Err("wayland connection lost".into())
    }
}

async fn release_capture(
    capturing: &mut bool,
    evdev: &mut EvdevCapture,
    wayland: &mut capture::Capture,
    transport: &mut SenderTransport,
) {
    if !*capturing {
        return;
    }
    *capturing = false;

    let release_events = evdev.release_all();
    for event in &release_events {
        let _ = transport.send(event).await;
    }
    let _ = transport.send(&Event::CaptureEnd).await;
    let _ = evdev.ungrab();
    wayland.release();
    log::info!("capture ended");
}
