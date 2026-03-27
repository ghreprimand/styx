use std::io;
use std::time::Duration;

use tokio::net::TcpStream;

use styx_proto::{self, Event, read_event, write_event};

pub struct ReceiverTransport {
    stream: Option<TcpStream>,
}

impl ReceiverTransport {
    pub fn new() -> Self {
        ReceiverTransport { stream: None }
    }

    pub fn is_connected(&self) -> bool {
        self.stream.is_some()
    }

    pub async fn recv(&mut self) -> Result<Event, styx_proto::DecodeError> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(styx_proto::DecodeError::ConnectionClosed);
        };
        read_event(stream).await
    }

    pub async fn send(&mut self, event: &Event) -> io::Result<()> {
        let Some(stream) = self.stream.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "not connected"));
        };
        write_event(stream, event).await
    }

    pub fn set_stream(&mut self, stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        // Enable TCP keepalive so the OS detects dead connections.
        let sock = socket2::SockRef::from(&stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(5))
            .with_interval(Duration::from_secs(5));
        let _ = sock.set_tcp_keepalive(&keepalive);
        self.stream = Some(stream);
    }

    pub fn disconnect(&mut self) {
        if self.stream.take().is_some() {
            log::info!("client disconnected");
        }
    }
}
