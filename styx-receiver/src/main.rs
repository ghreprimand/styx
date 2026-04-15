mod clipboard;
mod clipboard_image;
mod edge;
mod inject;
mod transport;

use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time;

use styx_proto::Event;

use inject::{Edge, Injector};
use transport::ReceiverTransport;

#[derive(Parser)]
#[command(name = "styx-receiver", about = "Styx software KVM receiver", version)]
struct Cli {
    #[arg(short, long, default_value = "~/.config/styx/config.toml")]
    config: String,
}

#[derive(Deserialize)]
struct Config {
    receiver: ReceiverConfig,
}

#[derive(Deserialize)]
struct ReceiverConfig {
    listen_host: String,
    listen_port: u16,
    #[serde(default = "default_return_edge")]
    return_edge: String,
    #[serde(default)]
    swap_alt_cmd: bool,
}

fn default_return_edge() -> String {
    "right".to_string()
}

fn parse_edge(s: &str) -> Result<Edge, String> {
    match s.to_lowercase().as_str() {
        "left" => Ok(Edge::Left),
        "right" => Ok(Edge::Right),
        "top" => Ok(Edge::Top),
        "bottom" => Ok(Edge::Bottom),
        _ => Err(format!("invalid edge: '{}' (expected left/right/top/bottom)", s)),
    }
}

fn expand_path(path: &str) -> std::path::PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return std::path::PathBuf::from(home).join(rest);
        }
        #[cfg(unix)]
        unsafe {
            let uid = libc::getuid();
            let pw = libc::getpwuid(uid);
            if !pw.is_null() {
                let dir = std::ffi::CStr::from_ptr((*pw).pw_dir);
                if let Ok(s) = dir.to_str() {
                    return std::path::PathBuf::from(s).join(rest);
                }
            }
        }
    }
    std::path::PathBuf::from(path)
}

fn load_config(path: &str) -> Result<Config, Box<dyn std::error::Error>> {
    let path = expand_path(path);
    let contents = std::fs::read_to_string(&path)
        .map_err(|e| format!("failed to read config at {}: {e}", path.display()))?;
    let config: Config = toml::from_str(&contents)?;
    Ok(config)
}

