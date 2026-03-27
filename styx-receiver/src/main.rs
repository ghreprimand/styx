mod edge;
mod inject;
mod transport;

use std::net::SocketAddr;

use clap::Parser;
use serde::Deserialize;
use tokio::signal::unix::{SignalKind, signal};

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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::init();
    let cli = Cli::parse();
    let config = load_config(&cli.config)?;
    let return_edge = parse_edge(&config.receiver.return_edge)?;

    let addr: SocketAddr = format!(
        "{}:{}",
        config.receiver.listen_host, config.receiver.listen_port
    )
    .parse()?;

    let mut transport = ReceiverTransport::bind(addr).await?;
    let mut injector = Injector::new(return_edge)?;

    let mut sigterm = signal(SignalKind::terminate())?;
    let mut sigint = signal(SignalKind::interrupt())?;

    log::info!("styx-receiver running (return_edge={:?})", return_edge);

    // Wait for initial connection.
    log::info!("waiting for connection...");
    transport.accept().await?;
    injector.reset_cursor_to_entry();

    loop {
        tokio::select! {
            // Check for new connections. If a new sender connects, drop the
            // old connection and switch to the new one.
            result = transport.listener().accept() => {
                match result {
                    Ok((stream, peer)) => {
                        let _ = stream.set_nodelay(true);
                        log::info!("new connection from {peer}, replacing previous");
                        injector.release_all_keys();
                        transport.disconnect();
                        transport.replace_stream(stream);
                        injector.reset_cursor_to_entry();
                    }
                    Err(e) => {
                        log::error!("accept error: {e}");
                    }
                }
            }

            result = transport.recv(), if transport.is_connected() => {
                match result {
                    Ok(event) => {
                        handle_event(&mut injector, &mut transport, event).await;
                    }
                    Err(styx_proto::DecodeError::ConnectionClosed) => {
                        log::info!("sender disconnected");
                        injector.release_all_keys();
                        transport.disconnect();
                    }
                    Err(e) => {
                        log::error!("recv error: {e}");
                        injector.release_all_keys();
                        transport.disconnect();
                    }
                }
            }

            _ = sigterm.recv() => {
                log::info!("SIGTERM received, shutting down");
                injector.release_all_keys();
                return Ok(());
            }
            _ = sigint.recv() => {
                log::info!("SIGINT received, shutting down");
                injector.release_all_keys();
                return Ok(());
            }
        }
    }
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
                log::info!("cursor hit return edge");
                injector.release_all_keys();
                let _ = transport.send(&Event::ReturnToSender).await;
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
        Event::CaptureBegin => {
            log::info!("capture begin");
            injector.reset_cursor_to_entry();
        }
        Event::CaptureEnd => {
            log::info!("capture end");
            injector.release_all_keys();
        }
        Event::Heartbeat => {
            let _ = transport.send(&Event::HeartbeatAck).await;
        }
        Event::ReturnToSender | Event::HeartbeatAck => {}
    }
}
