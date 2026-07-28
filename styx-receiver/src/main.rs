mod downstream;
mod edge;
mod geometry;
mod transport;

// Injection backend. Both modules expose the same `Injector` surface, so
// every call site below is platform-agnostic; only construction differs.
#[cfg(target_os = "macos")]
mod inject;
#[cfg(target_os = "linux")]
#[path = "inject_linux.rs"]
mod inject;

// Clipboard backend. macOS splits text (`clipboard`) from the AppKit-backed
// image/HTML path (`clipboard_image`); the Linux module implements the union
// of both surfaces, so it is bound under both names and the call sites stay
// identical across platforms.
#[cfg(target_os = "macos")]
mod clipboard;
#[cfg(target_os = "macos")]
mod clipboard_image;
#[cfg(target_os = "linux")]
#[path = "clipboard_linux.rs"]
mod clipboard;
#[cfg(target_os = "linux")]
use clipboard as clipboard_image;

use std::net::SocketAddr;
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;
use tokio::net::TcpListener;
use tokio::signal::unix::{SignalKind, signal};
use tokio::time;

use styx_proto::Event;

use downstream::DownstreamLink;
use geometry::{Edge, EdgeHit};
use inject::Injector;
use transport::ReceiverTransport;

/// What this node is currently doing. Derived entirely from link state and
/// cursor position; never configured directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Injecting upstream events locally. The behaviour that predates the
    /// relay, and the only mode a node without `[receiver.relay]` ever has.
    Receiving,
    /// Routing upstream events to the downstream peer, injecting nothing
    /// locally.
    Forwarding,
}

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
    /// Single host to bind to (backward-compatible). If set, merged
    /// with `listen_hosts` when building the final bind list. The
    /// receiver refuses to start if neither `listen_host` nor
    /// `listen_hosts` is configured.
    #[serde(default)]
    listen_host: Option<String>,
    /// List of hosts to bind to. Each is tried independently; the
    /// receiver binds to every IP that resolves to a live local
    /// interface and ignores the rest with a warning. Use this to
    /// reserve multiple home-network IPs (ethernet + wifi) while
    /// refusing to start on public networks where those IPs do not
    /// exist.
    #[serde(default)]
    listen_hosts: Vec<String>,
    /// List of peer IP addresses permitted to connect. An empty list
    /// disables the allowlist and every TCP connection that reaches a
    /// bound port is accepted (legacy/default behaviour). When the
    /// list is non-empty, connections from any peer not in the list
    /// are closed immediately with a log line, before any bytes are
    /// read. Use this alongside `listen_hosts` to defend against both
    /// network-level exposure (listen_hosts) and LAN-local hostile
    /// peers on networks where the receiver does bind (allowed_senders).
    #[serde(default)]
    allowed_senders: Vec<String>,
    listen_port: u16,
    #[serde(default = "default_return_edge")]
    return_edge: String,
    #[serde(default)]
    swap_alt_cmd: bool,
    /// Display layout, used only by the Linux backend.
    ///
    /// macOS enumerates displays through CoreGraphics, but the Linux
    /// receiver holds no display-server connection, so the layout has to be
    /// declared. The virtual pointer's absolute axes are mapped onto the
    /// extent computed from these rectangles; if it disagrees with the
    /// compositor's real layout the cursor lands in the wrong place.
    ///
    /// Parsed on both platforms so a config file can be shared between them
    /// without one side rejecting the other's keys.
    #[serde(default)]
    display: Vec<DisplayConfig>,
    /// Downstream peer, when this node relays onward. Absent on a terminal
    /// receiver, in which case no outbound link is constructed at all and the
    /// binary behaves exactly as it did before the relay existed.
    #[serde(default)]
    relay: Option<RelayConfig>,
}

/// Downstream half of a relay node.
#[derive(Deserialize, Debug)]
struct RelayConfig {
    /// Single downstream address.
    #[serde(default)]
    host: Option<String>,
    /// Several downstream addresses, tried in order. Use when the peer has
    /// more than one interface.
    #[serde(default)]
    hosts: Vec<String>,
    port: u16,
    /// Side of this machine's display that faces the downstream peer. Must
    /// differ from `return_edge`.
    forward_edge: String,
}

