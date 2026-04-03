use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::net::TcpStream;
use tokio::time;

use styx_proto::{self, Event, read_event, write_event};

pub struct SenderTransport {
    stream: Option<TcpStream>,
    addrs: Vec<SocketAddr>,
}

impl SenderTransport {
    pub fn new(addrs: Vec<SocketAddr>) -> Self {
        SenderTransport {
            stream: None,
            addrs,
        }
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    pub async fn connect(&mut self) -> io::Result<()> {
        let mut backoff = Duration::from_secs(1);
        loop {
            for &addr in &self.addrs {
                match time::timeout(Duration::from_secs(5), TcpStream::connect(addr)).await {
                    Ok(Ok(stream)) => {
                        stream.set_nodelay(true)?;
                        let sock = socket2::SockRef::from(&stream);
                        let keepalive = socket2::TcpKeepalive::new()
                            .with_time(Duration::from_secs(5))
                            .with_interval(Duration::from_secs(5));
                        let _ = sock.set_tcp_keepalive(&keepalive);
                        log::info!("connected to {addr}");
                        self.stream = Some(stream);
                        return Ok(());
                    }
                    Ok(Err(e)) => {
                        log::warn!("connection to {addr} failed: {e}");
                    }
                    Err(_) => {
                        log::warn!("connection to {addr} timed out");
                    }
                }
            }
            log::info!("retrying in {backoff:?}");
            time::sleep(backoff).await;
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    pub async fn send(&mut self, event: &Event) -> io::Result<()> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "not connected"));
        };
        write_event(stream, event).await
    }

    pub async fn recv(&mut self) -> Result<Event, styx_proto::DecodeError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(styx_proto::DecodeError::ConnectionClosed);
        };
        read_event(stream).await
    }

    pub fn disconnect(&mut self) {
        if self.stream.take().is_some() {
            log::info!("disconnected");
        }
    }
}
