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
        if self.stream.is_some() {
            log::info!("dropping old connection, accepted new from {peer}");
        } else {
            log::info!("accepted connection from {peer}");
        }
        self.stream = Some(stream);
        Ok(())
    }

    /// Accept a new connection if one is pending, without blocking.
    /// Returns true if a new connection replaced the old one.
    pub async fn try_accept(&mut self) -> io::Result<bool> {
        match self.listener.try_accept() {
            Ok((stream, peer)) => {
                stream.set_nodelay(true)?;
                log::info!("new connection from {peer}, replacing old");
                self.stream = Some(stream);
                Ok(true)
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => Ok(false),
            Err(e) => Err(e),
        }
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

    pub fn replace_stream(&mut self, stream: TcpStream) {
        self.stream = Some(stream);
    }

    pub fn listener(&self) -> &TcpListener {
        &self.listener
    }
}
