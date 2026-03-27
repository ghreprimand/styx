use std::io;

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
        self.stream = Some(stream);
    }

    pub fn disconnect(&mut self) {
        if self.stream.take().is_some() {
            log::info!("client disconnected");
        }
    }
}
