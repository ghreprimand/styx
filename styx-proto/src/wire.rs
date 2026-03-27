use std::io;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

const EVENT_MOUSE_MOTION: u8 = 0x01;
const EVENT_MOUSE_BUTTON: u8 = 0x02;
const EVENT_MOUSE_SCROLL: u8 = 0x03;
const EVENT_KEY_PRESS: u8 = 0x10;
const EVENT_KEY_RELEASE: u8 = 0x11;
const EVENT_CAPTURE_BEGIN: u8 = 0x20;
const EVENT_CAPTURE_END: u8 = 0x21;
const EVENT_RETURN_TO_SENDER: u8 = 0x22;
const EVENT_HEARTBEAT: u8 = 0x30;
const EVENT_HEARTBEAT_ACK: u8 = 0x31;

#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    MouseMotion { dx: f64, dy: f64 },
    MouseButton { button: u32, state: u8 },
    MouseScroll { axis: u8, value: f64 },
    KeyPress { code: u32 },
    KeyRelease { code: u32 },
    /// Cursor crossed the edge. Carries the pixel distance from the bottom
    /// of the source monitor and the source monitor's total height so the
    /// receiving side can map proportionally.
    CaptureBegin { from_bottom: f64, source_height: f64 },
    CaptureEnd,
    /// Cursor hit the return edge. Same fields as CaptureBegin.
    ReturnToSender { from_bottom: f64, source_height: f64 },
    Heartbeat,
    HeartbeatAck,
}

impl Event {
    fn type_byte(&self) -> u8 {
        match self {
            Event::MouseMotion { .. } => EVENT_MOUSE_MOTION,
            Event::MouseButton { .. } => EVENT_MOUSE_BUTTON,
            Event::MouseScroll { .. } => EVENT_MOUSE_SCROLL,
            Event::KeyPress { .. } => EVENT_KEY_PRESS,
            Event::KeyRelease { .. } => EVENT_KEY_RELEASE,
            Event::CaptureBegin { .. } => EVENT_CAPTURE_BEGIN,
            Event::CaptureEnd => EVENT_CAPTURE_END,
            Event::ReturnToSender { .. } => EVENT_RETURN_TO_SENDER,
            Event::Heartbeat => EVENT_HEARTBEAT,
            Event::HeartbeatAck => EVENT_HEARTBEAT_ACK,
        }
    }

    fn payload_len(&self) -> usize {
        match self {
            Event::MouseMotion { .. } => 16,
            Event::MouseButton { .. } => 5,
            Event::MouseScroll { .. } => 9,
            Event::KeyPress { .. } | Event::KeyRelease { .. } => 4,
            Event::CaptureBegin { .. } | Event::ReturnToSender { .. } => 16,
            _ => 0,
        }
    }

    fn encode_payload(&self, buf: &mut [u8]) {
        match self {
            Event::MouseMotion { dx, dy } => {
                buf[0..8].copy_from_slice(&dx.to_be_bytes());
                buf[8..16].copy_from_slice(&dy.to_be_bytes());
            }
            Event::MouseButton { button, state } => {
                buf[0..4].copy_from_slice(&button.to_be_bytes());
                buf[4] = *state;
            }
            Event::MouseScroll { axis, value } => {
                buf[0] = *axis;
                buf[1..9].copy_from_slice(&value.to_be_bytes());
            }
            Event::KeyPress { code } | Event::KeyRelease { code } => {
                buf[0..4].copy_from_slice(&code.to_be_bytes());
            }
            Event::CaptureBegin { from_bottom, source_height }
            | Event::ReturnToSender { from_bottom, source_height } => {
                buf[0..8].copy_from_slice(&from_bottom.to_be_bytes());
                buf[8..16].copy_from_slice(&source_height.to_be_bytes());
            }
            _ => {}
        }
    }

    fn decode_payload(type_byte: u8, buf: &[u8]) -> Result<Self, DecodeError> {
        match type_byte {
            EVENT_MOUSE_MOTION => {
                if buf.len() < 16 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let dx = f64::from_be_bytes(buf[0..8].try_into().unwrap());
                let dy = f64::from_be_bytes(buf[8..16].try_into().unwrap());
                Ok(Event::MouseMotion { dx, dy })
            }
            EVENT_MOUSE_BUTTON => {
                if buf.len() < 5 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let button = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                let state = buf[4];
                Ok(Event::MouseButton { button, state })
            }
            EVENT_MOUSE_SCROLL => {
                if buf.len() < 9 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let axis = buf[0];
                let value = f64::from_be_bytes(buf[1..9].try_into().unwrap());
                Ok(Event::MouseScroll { axis, value })
            }
            EVENT_KEY_PRESS => {
                if buf.len() < 4 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let code = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                Ok(Event::KeyPress { code })
            }
            EVENT_KEY_RELEASE => {
                if buf.len() < 4 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let code = u32::from_be_bytes(buf[0..4].try_into().unwrap());
                Ok(Event::KeyRelease { code })
            }
            EVENT_CAPTURE_BEGIN => {
                if buf.len() < 16 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let from_bottom = f64::from_be_bytes(buf[0..8].try_into().unwrap());
                let source_height = f64::from_be_bytes(buf[8..16].try_into().unwrap());
                Ok(Event::CaptureBegin { from_bottom, source_height })
            }
            EVENT_CAPTURE_END => Ok(Event::CaptureEnd),
            EVENT_RETURN_TO_SENDER => {
                if buf.len() < 16 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let from_bottom = f64::from_be_bytes(buf[0..8].try_into().unwrap());
                let source_height = f64::from_be_bytes(buf[8..16].try_into().unwrap());
                Ok(Event::ReturnToSender { from_bottom, source_height })
            }
            EVENT_HEARTBEAT => Ok(Event::Heartbeat),
            EVENT_HEARTBEAT_ACK => Ok(Event::HeartbeatAck),
            _ => Err(DecodeError::UnknownEventType(type_byte)),
        }
    }
}