/// No data for this long means the connection is dead.
const RECV_TIMEOUT: Duration = Duration::from_secs(15);

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();

    // Check Accessibility permission early so it's obvious in the logs.
    let trusted = unsafe {
        #[link(name = "ApplicationServices", kind = "framework")]
        unsafe extern "C" {
            fn AXIsProcessTrusted() -> bool;
        }
        AXIsProcessTrusted()
    };
    if trusted {
        log::info!("accessibility: granted");
    } else {
        log::error!("accessibility: NOT GRANTED -- input injection will silently fail");
        log::error!("grant permission to Styx Receiver.app in System Settings > Privacy & Security > Accessibility");
    }

    let cli = Cli::parse();
    let config = load_config(&cli.config)?;
    let return_edge = parse_edge(&config.receiver.return_edge)?;

    let addr: SocketAddr = format!(
        "{}:{}",
        config.receiver.listen_host, config.receiver.listen_port
    )
    .parse()?;

    let listener = TcpListener::bind(addr).await?;
    log::info!("listening on {addr}");

    let mut transport = ReceiverTransport::new();
    let mut injector = Injector::new(return_edge, config.receiver.swap_alt_cmd)?;
    if config.receiver.swap_alt_cmd {
        log::info!("modifier remap: Alt->Cmd, Super->Option (swap_alt_cmd=true)");
    }

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;
    let mut last_clip_hash: u64 = 0;

    // Proactive clipboard sync: a background task polls
    // NSPasteboard.changeCount at 10 Hz and forwards any new clipboard
    // content over this channel. The main loop deduplicates against
    // last_clip_hash and performs the transport.send so it remains the
    // sole owner of both. Channel capacity 1: newer clipboard content
    // supersedes any older item still in the queue; try_send drops on
    // full, and the next tick re-reads if something was lost.
    let (clip_tx, mut clip_rx) = tokio::sync::mpsc::channel::<Event>(1);
    tokio::spawn(proactive_clipboard_poll(clip_tx));

    log::info!("styx-receiver running (return_edge={:?})", return_edge);

    // Periodically recreate the CGEventSource and recompute display bounds.
    // This fixes stale event injection after macOS sleep/wake cycles and
    // handles monitor configuration changes.
    let mut reinit_timer = time::interval(Duration::from_secs(30));
    reinit_timer.tick().await; // consume the immediate first tick

    loop {
        // Wait for a connection.
        log::info!("waiting for connection...");
        let (stream, peer) = tokio::select! {
            r = listener.accept() => match r {
                Ok(conn) => conn,
                Err(e) => {
                    log::error!("accept error: {e}");
                    continue;
                }
            },
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        };
        log::info!("accepted connection from {peer}");
        transport.set_stream(stream);

        // Process events on this connection until it dies.
        // Also accept new connections -- if the sender reconnects, drop the
        // stale socket and switch to the new one immediately.
        loop {
            let event = tokio::select! {
                r = time::timeout(RECV_TIMEOUT, transport.recv()) => match r {
                    Ok(Ok(event)) => event,
                    Ok(Err(styx_proto::DecodeError::ConnectionClosed)) => {
                        log::info!("sender disconnected");
                        break;
                    }
                    Ok(Err(e)) => {
                        log::error!("recv error: {e}");
                        break;
                    }
                    Err(_) => {
                        log::warn!("recv timeout, connection dead");
                        break;
                    }
                },
                Some(event) = clip_rx.recv() => {
                    let h = match &event {
                        Event::ClipboardImage { format, data } => Some(clipboard::hash_image(format, data)),
                        Event::ClipboardData { text } => Some(clipboard::hash_text(text)),
                        _ => None,
                    };
                    if let Some(h) = h {
                        if h != last_clip_hash {
                            last_clip_hash = h;
                            match &event {
                                Event::ClipboardImage { format, data } => log::info!(
                                    "proactive clipboard image to sender ({}, {} bytes)",
                                    format, data.len(),
                                ),
                                Event::ClipboardData { text } => log::debug!(
                                    "proactive clipboard text to sender ({} bytes)",
                                    text.len(),
                                ),
                                _ => {}
                            }
                            let _ = transport.send(&event).await;
                        }
                    }
                    continue;
                },
                r = listener.accept() => match r {
                    Ok((stream, peer)) => {
                        log::info!("new connection from {peer}, replacing existing");
                        injector.release_all_keys();
                        transport.disconnect();
                        transport.set_stream(stream);
                        continue;
                    }
                    Err(e) => {
                        log::error!("accept error: {e}");
                        continue;
                    }
                },
                _ = reinit_timer.tick() => {
                    injector.reinit();
                    continue;
                },
                _ = sigterm.recv() => {
                    injector.release_all_keys();
                    transport.disconnect();
                    log::info!("signal received, shutting down");
                    return Ok(());
                }
                _ = sigint.recv() => {
                    injector.release_all_keys();
                    transport.disconnect();
                    log::info!("signal received, shutting down");
                    return Ok(());
                }
            };
            let returned = handle_event(&mut injector, &mut transport, &mut last_clip_hash, event).await;
            if returned {
                // Cursor returned to sender. Drain events until we get
                // CaptureEnd to avoid sending duplicate ReturnToSender
                // from buffered mouse motion events.
                loop {
                    match time::timeout(RECV_TIMEOUT, transport.recv()).await {
                        Ok(Ok(Event::CaptureEnd)) => {
                            log::info!("capture end");
                            injector.release_all_keys();
                            break;
                        }
                        Ok(Ok(Event::CaptureBegin { from_bottom, source_height })) => {
                            log::info!("capture begin (from_bottom={from_bottom:.0}, source_height={source_height:.0})");
                            injector.place_cursor_from_bottom(from_bottom);
                            break;
                        }
                        Ok(Ok(_)) => continue, // discard buffered events
                        Ok(Err(_)) | Err(_) => break, // error or timeout
                    }
                }
            }
        }

        injector.release_all_keys();
        transport.disconnect();
    }

    injector.release_all_keys();
    transport.disconnect();
    log::info!("shutdown complete");
    Ok(())
}

