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
    keyboard_device: String,
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let cli = Cli::parse();
    let config = load_config(&cli.config)?;
    let edge = parse_edge(&config.sender.edge)?;

    let addr: SocketAddr = format!(
        "{}:{}",
        config.sender.receiver_host, config.sender.receiver_port
    )
    .parse()?;

    let mut transport = SenderTransport::new(addr);
    let mut wayland_capture = capture::Capture::new(&config.sender.monitor, edge)?;
    let mut evdev_capture = EvdevCapture::open(&PathBuf::from(&config.sender.keyboard_device))?;
    let async_evdev = AsyncEvdev::new(&evdev_capture)?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    let mut capturing = false;
    let mut missed_heartbeats: u32 = 0;
    let mut heartbeat_interval = time::interval(Duration::from_millis(
        config.sender.heartbeat.idle_interval_ms,
    ));
    let mut return_cooldown: Option<time::Instant> = None;

    // Initial connection.
    transport.connect().await?;

    log::info!("styx-sender running (monitor={}, edge={:?})", config.sender.monitor, edge);

    loop {
        tokio::select! {
            // Wayland capture events (mouse motion, buttons, scroll, capture begin).
            event = poll_fn(|cx| wayland_capture.poll_event(cx)) => {
                let Some(event) = event else {
                    log::error!("wayland capture ended");
                    break;
                };
                match event {
                    CaptureEvent::Begin => {
                        if let Some(cooldown_until) = return_cooldown {
                            if time::Instant::now() < cooldown_until {
                                wayland_capture.release();
                                continue;
                            }
                        }
                        return_cooldown = None;
                        capturing = true;
                        heartbeat_interval = time::interval(Duration::from_millis(
                            config.sender.heartbeat.active_interval_ms,
                        ));
                        missed_heartbeats = 0;

                        // Grab keyboard via evdev.
                        if let Err(e) = evdev_capture.grab() {
                            log::error!("evdev grab failed: {e}");
                            capturing = false;
                            wayland_capture.release();
                            continue;
                        }

                        // Send modifier state so the Mac starts correct.
                        for code in evdev_capture.held_modifiers() {
                            let _ = transport.send(&Event::KeyPress { code }).await;
                        }
                        let _ = transport.send(&Event::CaptureBegin).await;
                        log::info!("capture active");
                    }
                    CaptureEvent::Input(event) => {
                        if capturing {
                            if let Err(e) = transport.send(&event).await {
                                log::error!("send error: {e}");
                                end_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport, &config.sender.heartbeat).await;
                            }
                        }
                    }
                }
            }

            // evdev keyboard events (only meaningful while capturing).
            _ = async_evdev.readable(), if capturing => {
                let events = evdev_capture.read_events();
                for event in events {
                    if let Err(e) = transport.send(&event).await {
                        log::error!("send error: {e}");
                        end_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport, &config.sender.heartbeat).await;
                        break;
                    }
                }
            }

            // TCP messages from receiver (ReturnToSender, HeartbeatAck).
            result = transport.recv(), if transport.is_connected() => {
                match result {
                    Ok(Event::ReturnToSender) => {
                        log::info!("return signal received");
                        end_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport, &config.sender.heartbeat).await;

                        // Warp cursor back onto the monitor.
                        if let Ok(geom) = hyprland::get_monitor(&config.sender.monitor).await {
                            let x = geom.x + 100;
                            let y = geom.y + geom.height / 2;
                            let _ = hyprland::warp_cursor(x, y).await;
                        }

                        return_cooldown = Some(time::Instant::now() + Duration::from_millis(100));
                    }
                    Ok(Event::HeartbeatAck) => {
                        missed_heartbeats = 0;
                    }
                    Ok(_) => {}
                    Err(e) => {
                        log::error!("recv error: {e}");
                        end_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport, &config.sender.heartbeat).await;
                        transport.disconnect();
                        transport.connect().await?;
                    }
                }
            }

            // Heartbeat.
            _ = heartbeat_interval.tick(), if transport.is_connected() => {
                if missed_heartbeats >= config.sender.heartbeat.miss_threshold {
                    log::warn!("heartbeat timeout, connection dead");
                    end_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport, &config.sender.heartbeat).await;
                    transport.disconnect();
                    transport.connect().await?;
                    missed_heartbeats = 0;
                } else {
                    let _ = transport.send(&Event::Heartbeat).await;
                    missed_heartbeats += 1;
                }
            }

            // Signal handling.
            _ = sigterm.recv() => {
                log::info!("SIGTERM received, shutting down");
                break;
            }
            _ = sigint.recv() => {
                log::info!("SIGINT received, shutting down");
                break;
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

async fn end_capture(
    capturing: &mut bool,
    evdev: &mut EvdevCapture,
    wayland: &mut capture::Capture,
    transport: &mut SenderTransport,
    heartbeat_config: &HeartbeatConfig,
) {
    if !*capturing {
        return;
    }
    *capturing = false;

    // Release all held keys on the Mac side.
    let release_events = evdev.release_all();
    for event in &release_events {
        let _ = transport.send(event).await;
    }
    let _ = transport.send(&Event::CaptureEnd).await;

    // Release evdev grab and wayland capture.
    let _ = evdev.ungrab();
    wayland.release();

    log::info!("capture ended");
}