#[derive(Debug)]
pub enum DecodeError {
    Io(io::Error),
    UnknownEventType(u8),
    TruncatedPayload,
    PayloadTooLarge(u16),
    ConnectionClosed,
}

impl From<io::Error> for DecodeError {
    fn from(e: io::Error) -> Self {
        DecodeError::Io(e)
    }
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DecodeError::Io(e) => write!(f, "io error: {e}"),
            DecodeError::UnknownEventType(t) => write!(f, "unknown event type: 0x{t:02x}"),
            DecodeError::TruncatedPayload => write!(f, "truncated payload"),
            DecodeError::PayloadTooLarge(len) => write!(f, "payload too large: {len}"),
            DecodeError::ConnectionClosed => write!(f, "connection closed"),
        }
    }
}

impl std::error::Error for DecodeError {}

/// Maximum payload size: type byte (1) + largest payload (16 for MouseMotion) = 17.
const MAX_FRAME_PAYLOAD: u16 = 17;

/// Write a length-prefixed event to the stream. The frame format is:
/// [u16 BE length][u8 event_type][payload bytes]
/// Length includes the type byte and payload, but not itself.
pub async fn write_event<W: AsyncWrite + Unpin>(w: &mut W, event: &Event) -> io::Result<()> {
    let payload_len = event.payload_len();
    let frame_len = (1 + payload_len) as u16;

    let mut buf = [0u8; 2 + 1 + 16]; // max frame: 2 (len) + 1 (type) + 16 (payload)
    buf[0..2].copy_from_slice(&frame_len.to_be_bytes());
    buf[2] = event.type_byte();
    event.encode_payload(&mut buf[3..3 + payload_len]);

    w.write_all(&buf[..2 + 1 + payload_len]).await
}

/// Read a length-prefixed event from the stream. Returns `DecodeError::ConnectionClosed`
/// on clean EOF.
pub async fn read_event<R: AsyncRead + Unpin>(r: &mut R) -> Result<Event, DecodeError> {
    let mut len_buf = [0u8; 2];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(DecodeError::ConnectionClosed);
        }
        Err(e) => return Err(DecodeError::Io(e)),
    }
    let frame_len = u16::from_be_bytes(len_buf);

    if frame_len == 0 {
        return Err(DecodeError::TruncatedPayload);
    }
    if frame_len > MAX_FRAME_PAYLOAD {
        return Err(DecodeError::PayloadTooLarge(frame_len));
    }

    let mut frame = [0u8; 17];
    r.read_exact(&mut frame[..frame_len as usize]).await?;

    let type_byte = frame[0];
    let payload = &frame[1..frame_len as usize];
    Event::decode_payload(type_byte, payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_events() -> Vec<Event> {
        vec![
            Event::MouseMotion { dx: -1.5, dy: 3.25 },
            Event::MouseButton { button: 0x110, state: 1 },
            Event::MouseButton { button: 0x111, state: 0 },
            Event::MouseScroll { axis: 0, value: -120.0 },
            Event::MouseScroll { axis: 1, value: 15.5 },
            Event::KeyPress { code: 42 },
            Event::KeyRelease { code: 42 },
            Event::CaptureBegin { from_bottom: 200.0, source_height: 1920.0 },
            Event::CaptureEnd,
            Event::ReturnToSender { from_bottom: 400.0, source_height: 956.0 },
            Event::Heartbeat,
            Event::HeartbeatAck,
        ]
    }

    #[tokio::test]
    async fn round_trip() {
        for event in all_events() {
            let mut buf = Vec::new();
            write_event(&mut buf, &event).await.unwrap();
            let decoded = read_event(&mut buf.as_slice()).await.unwrap();
            assert_eq!(event, decoded, "round-trip failed for {event:?}");
        }
    }

    #[tokio::test]
    async fn sequential_stream() {
        let mut buf = Vec::new();
        let events = all_events();
        for event in &events {
            write_event(&mut buf, event).await.unwrap();
        }

        let mut cursor = buf.as_slice();
        for expected in &events {
            let decoded = read_event(&mut cursor).await.unwrap();
            assert_eq!(*expected, decoded);
        }
    }

    #[tokio::test]
    async fn eof_returns_connection_closed() {
        let buf: &[u8] = &[];
        let result = read_event(&mut &*buf).await;
        assert!(matches!(result, Err(DecodeError::ConnectionClosed)));
    }

    #[tokio::test]
    async fn unknown_type_rejected() {
        let frame = [0x00, 0x01, 0xFF]; // length=1, type=0xFF
        let result = read_event(&mut frame.as_slice()).await;
        assert!(matches!(result, Err(DecodeError::UnknownEventType(0xFF))));
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        let frame = [0x00, 0xFF, 0x01]; // length=255, which exceeds MAX_FRAME_PAYLOAD
        let result = read_event(&mut frame.as_slice()).await;
        assert!(matches!(result, Err(DecodeError::PayloadTooLarge(255))));
    }
}