/// One display rectangle, in the compositor's logical layout coordinates
/// (the same space `hyprctl monitors` reports as `x`, `y`, and the
/// scale-adjusted `width`/`height`).
#[derive(Deserialize, Clone, Copy, Debug)]
#[cfg_attr(target_os = "macos", allow(dead_code))]
struct DisplayConfig {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

impl DisplayConfig {
    #[cfg_attr(target_os = "macos", allow(dead_code))]
    fn to_bounds(self) -> geometry::DisplayBounds {
        geometry::DisplayBounds {
            min_x: self.x,
            min_y: self.y,
            max_x: self.x + self.width,
            max_y: self.y + self.height,
        }
    }
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

/// Hard cap on the launchd-managed stderr log. If the receiver restarts
/// while the file is larger than this, it gets truncated before the new
/// process writes anything. Prevents runaway growth when the receiver
/// is stuck in a respawn loop on networks where none of `listen_hosts`
/// bind. 10 MiB is plenty of recent history for troubleshooting without
/// accumulating hundreds of MiB across weeks of travel.
#[cfg(target_os = "macos")]
const MAX_LOG_SIZE: u64 = 10 * 1024 * 1024;

/// Matches `StandardErrorPath` in `dist/macos/styx-receiver.plist`.
/// Hard-coded because launchd does not expose this to the child process.
#[cfg(target_os = "macos")]
const STDERR_LOG_PATH: &str = "/tmp/styx-receiver.stderr.log";

/// Check the launchd-managed stderr log and, if it is larger than
/// `MAX_LOG_SIZE`, truncate it and rebind stderr (fd 2) to a fresh
/// write handle. The rebind matters: launchd has already dup'd its
/// own open file descriptor onto our stderr, and that descriptor
/// carries an offset that would cause subsequent writes to land
/// past the truncation point, leaving a sparse gap. Opening the
/// file ourselves and `dup2`-ing over fd 2 gives us a fresh offset
/// so the new log is written contiguously from byte zero.
///
/// Best-effort: any failure (missing file, permission error) leaves
/// launchd's original redirect in place and logging continues to
/// work normally.
///
/// macOS only. On Linux the receiver runs under systemd, which captures
/// stderr into the journal and rotates it itself -- hijacking fd 2 to a
/// fixed path there would keep output out of `journalctl`, silence
/// foreground runs, and (since `/tmp` is world-writable) append into a
/// file another user could have created first.
#[cfg(target_os = "macos")]
fn cap_stderr_log() {
    let truncate = match std::fs::metadata(STDERR_LOG_PATH) {
        Ok(meta) => meta.len() > MAX_LOG_SIZE,
        Err(_) => false,
    };

    let file = if truncate {
        std::fs::File::create(STDERR_LOG_PATH).ok()
    } else {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(STDERR_LOG_PATH)
            .ok()
    };

    if let Some(f) = file {
        use std::os::unix::io::AsRawFd;
        // SAFETY: dup2 operates on two valid file descriptors; fd 2
        // (stderr) is always valid in a process, and f.as_raw_fd()
        // is valid for the lifetime of `f`.
        unsafe {
            libc::dup2(f.as_raw_fd(), 2);
        }
        // Leak `f` so Rust does not close the fd we just installed
        // as stderr.
        std::mem::forget(f);
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    #[cfg(target_os = "macos")]
    cap_stderr_log();
    env_logger::init();

    // Check Accessibility permission early so it's obvious in the logs.
    // macOS-only: the Linux backend's equivalent failure is `/dev/uinput`
    // being unopenable, which surfaces as a hard error at Injector::new.
    #[cfg(target_os = "macos")]
    {
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
    }

    let cli = Cli::parse();
    let config = load_config(&cli.config)?;
    let return_edge = parse_edge(&config.receiver.return_edge)?;

    // Assemble the final bind list: listen_hosts takes precedence but
    // listen_host is kept for backward compat with 0.4.x configs. At
    // least one must be set.
    let mut bind_hosts: Vec<String> = config.receiver.listen_hosts.clone();
    if let Some(h) = &config.receiver.listen_host {
        if !bind_hosts.iter().any(|x| x == h) {
            bind_hosts.push(h.clone());
        }
    }
    if bind_hosts.is_empty() {
        return Err("receiver config: at least one of listen_host or listen_hosts must be set".into());
    }

    // Bind every host we can; warn on the rest. If nothing binds we
    // exit cleanly -- that is the intended behaviour on networks
    // without any of the configured home IPs (e.g. public wifi), so
    // the receiver is not reachable when it should not be.
    let port = config.receiver.listen_port;
    let mut listeners: Vec<TcpListener> = Vec::new();
    for host in &bind_hosts {
        let addr: SocketAddr = match format!("{host}:{port}").parse() {
            Ok(a) => a,
            Err(e) => {
                log::warn!("invalid listen host '{host}:{port}': {e}");
                continue;
            }
        };
        match TcpListener::bind(addr).await {
            Ok(l) => {
                log::info!("listening on {addr}");
                listeners.push(l);
            }
            Err(e) => log::warn!("failed to bind {addr}: {e}"),
        }
    }
    if listeners.is_empty() {
        return Err(format!(
            "no configured listen hosts bound successfully ({} attempted); receiver not reachable on this network",
            bind_hosts.len(),
        )
        .into());
    }

    // Pre-parse allowed_senders into IpAddr values once at startup so
    // the accept tasks do string parsing zero times per connection.
    // Invalid entries produce a startup warning and are dropped.
    let allowed_senders: Vec<std::net::IpAddr> = config
        .receiver
        .allowed_senders
        .iter()
        .filter_map(|s| match s.parse::<std::net::IpAddr>() {
            Ok(ip) => Some(ip),
            Err(e) => {
                log::warn!("invalid allowed_senders entry '{s}': {e}; ignoring");
                None
            }
        })
        .collect();
    if allowed_senders.is_empty() && !config.receiver.allowed_senders.is_empty() {
        return Err(
            "allowed_senders is set but no entry parsed as a valid IP address; \
             refusing to start rather than accepting every peer"
                .into(),
        );
    }
    if !allowed_senders.is_empty() {
        log::info!(
            "sender allowlist active: {} peer(s) permitted",
            allowed_senders.len(),
        );
    } else {
        log::info!("sender allowlist not configured; any peer that reaches a bound port will be accepted");
    }

    // Fan all per-listener accept() loops into a single mpsc channel
    // so the main loop can wait on connections from any of them
    // uniformly. Capacity 4 is a small cushion for multiple listeners
    // producing connections simultaneously; main drains quickly so
    // this rarely fills.
    let (accept_tx, mut accept_rx) =
        tokio::sync::mpsc::channel::<(tokio::net::TcpStream, SocketAddr)>(4);
    for listener in listeners {
        let tx = accept_tx.clone();
        let allowlist = allowed_senders.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, peer)) => {
                        if !allowlist.is_empty() && !allowlist.contains(&peer.ip()) {
                            log::warn!(
                                "rejected connection from {peer}: not in allowed_senders"
                            );
                            drop(stream);
                            continue;
                        }
                        if tx.send((stream, peer)).await.is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        log::error!("accept error: {e}");
                    }
                }
            }
        });
    }
    drop(accept_tx);

    let mut transport = ReceiverTransport::new();

    // Downstream relay link, if configured. Constructed before the injector
    // because the injector needs to know which edge faces the peer.
    let (mut downstream, forward_edge) = match &config.receiver.relay {
        Some(relay) => {
            let edge = parse_edge(&relay.forward_edge)?;
            if edge == return_edge {
                return Err(format!(
                    "receiver config: forward_edge and return_edge are both '{}'; \
                     they must be different sides",
                    relay.forward_edge,
                )
                .into());
            }

            let mut hosts: Vec<String> = Vec::new();
            if let Some(h) = &relay.host {
                hosts.push(h.clone());
            }
            for h in &relay.hosts {
                if !hosts.contains(h) {
                    hosts.push(h.clone());
                }
            }
            if hosts.is_empty() {
                return Err(
                    "receiver config: [receiver.relay] must set host or hosts".into(),
                );
            }

            let addrs: Vec<SocketAddr> = hosts
                .iter()
                .map(|h| format!("{h}:{}", relay.port).parse())
                .collect::<Result<_, _>>()
                .map_err(|e| format!("invalid downstream address: {e}"))?;

            log::info!(
                "relay configured: forward_edge={:?}, downstream={:?}",
                edge, addrs,
            );
            log::info!(
                "forward edge stays disarmed until the downstream link is up; \
                 until then this node behaves as a plain receiver",
            );
            (Some(DownstreamLink::spawn(addrs)), Some(edge))
        }
        None => (None, None),
    };

    #[cfg(target_os = "macos")]
    let mut injector = {
        let injector = Injector::new(return_edge, config.receiver.swap_alt_cmd, forward_edge)?;
        if config.receiver.swap_alt_cmd {
            log::info!("modifier remap: Alt->Cmd, Super->Option (swap_alt_cmd=true)");
        }
        injector
    };

    // The Linux backend needs the display layout up front and has no way to
    // discover it, so an empty list is a hard configuration error rather than
    // something to guess around.
    #[cfg(target_os = "linux")]
    let mut injector = {
        clipboard::check_tools();
        let displays: Vec<geometry::DisplayBounds> = config
            .receiver
            .display
            .iter()
            .map(|d| d.to_bounds())
            .collect();
        if displays.is_empty() {
            return Err(
                "receiver config: the Linux backend needs at least one [[receiver.display]] \
                 entry describing the compositor's layout. See docs/linux-receiver.md."
                    .into(),
            );
        }
        if config.receiver.swap_alt_cmd {
            log::warn!("swap_alt_cmd is ignored by the Linux backend; both ends use evdev codes");
        }
        Injector::new(return_edge, displays, forward_edge)?
    };

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
        // Wait for a connection on any of the bound listeners.
        log::info!("waiting for connection...");
        let (stream, peer) = tokio::select! {
            maybe = accept_rx.recv() => match maybe {
                Some(pair) => pair,
                None => {
                    log::error!("all accept tasks exited; no listeners remain");
                    break;
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
        //
        // A fresh upstream connection always starts in Receiving: the cursor
        // arrives on this machine, not beyond it.
        let mut mode = Mode::Receiving;

        loop {
            // Arm the forward edge from downstream health on every iteration.
            // Cheap (one relaxed load and a bool store) and keeps arming
            // reactive to link state without a separate timer.
            let downstream_up = downstream.as_ref().is_some_and(|d| d.is_healthy());
            injector.set_forward_armed(downstream_up);

            // Downstream died while the cursor was over there. The upstream
            // sender still holds its evdev grab and is streaming into a route
            // that no longer exists, so recover the cursor onto this machine
            // rather than swallowing input. The user sees a jump, which is the
            // correct outcome.
            if mode == Mode::Forwarding && !downstream_up {
                log::warn!("downstream lost while forwarding; recovering cursor locally");
                injector.place_cursor_at_forward_edge(0.0);
                injector.release_all_keys();
                mode = Mode::Receiving;
            }

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
                        Event::ClipboardHtml { html, plain } => Some(clipboard::hash_html(html, plain)),
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
                                Event::ClipboardHtml { html, plain } => log::info!(
                                    "proactive clipboard html to sender ({} html bytes, {} plain bytes)",
                                    html.len(), plain.len(),
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
                Some(event) = async {
                    match downstream.as_mut() {
                        Some(d) => d.recv().await,
                        // No relay configured: never resolves, so this branch
                        // is inert rather than spinning.
                        None => std::future::pending().await,
                    }
                } => {
                    if let Event::ReturnToSender { from_bottom, source_height } = event {
                        log::info!(
                            "downstream returned cursor (from_bottom={from_bottom:.0}, height={source_height:.0})"
                        );
                        // Tell the peer to drop any held keys, then take the
                        // cursor back at the forward edge.
                        if let Some(d) = downstream.as_ref() {
                            d.try_send(Event::CaptureEnd);
                        }
                        let scaled = if source_height > 0.0 {
                            let (_, own_height) = injector.cursor_from_forward_edge();
                            from_bottom * (own_height / source_height)
                        } else {
                            from_bottom
                        };
                        injector.place_cursor_at_forward_edge(scaled);
                        mode = Mode::Receiving;
                    }
                    continue;
                },

                maybe = accept_rx.recv() => match maybe {
                    Some((stream, peer)) => {
                        log::info!("new connection from {peer}, replacing existing");
                        injector.release_all_keys();
                        transport.disconnect();
                        transport.set_stream(stream);
                        continue;
                    }
                    None => {
                        log::error!("all accept tasks exited; no listeners remain");
                        break;
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
            // While forwarding, this node is a pure router: input events go
            // straight out the downstream link and nothing is injected here.
            // Clipboard and capture bookkeeping are still handled locally.
            if mode == Mode::Forwarding {
                match event {
                    Event::MouseMotion { .. }
                    | Event::MouseButton { .. }
                    | Event::MouseScroll { .. }
                    | Event::KeyPress { .. }
                    | Event::KeyRelease { .. } => {
                        let sent = downstream
                            .as_ref()
                            .map(|d| d.try_send(event))
                            .unwrap_or(false);
                        if !sent {
                            // The next loop iteration sees the link unhealthy
                            // and runs the cursor recovery above.
                            log::warn!("forward failed; downstream link is wedged");
                        }
                        continue;
                    }
                    // Answer upstream heartbeats locally. Forwarding them
                    // would make the sender's dead-peer detection measure the
                    // whole chain.
                    Event::Heartbeat => {
                        let _ = transport.send(&Event::HeartbeatAck).await;
                        continue;
                    }
                    // Upstream ended capture (cursor went back to the
                    // workstation, or the sender is shutting down). Tear the
                    // downstream capture down with it so no key is left held.
                    Event::CaptureEnd => {
                        if let Some(d) = downstream.as_ref() {
                            d.try_send(Event::CaptureEnd);
                        }
                        log::info!("upstream ended capture while forwarding");
                        mode = Mode::Receiving;
                        continue;
                    }
                    // Clipboard and anything else falls through to the normal
                    // local handling below.
                    _ => {}
                }
            }

            let outcome = handle_event(&mut injector, &mut transport, &mut last_clip_hash, event).await;

            if outcome == EdgeHit::Forward {
                // Cursor reached the edge facing the downstream peer, and the
                // edge only fires when the link is healthy. Hand over: seed
                // the peer with any keys held across the crossover, then send
                // CaptureBegin with the cursor height.
                let (from_bottom, height) = injector.cursor_from_forward_edge();
                log::info!(
                    "cursor crossed forward (from_bottom={from_bottom:.0}, height={height:.0})"
                );
                let held = injector.held_keys();
                injector.release_all_keys();
                if let Some(d) = downstream.as_ref() {
                    for code in held {
                        d.try_send(Event::KeyPress { code });
                    }
                    d.try_send(Event::CaptureBegin {
                        from_bottom,
                        source_height: height,
                    });
                }
                mode = Mode::Forwarding;
                continue;
            }

            if outcome == EdgeHit::Return {
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

/// Inject one event locally. Returns which crossover edge the cursor reached,
/// if any. `EdgeHit::Return` means `ReturnToSender` has already been sent and
/// the caller should drain until `CaptureEnd`; `EdgeHit::Forward` means the
/// caller should hand the cursor to the downstream peer.
async fn handle_event(
    injector: &mut Injector,
    transport: &mut ReceiverTransport,
    last_clip_hash: &mut u64,
    event: Event,
) -> EdgeHit {
    match event {
        Event::MouseMotion { dx, dy } => {
            match injector.inject_mouse_motion(dx, dy) {
                EdgeHit::Return => {
                    let (from_bottom, source_height) = injector.cursor_from_bottom();
                    log::info!("cursor hit return edge (from_bottom={from_bottom:.0}, height={source_height:.0})");
                    injector.release_all_keys();
                    // Clipboard stays in sync via the proactive_clipboard_poll
                    // task, so no read happens here. This avoids the
                    // pasteboard-not-yet-settled race when the user hits
                    // Cmd+C and immediately crosses back.
                    let _ = transport.send(&Event::ReturnToSender { from_bottom, source_height }).await;
                    return EdgeHit::Return;
                }
                EdgeHit::Forward => return EdgeHit::Forward,
                EdgeHit::None => {}
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
        Event::ClipboardHtml { html, plain } => {
            log::info!(
                "received clipboard html from sender ({} html bytes, {} plain bytes)",
                html.len(),
                plain.len(),
            );
            *last_clip_hash = clipboard::hash_html(&html, &plain);
            let h = html.clone();
            let p = plain.clone();
            let _ = tokio::task::spawn_blocking(move || {
                clipboard_image::write_clipboard_html(&h, &p);
            })
            .await;
        }
        Event::ReturnToSender { .. } | Event::HeartbeatAck => {}
    }
    EdgeHit::None
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

        // Preference order: image > html > plain text. Image is the
        // heaviest-content type, html carries more context than plain,
        // plain is the lowest-common-denominator fallback. Both image
        // and html reads dedupe internally against their own hashes so
        // we do not re-send when the pasteboard has the same content
        // as a previous sync.
        let image = tokio::task::spawn_blocking(clipboard_image::read_clipboard_image)
            .await
            .ok()
            .flatten();
        let event = if let Some((format, data)) = image {
            Event::ClipboardImage { format, data }
        } else {
            let html = tokio::task::spawn_blocking(clipboard_image::read_clipboard_html)
                .await
                .ok()
                .flatten();
            if let Some((html, plain)) = html {
                Event::ClipboardHtml { html, plain }
            } else if let Some(text) = clipboard::read_clipboard().await {
                Event::ClipboardData { text }
            } else {
                continue;
            }
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