/// Returns true if ReturnToSender was sent (caller should drain until CaptureEnd).
async fn handle_event(
    injector: &mut Injector,
    transport: &mut ReceiverTransport,
    last_clip_hash: &mut u64,
    event: Event,
) -> bool {
    match event {
        Event::MouseMotion { dx, dy } => {
            let hit_edge = injector.inject_mouse_motion(dx, dy);
            if hit_edge {
                let (from_bottom, source_height) = injector.cursor_from_bottom();
                log::info!("cursor hit return edge (from_bottom={from_bottom:.0}, height={source_height:.0})");
                injector.release_all_keys();
                // Clipboard stays in sync via the proactive_clipboard_poll
                // task, so no read happens here. This avoids the
                // pasteboard-not-yet-settled race when the user hits
                // Cmd+C and immediately crosses back.
                let _ = transport.send(&Event::ReturnToSender { from_bottom, source_height }).await;
                return true;
            }
        }
        Event::MouseButton { button, state } => {
            injector.inject_mouse_button(button, state);
        }
        Event::MouseScroll { axis, value } => {
            injector.inject_scroll(axis, value);
        }
        Event::KeyPress { code } => {
            injector.inject_key(code, true);
        }
        Event::KeyRelease { code } => {
            injector.inject_key(code, false);
        }
        Event::CaptureBegin { from_bottom, source_height } => {
            log::info!("capture begin (from_bottom={from_bottom:.0}, source_height={source_height:.0})");
            injector.place_cursor_from_bottom(from_bottom);
        }
        Event::CaptureEnd => {
            log::info!("capture end");
            injector.release_all_keys();
        }
        Event::Heartbeat => {
            let _ = transport.send(&Event::HeartbeatAck).await;
        }
        Event::ClipboardData { text } => {
            log::debug!("received clipboard text from sender ({} bytes)", text.len());
            *last_clip_hash = clipboard::hash_text(&text);
            clipboard::write_clipboard(&text).await;
        }
        Event::ClipboardImage { format, data } => {
            log::info!(
                "received clipboard image from sender ({}, {} bytes)",
                format,
                data.len(),
            );
            *last_clip_hash = clipboard::hash_image(&format, &data);
            let fmt = format.clone();
            let _ = tokio::task::spawn_blocking(move || {
                clipboard_image::write_clipboard_image(&fmt, &data);
            })
            .await;
        }
        Event::ReturnToSender { .. } | Event::HeartbeatAck => {}
    }
    false
}

/// Polls `NSPasteboard.changeCount` at 10 Hz. On every bump, reads the
/// clipboard (PNG image first, text via pbpaste as fallback) and
/// forwards an `Event` on the channel so the main loop can dedup and
/// transmit. Runs for the lifetime of the process; survives sender
/// disconnects because the channel is bounded capacity 1 with
/// try_send, so stale pending events are replaced by the latest.
///
/// Starts by reading the current changeCount and treating it as the
/// baseline so existing pasteboard contents at startup are NOT
/// re-synced; that avoids spamming the sender on every reconnect.
async fn proactive_clipboard_poll(tx: tokio::sync::mpsc::Sender<Event>) {
    let mut last_change: isize = tokio::task::spawn_blocking(clipboard_image::pasteboard_change_count)
        .await
        .unwrap_or(0);
    let mut ticker = time::interval(Duration::from_millis(100));
    ticker.set_missed_tick_behavior(time::MissedTickBehavior::Delay);
    // Consume the immediate first tick -- interval() fires immediately by default.
    ticker.tick().await;

    loop {
        ticker.tick().await;

        let current = match tokio::task::spawn_blocking(clipboard_image::pasteboard_change_count).await {
            Ok(c) => c,
            Err(e) => {
                log::warn!("pasteboard_change_count join error: {e}");
                continue;
            }
        };
        if current == last_change {
            continue;
        }
        last_change = current;

        // Prefer image (same order as the previous edge-cross read).
        let image = tokio::task::spawn_blocking(clipboard_image::read_clipboard_image)
            .await
            .ok()
            .flatten();
        let event = if let Some((format, data)) = image {
            Event::ClipboardImage { format, data }
        } else if let Some(text) = clipboard::read_clipboard().await {
            Event::ClipboardData { text }
        } else {
            continue;
        };

        // try_send: if the main loop has not drained the previous event
        // yet, drop this tick -- a later tick will catch up with
        // whatever is on the pasteboard then.
        if let Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) = tx.try_send(event) {
            // Main loop ended; nothing to do.
            return;
        }
    }
}
