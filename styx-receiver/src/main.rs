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
#[command(name = "styx-receiver", about = "Styx software KVM receiver")]
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
    let mut injector = Injector::new(return_edge)?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    log::info!("styx-receiver running (return_edge={:?})", return_edge);

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
        injector.reset_cursor_to_entry(0.5);

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
                r = listener.accept() => match r {
                    Ok((stream, peer)) => {
                        log::info!("new connection from {peer}, replacing existing");
                        injector.release_all_keys();
                        transport.disconnect();
                        transport.set_stream(stream);
                        injector.reset_cursor_to_entry(0.5);
                        continue;
                    }
                    Err(e) => {
                        log::error!("accept error: {e}");
                        continue;
                    }
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
            handle_event(&mut injector, &mut transport, event).await;
        }

        injector.release_all_keys();
        transport.disconnect();
    }

    injector.release_all_keys();
    transport.disconnect();
    log::info!("shutdown complete");
    Ok(())
}

async fn handle_event(
    injector: &mut Injector,
    transport: &mut ReceiverTransport,
    event: Event,
) {
    match event {
        Event::MouseMotion { dx, dy } => {
            let hit_edge = injector.inject_mouse_motion(dx, dy);
            if hit_edge {
                let fraction = injector.edge_fraction();
                log::info!("cursor hit return edge (fraction={fraction:.2})");
                injector.release_all_keys();
                let _ = transport.send(&Event::ReturnToSender { edge_fraction: fraction }).await;
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
        Event::CaptureBegin { edge_fraction } => {
            log::info!("capture begin (fraction={edge_fraction:.2})");
            injector.reset_cursor_to_entry(edge_fraction);
        }
        Event::CaptureEnd => {
            log::info!("capture end");
            injector.release_all_keys();
        }
        Event::Heartbeat => {
            let _ = transport.send(&Event::HeartbeatAck).await;
        }
        Event::ReturnToSender { .. } | Event::HeartbeatAck => {}
    }
}
