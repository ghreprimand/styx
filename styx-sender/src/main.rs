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

    // Tracks rapid compositor force-release cycles so we can back off.
    // Each time the compositor pulls our pointer grab while we were
    // actively capturing, we arm return_cooldown so the next pointer
    // Enter is ignored; if the releases keep happening in quick
    // succession, the cooldown grows exponentially up to 30s to
    // prevent the evdev keyboard grab from cycling hundreds of times
    // per second and locking the user out of typing.
    let mut force_release_streak: u32 = 0;
    let mut force_release_last: Option<time::Instant> = None;

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

                            // Preference order image > html > plain.
                            // Only one clipboard event is sent per crossover.
                            if let Some((format, data)) = clipboard::read_clipboard_image().await {
                                let h = clipboard::hash_image(&format, &data);
                                if h != last_clip_hash {
                                    last_clip_hash = h;
                                    log::info!("sent clipboard image ({}, {} bytes) to receiver", format, data.len());
                                    let _ = transport.send(&Event::ClipboardImage { format, data }).await;
                                }
                            } else if let Some((html, plain)) = clipboard::read_clipboard_html().await {
                                let h = clipboard::hash_html(&html, &plain);
                                if h != last_clip_hash {
                                    last_clip_hash = h;
                                    log::info!(
                                        "sent clipboard html to receiver ({} html bytes, {} plain bytes)",
                                        html.len(), plain.len(),
                                    );
                                    let _ = transport.send(&Event::ClipboardHtml { html, plain }).await;
                                }
                            } else if let Some(text) = clipboard::read_clipboard().await {
                                let h = clipboard::hash_text(&text);
                                if h != last_clip_hash {
                                    last_clip_hash = h;
                                    let _ = transport.send(&Event::ClipboardData { text }).await;
                                    log::debug!("sent clipboard text to receiver");
                                }
                            }
                        }
                        CaptureEvent::Released => {
                            if capturing {
                                log::warn!("compositor forced pointer release, ending capture");
                                release_capture(&mut capturing, &mut evdev_capture, &mut wayland_capture, &mut transport).await;

                                let now = time::Instant::now();
                                let close = force_release_last
                                    .map(|t| now.duration_since(t) < Duration::from_secs(1))
                                    .unwrap_or(false);
                                force_release_streak = if close {
                                    force_release_streak.saturating_add(1)
                                } else {
                                    1
                                };
                                force_release_last = Some(now);
                                let cooldown_ms = match force_release_streak {
                                    1 => 300,
                                    2..=4 => 1_000,
                                    5..=20 => 5_000,
                                    _ => 30_000,
                                };
                                if force_release_streak >= 5 {
                                    log::error!(
                                        "pointer grab contention: {} force-releases in quick succession, backing off {} ms",
                                        force_release_streak,
                                        cooldown_ms,
                                    );
                                }
                                return_cooldown = Some(now + Duration::from_millis(cooldown_ms));
                                // Also silence the Wayland-side Enter
                                // handler for the same window. Without
                                // this, capture.rs keeps re-locking the
                                // pointer on every compositor-driven
                                // Enter, producing hundreds of
                                // Leave-while-locked warnings per
                                // second and starving input events.
                                wayland_capture.suppress_grab_until(
                                    std::time::Instant::now() + Duration::from_millis(cooldown_ms),
                                );
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
                            log::debug!("received clipboard text from receiver ({} bytes)", text.len());
                            last_clip_hash = clipboard::hash_text(&text);
                            clipboard::write_clipboard(&text).await;
                        }
                        Ok(Event::ClipboardImage { format, data }) => {
                            log::info!(
                                "received clipboard image from receiver ({}, {} bytes)",
                                format,
                                data.len(),
                            );
                            last_clip_hash = clipboard::hash_image(&format, &data);
                            clipboard::write_clipboard_image(&format, &data).await;
                        }
                        Ok(Event::ClipboardHtml { html, plain }) => {
                            // wl-copy only accepts a single MIME type per
                            // invocation, so we write the plain-text
                            // fallback and discard the html. Linux apps
                            // overwhelmingly paste text/plain anyway.
                            log::info!(
                                "received clipboard html from receiver ({} html bytes, {} plain bytes); writing plain",
                                html.len(), plain.len(),
                            );
                            last_clip_hash = clipboard::hash_html(&html, &plain);
                            let to_write = if plain.is_empty() { strip_html_tags(&html) } else { plain };
                            clipboard::write_clipboard(&to_write).await;
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

/// Strip HTML tags and collapse whitespace, producing a best-effort
/// plain-text rendering of an HTML fragment. Only used when the sender
/// received a `ClipboardHtml` with an empty `plain` field and must
/// still hand wl-copy something meaningful. Not a full HTML parser --
/// deliberately simple: drop everything between `<` and `>`, decode
/// the five XML entities, collapse runs of whitespace to a single
/// space. Good enough for the typical "user copied a paragraph from a
/// browser and wants to paste plain text into a terminal" case.
fn strip_html_tags(html: &str) -> String {
    let mut out = String::with_capacity(html.len());
    let mut in_tag = false;
    for ch in html.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            c if !in_tag => out.push(c),
            _ => {}
        }
    }
    let decoded = out
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'")
        .replace("&nbsp;", " ");
    decoded.split_whitespace().collect::<Vec<_>>().join(" ")
}
