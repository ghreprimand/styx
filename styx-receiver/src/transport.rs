use std::io;
use std::net::SocketAddr;

use tokio::net::{TcpListener, TcpStream};

use styx_proto::{self, Event, read_event, write_event};

pub struct ReceiverTransport {
    listener: TcpListener,
    stream: Option<TcpStream>,
}

impl ReceiverTransport {
    pub async fn bind(addr: SocketAddr) -> io::Result<Self> {
        let listener = TcpListener::bind(addr).await?;
        log::info!("listening on {addr}");
        Ok(ReceiverTransport {
            listener,
            stream: None,
        })
    }

    pub async fn accept(&mut self) -> io::Result<()> {
        let (stream, peer) = self.listener.accept().await?;
        stream.set_nodelay(true)?;
        log::info!("accepted connection from {peer}");
        self.stream = Some(stream);
        Ok(())
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

    pub fn disconnect(&mut self) {
        if self.stream.take().is_some() {
            log::info!("client disconnected");
        }
    }
}
