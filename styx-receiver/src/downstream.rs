//! Outbound link to the next node in a relay chain.
//!
//! A receiver configured with a downstream peer holds two endpoints: the
//! inbound listener it already had (facing the upstream sender) and this
//! outbound link (facing the next machine). Together they let the node route
//! an event stream onward without capturing anything locally.
//!
//! The link is owned by a background task so the main loop never blocks on
//! connect, reconnect, or a slow peer. The task also **terminates heartbeats
//! per hop**: it answers inbound heartbeats itself and runs its own outbound
//! heartbeat. Forwarding them end to end would make the upstream sender's
//! dead-peer detection measure the whole chain, tearing down a healthy
//! upstream link because the far end went to sleep.
//!
//! `is_healthy()` is the single source of truth for whether the forward edge
//! is armed. When it is false the relay is inert and the node behaves exactly
//! as a plain receiver.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time;

use styx_proto::{Event, FrameReader, write_event};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const BACKOFF_START: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(2);
const HEARTBEAT_MISS_LIMIT: u32 = 3;

/// Outbound queue depth. Deep enough to absorb a burst of motion events
/// during a momentary stall, shallow enough that a genuinely wedged peer is
/// noticed within a fraction of a second rather than accumulating a backlog
/// of stale input that would replay when it recovers.
const QUEUE_DEPTH: usize = 256;

pub struct DownstreamLink {
    to_peer: mpsc::Sender<Event>,
    from_peer: mpsc::Receiver<Event>,
    healthy: Arc<AtomicBool>,
}

impl DownstreamLink {
    /// Start the background connection task. Returns immediately; the link
    /// reports unhealthy until the first successful connect.
    pub fn spawn(addrs: Vec<SocketAddr>) -> Self {
        let (to_tx, to_rx) = mpsc::channel(QUEUE_DEPTH);
        let (from_tx, from_rx) = mpsc::channel(QUEUE_DEPTH);
        let healthy = Arc::new(AtomicBool::new(false));

        tokio::spawn(run(addrs, to_rx, from_tx, Arc::clone(&healthy)));

        DownstreamLink {
            to_peer: to_tx,
            from_peer: from_rx,
            healthy,
        }
    }

    /// Whether the link is connected and its heartbeat is answering. This is
    /// what arms the forward edge.
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// Queue an event for the downstream peer.
    ///
    /// Returns false when the queue is full, which means the peer has stopped
    /// draining. The caller should treat that as a link failure and recover
    /// the cursor rather than continuing to forward into a stall: dropping
    /// individual input events silently is what produces stuck modifiers.
    pub fn try_send(&self, event: Event) -> bool {
        match self.to_peer.try_send(event) {
            Ok(()) => true,
            Err(mpsc::error::TrySendError::Full(_)) => {
                log::warn!("downstream queue full; peer is not draining");
                self.healthy.store(false, Ordering::Relaxed);
                false
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                self.healthy.store(false, Ordering::Relaxed);
                false
            }
        }
    }

    /// Receive the next event from the downstream peer. Cancel-safe, so it
    /// can be used directly in `tokio::select!`.
    pub async fn recv(&mut self) -> Option<Event> {
        self.from_peer.recv().await
    }
}

async fn connect_any(addrs: &[SocketAddr]) -> Option<TcpStream> {
    for &addr in addrs {
        match time::timeout(CONNECT_TIMEOUT, TcpStream::connect(addr)).await {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                let sock = socket2::SockRef::from(&stream);
                let keepalive = socket2::TcpKeepalive::new()
                    .with_time(Duration::from_secs(5))
                    .with_interval(Duration::from_secs(5));
                let _ = sock.set_tcp_keepalive(&keepalive);
                log::info!("downstream connected to {addr}");
                return Some(stream);
            }
            Ok(Err(e)) => log::debug!("downstream connect to {addr} failed: {e}"),
            Err(_) => log::debug!("downstream connect to {addr} timed out"),
        }
    }
    None
}

async fn run(
    addrs: Vec<SocketAddr>,
    mut outbound: mpsc::Receiver<Event>,
    inbound: mpsc::Sender<Event>,
    healthy: Arc<AtomicBool>,
) {
    let mut backoff = BACKOFF_START;

    loop {
        let Some(stream) = connect_any(&addrs).await else {
            // Quiet by design. A downstream peer that is switched off is the
            // normal case, not an error, and this loop runs for as long as the
            // process does -- logging every attempt at info would fill the
            // journal overnight.
            log::debug!("downstream unreachable; retrying in {backoff:?}");
            time::sleep(backoff).await;
            backoff = (backoff * 2).min(BACKOFF_MAX);
            continue;
        };
        backoff = BACKOFF_START;

        // Split so reads and writes can be driven concurrently in the select
        // below; a single handle cannot be borrowed by two branches at once.
        let (rd, mut wr) = stream.into_split();
        let mut reader = FrameReader::new(rd);

        healthy.store(true, Ordering::Relaxed);
        log::info!("downstream link up; forward edge armed");

        let mut ticker = time::interval(HEARTBEAT_INTERVAL);
        ticker.tick().await; // consume the immediate first tick
        let mut misses: u32 = 0;

        loop {
            tokio::select! {
                maybe = outbound.recv() => {
                    let Some(event) = maybe else {
                        // Main loop dropped the link; shut the task down.
                        healthy.store(false, Ordering::Relaxed);
                        return;
                    };
                    if let Err(e) = write_event(&mut wr, &event).await {
                        log::warn!("downstream write failed: {e}");
                        break;
                    }
                }

                r = reader.read_event() => {
                    match r {
                        // Heartbeats terminate here rather than propagating.
                        Ok(Event::Heartbeat) => {
                            if write_event(&mut wr, &Event::HeartbeatAck).await.is_err() {
                                break;
                            }
                        }
                        Ok(Event::HeartbeatAck) => misses = 0,
                        Ok(event) => {
                            if inbound.send(event).await.is_err() {
                                healthy.store(false, Ordering::Relaxed);
                                return;
                            }
                        }
                        Err(e) => {
                            log::info!("downstream closed: {e}");
                            break;
                        }
                    }
                }

                _ = ticker.tick() => {
                    if misses >= HEARTBEAT_MISS_LIMIT {
                        log::warn!("downstream heartbeat lost after {misses} misses");
                        break;
                    }
                    misses += 1;
                    if write_event(&mut wr, &Event::Heartbeat).await.is_err() {
                        break;
                    }
                }
            }
        }

        healthy.store(false, Ordering::Relaxed);
        log::warn!("downstream link down; forward edge disarmed");

        // Drop anything queued for a peer that is no longer there. Replaying
        // stale input on reconnect would be worse than losing it.
        while outbound.try_recv().is_ok() {}
    }
}
