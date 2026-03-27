mod capture;
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
#[command(name = "styx-sender", about = "Styx software KVM sender")]
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
    receiver_host: String,
    receiver_port: u16,
    monitor: String,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let cli = Cli::parse();
    let config = load_config(&cli.config)?;
    let edge = parse_edge(&config.sender.edge)?;

    let kbd_path = match &config.sender.keyboard_device {
        Some(path) => PathBuf::from(path),
        None => {
            let detected = detect_keyboard()?;
            log::info!("auto-detected keyboard: {}", detected.display());
            detected
        }
    };

    let addr: SocketAddr = format!(
        "{}:{}",
        config.sender.receiver_host, config.sender.receiver_port
    )
    .parse()?;

    let mut transport = SenderTransport::new(addr);
    let mut wayland_capture = capture::Capture::new(&config.sender.monitor, edge)?;
    let mut evdev_capture = EvdevCapture::open(&kbd_path)?;
    let async_evdev = AsyncEvdev::new(&evdev_capture)?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let mut capturing = false;
    let mut return_cooldown: Option<time::Instant> = None;

    log::info!("styx-sender running (monitor={}, edge={:?})", config.sender.monitor, edge);

    // Outer loop: connect, run event loop, reconnect on failure.
    'outer: loop {
        transport.connect().await?;
        // Brief settle time so the receiver's event loop can start
        // processing before we fire recv().
        time::sleep(Duration::from_millis(50)).await;

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
                        CaptureEvent::Begin { edge_fraction } => {
                            if capturing {
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
                                wayland_capture.release();
                                continue;
                            }

                            for code in evdev_capture.held_modifiers() {
                                let _ = transport.send(&Event::KeyPress { code }).await;
                            }
                            let _ = transport.send(&Event::CaptureBegin { edge_fraction }).await;
                            log::info!("capture active");
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

                _ = async_evdev.readable(), if capturing => {
                    let events = evdev_capture.read_events();
                    for event in events {
                        if let Err(e) = transport.send(&event).await {
                            log::error!("send error: {e}");
                            release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;
                            break; // reconnect
                        }
                    }
                }

                result = transport.recv(), if transport.is_connected() => {
                    match result {
                        Ok(Event::ReturnToSender { edge_fraction }) => {
                            log::info!("return signal received (fraction={edge_fraction:.2})");
                            release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;

                            if let Ok(geom) = hyprland::get_monitor(&config.sender.monitor).await {
                                let x = geom.x + 100;
                                let y = geom.y + (edge_fraction * geom.height as f64) as i32;
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
                    break 'outer;
                }
                _ = sigint.recv() => {
                    log::info!("SIGINT received, shutting down");
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
    Ok(())
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
