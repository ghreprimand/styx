use std::io;
use std::time::Duration;

use tokio::net::TcpStream;

use styx_proto::{self, Event, FrameReader, write_event};

pub struct ReceiverTransport {
    reader: Option<FrameReader<TcpStream>>,
}

impl ReceiverTransport {
    pub fn new() -> Self {
        ReceiverTransport { reader: None }
    }


    pub async fn recv(&mut self) -> Result<Event, styx_proto::DecodeError> {
        let Some(reader) = self.reader.as_mut() else {
            return Err(styx_proto::DecodeError::ConnectionClosed);
        };
        reader.read_event().await
    }

    pub async fn send(&mut self, event: &Event) -> io::Result<()> {
        let Some(reader) = self.reader.as_mut() else {
            return Err(io::Error::new(io::ErrorKind::NotConnected, "not connected"));
        };
        write_event(reader.get_mut(), event).await
    }

    pub fn set_stream(&mut self, stream: TcpStream) {
        let _ = stream.set_nodelay(true);
        // Enable TCP keepalive so the OS detects dead connections.
        let sock = socket2::SockRef::from(&stream);
        let keepalive = socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(5))
            .with_interval(Duration::from_secs(5));
        let _ = sock.set_tcp_keepalive(&keepalive);
        self.reader = Some(FrameReader::new(stream));
    }

    pub fn disconnect(&mut self) {
        if self.reader.take().is_some() {
            log::info!("client disconnected");
        }
    }
}
