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
const EVENT_CLIPBOARD_DATA: u8 = 0x40;
const EVENT_CLIPBOARD_IMAGE: u8 = 0x41;

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
    ClipboardData { text: String },
    /// Binary image clipboard payload. `format` is a MIME type string
    /// (e.g. `image/png`). `data` is the encoded image bytes.
    ClipboardImage { format: String, data: Vec<u8> },
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
            Event::ClipboardData { .. } => EVENT_CLIPBOARD_DATA,
            Event::ClipboardImage { .. } => EVENT_CLIPBOARD_IMAGE,
        }
    }

    fn payload_len(&self) -> usize {
        match self {
            Event::MouseMotion { .. } => 16,
            Event::MouseButton { .. } => 5,
            Event::MouseScroll { .. } => 9,
            Event::KeyPress { .. } | Event::KeyRelease { .. } => 4,
            Event::CaptureBegin { .. } | Event::ReturnToSender { .. } => 16,
            Event::ClipboardData { text } => 4 + text.len(),
            Event::ClipboardImage { format, data } => 2 + format.len() + 4 + data.len(),
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
            Event::ClipboardData { text } => {
                let len = text.len() as u32;
                buf[0..4].copy_from_slice(&len.to_be_bytes());
                buf[4..4 + text.len()].copy_from_slice(text.as_bytes());
            }
            Event::ClipboardImage { format, data } => {
                let fmt_len = format.len() as u16;
                buf[0..2].copy_from_slice(&fmt_len.to_be_bytes());
                let fmt_end = 2 + format.len();
                buf[2..fmt_end].copy_from_slice(format.as_bytes());
                let data_len = data.len() as u32;
                buf[fmt_end..fmt_end + 4].copy_from_slice(&data_len.to_be_bytes());
                buf[fmt_end + 4..fmt_end + 4 + data.len()].copy_from_slice(data);
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
            EVENT_CLIPBOARD_DATA => {
                if buf.len() < 4 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let text_len = u32::from_be_bytes(buf[0..4].try_into().unwrap()) as usize;
                if buf.len() < 4 + text_len {
                    return Err(DecodeError::TruncatedPayload);
                }
                let text = String::from_utf8_lossy(&buf[4..4 + text_len]).into_owned();
                Ok(Event::ClipboardData { text })
            }
            EVENT_CLIPBOARD_IMAGE => {
                if buf.len() < 2 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let fmt_len = u16::from_be_bytes(buf[0..2].try_into().unwrap()) as usize;
                let fmt_end = 2 + fmt_len;
                if buf.len() < fmt_end + 4 {
                    return Err(DecodeError::TruncatedPayload);
                }
                let format = String::from_utf8_lossy(&buf[2..fmt_end]).into_owned();
                let data_len = u32::from_be_bytes(
                    buf[fmt_end..fmt_end + 4].try_into().unwrap(),
                ) as usize;
                if buf.len() < fmt_end + 4 + data_len {
                    return Err(DecodeError::TruncatedPayload);
                }
                let data = buf[fmt_end + 4..fmt_end + 4 + data_len].to_vec();
                Ok(Event::ClipboardImage { format, data })
            }
            _ => Err(DecodeError::UnknownEventType(type_byte)),
        }
    }
}

#[derive(Debug)]
pub enum DecodeError {
    Io(io::Error),
    UnknownEventType(u8),
    TruncatedPayload,
    PayloadTooLarge(u32),
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

/// Maximum frame payload (type byte + event payload). 32 MiB is enough for
/// PNG clipboard images well beyond typical 4K screenshots while staying
/// far below u32::MAX.
pub const MAX_FRAME_PAYLOAD: u32 = 32 * 1024 * 1024;

/// Length-prefix width on the wire (u32 big-endian).
const LEN_PREFIX: usize = 4;

/// Largest fixed-size event payload (MouseMotion, CaptureBegin, ReturnToSender).
const MAX_FIXED_PAYLOAD: usize = 16;

/// Write a length-prefixed event to the stream. The frame format is:
/// [u32 BE length][u8 event_type][payload bytes]
/// Length includes the type byte and payload, but not itself.
pub async fn write_event<W: AsyncWrite + Unpin>(w: &mut W, event: &Event) -> io::Result<()> {
    let payload_len = event.payload_len();
    let frame_bytes = 1 + payload_len;
    if frame_bytes > MAX_FRAME_PAYLOAD as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("event payload too large ({} bytes)", frame_bytes),
        ));
    }
    let frame_len = frame_bytes as u32;

    if payload_len <= MAX_FIXED_PAYLOAD {
        let mut buf = [0u8; LEN_PREFIX + 1 + MAX_FIXED_PAYLOAD];
        buf[0..LEN_PREFIX].copy_from_slice(&frame_len.to_be_bytes());
        buf[LEN_PREFIX] = event.type_byte();
        event.encode_payload(&mut buf[LEN_PREFIX + 1..LEN_PREFIX + 1 + payload_len]);
        w.write_all(&buf[..LEN_PREFIX + 1 + payload_len]).await
    } else {
        let mut buf = vec![0u8; LEN_PREFIX + 1 + payload_len];
        buf[0..LEN_PREFIX].copy_from_slice(&frame_len.to_be_bytes());
        buf[LEN_PREFIX] = event.type_byte();
        event.encode_payload(&mut buf[LEN_PREFIX + 1..]);
        w.write_all(&buf).await
    }
}

/// Read a length-prefixed event from the stream. Returns `DecodeError::ConnectionClosed`
/// on clean EOF.
///
/// WARNING: this function is NOT cancellation-safe. Cancelling the returned
/// future mid-read can drop bytes from the stream and desynchronise
/// subsequent reads. Callers that need cancellation safety (e.g. using
/// `recv` inside a `tokio::select!`) should use [`FrameReader`] instead,
/// which keeps received bytes in a persistent buffer across calls.
pub async fn read_event<R: AsyncRead + Unpin>(r: &mut R) -> Result<Event, DecodeError> {
    let mut len_buf = [0u8; LEN_PREFIX];
    match r.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
            return Err(DecodeError::ConnectionClosed);
        }
        Err(e) => return Err(DecodeError::Io(e)),
    }
    let frame_len = u32::from_be_bytes(len_buf);

    if frame_len == 0 {
        return Err(DecodeError::TruncatedPayload);
    }
    if frame_len > MAX_FRAME_PAYLOAD {
        return Err(DecodeError::PayloadTooLarge(frame_len));
    }

    let frame_usize = frame_len as usize;
    if frame_usize <= 1 + MAX_FIXED_PAYLOAD {
        let mut frame = [0u8; 1 + MAX_FIXED_PAYLOAD];
        r.read_exact(&mut frame[..frame_usize]).await?;
        let type_byte = frame[0];
        let payload = &frame[1..frame_usize];
        Event::decode_payload(type_byte, payload)
    } else {
        let mut frame = vec![0u8; frame_usize];
        r.read_exact(&mut frame).await?;
        let type_byte = frame[0];
        let payload = &frame[1..];
        Event::decode_payload(type_byte, payload)
    }
}

/// Try to pull one complete event out of an in-memory buffer. Returns
/// `Ok(None)` if the buffer does not yet hold a full frame. On success
/// returns the decoded event and the number of bytes consumed; the
/// caller should discard that many bytes from the front of the buffer.
pub fn try_decode_event(buf: &[u8]) -> Result<Option<(Event, usize)>, DecodeError> {
    if buf.len() < LEN_PREFIX {
        return Ok(None);
    }
    let frame_len = u32::from_be_bytes(buf[0..LEN_PREFIX].try_into().unwrap());
    if frame_len == 0 {
        return Err(DecodeError::TruncatedPayload);
    }
    if frame_len > MAX_FRAME_PAYLOAD {
        return Err(DecodeError::PayloadTooLarge(frame_len));
    }
    let frame_usize = frame_len as usize;
    let total = LEN_PREFIX + frame_usize;
    if buf.len() < total {
        return Ok(None);
    }
    let type_byte = buf[LEN_PREFIX];
    let payload = &buf[LEN_PREFIX + 1..total];
    let event = Event::decode_payload(type_byte, payload)?;
    Ok(Some((event, total)))
}

/// Cancellation-safe wrapper around an `AsyncRead` that accumulates
/// partial reads in an internal buffer. If the future returned by
/// `read_event` is dropped before completion, the bytes pulled off the
/// underlying stream remain in the buffer and the next call resumes
/// from the same point.
pub struct FrameReader<R> {
    inner: R,
    buf: Vec<u8>,
}

impl<R: AsyncRead + Unpin> FrameReader<R> {
    pub fn new(inner: R) -> Self {
        FrameReader { inner, buf: Vec::with_capacity(4096) }
    }

    pub fn get_mut(&mut self) -> &mut R {
        &mut self.inner
    }

    /// Read the next event. Cancellation-safe: if the future is dropped,
    /// any bytes already pulled from the stream are preserved.
    pub async fn read_event(&mut self) -> Result<Event, DecodeError> {
        loop {
            if let Some((event, consumed)) = try_decode_event(&self.buf)? {
                self.buf.drain(..consumed);
                return Ok(event);
            }
            let mut chunk = [0u8; 16 * 1024];
            let n = self.inner.read(&mut chunk).await?;
            if n == 0 {
                return Err(DecodeError::ConnectionClosed);
            }
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }
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
            Event::ClipboardData { text: "hello clipboard".to_string() },
            Event::ClipboardImage {
                format: "image/png".to_string(),
                data: vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
            },
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
        // u32 BE length=1, type=0xFF
        let frame = [0x00, 0x00, 0x00, 0x01, 0xFF];
        let result = read_event(&mut frame.as_slice()).await;
        assert!(matches!(result, Err(DecodeError::UnknownEventType(0xFF))));
    }

    #[tokio::test]
    async fn oversized_frame_rejected() {
        // Declare a frame length exceeding MAX_FRAME_PAYLOAD.
        let too_big = MAX_FRAME_PAYLOAD + 1;
        let mut frame = too_big.to_be_bytes().to_vec();
        frame.push(0xFF);
        let result = read_event(&mut frame.as_slice()).await;
        assert!(matches!(result, Err(DecodeError::PayloadTooLarge(len)) if len == too_big));
    }

    #[tokio::test]
    async fn clipboard_large_round_trip() {
        let text = "x".repeat(60_000);
        let event = Event::ClipboardData { text: text.clone() };
        let mut buf = Vec::new();
        write_event(&mut buf, &event).await.unwrap();
        let decoded = read_event(&mut buf.as_slice()).await.unwrap();
        assert_eq!(Event::ClipboardData { text }, decoded);
    }

    #[tokio::test]
    async fn clipboard_empty_round_trip() {
        let event = Event::ClipboardData { text: String::new() };
        let mut buf = Vec::new();
        write_event(&mut buf, &event).await.unwrap();
        let decoded = read_event(&mut buf.as_slice()).await.unwrap();
        assert_eq!(event, decoded);
    }

    #[tokio::test]
    async fn clipboard_image_multi_mb_round_trip() {
        // Simulate a realistic PNG screenshot payload (~4 MB) to exercise the
        // u32 length prefix and the heap-allocated write/read paths.
        let data: Vec<u8> = (0..4 * 1024 * 1024).map(|i| (i % 256) as u8).collect();
        let event = Event::ClipboardImage {
            format: "image/png".to_string(),
            data: data.clone(),
        };
        let mut buf = Vec::new();
        write_event(&mut buf, &event).await.unwrap();
        let decoded = read_event(&mut buf.as_slice()).await.unwrap();
        assert_eq!(
            Event::ClipboardImage { format: "image/png".to_string(), data },
            decoded,
        );
    }

    #[tokio::test]
    async fn clipboard_image_empty_payload() {
        let event = Event::ClipboardImage {
            format: "image/png".to_string(),
            data: Vec::new(),
        };
        let mut buf = Vec::new();
        write_event(&mut buf, &event).await.unwrap();
        let decoded = read_event(&mut buf.as_slice()).await.unwrap();
        assert_eq!(event, decoded);
    }

    #[tokio::test]
    async fn frame_reader_handles_chunked_reads() {
        use tokio::io::AsyncWriteExt;

        // Duplex buffer generously sized so writes never block on the reader.
        let (mut w, r) = tokio::io::duplex(1 << 20);
        let mut fr = FrameReader::new(r);

        let event = Event::ClipboardImage {
            format: "image/png".to_string(),
            data: vec![0xAB; 10_000],
        };
        let mut bytes = Vec::new();
        write_event(&mut bytes, &event).await.unwrap();
        let mid = bytes.len() / 2;

        w.write_all(&bytes[..mid]).await.unwrap();
        tokio::task::yield_now().await;
        w.write_all(&bytes[mid..]).await.unwrap();

        let decoded = fr.read_event().await.unwrap();
        assert_eq!(decoded, event);
    }

    #[tokio::test]
    async fn frame_reader_survives_cancellation_mid_frame() {
        use tokio::io::AsyncWriteExt;

        let (mut w, r) = tokio::io::duplex(1 << 20);
        let mut fr = FrameReader::new(r);

        let event = Event::ClipboardImage {
            format: "image/png".to_string(),
            data: vec![0xCD; 10_000],
        };
        let mut bytes = Vec::new();
        write_event(&mut bytes, &event).await.unwrap();
        let mid = bytes.len() / 2;

        // Write only the first half; read_event will consume what is
        // available and then block waiting for the rest.
        w.write_all(&bytes[..mid]).await.unwrap();

        // Cancel read_event mid-frame. If FrameReader is cancel-safe, the
        // bytes it already consumed from the duplex survive in its
        // internal buffer and are reused on the next call.
        let timed = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            fr.read_event(),
        ).await;
        assert!(timed.is_err(), "expected timeout while frame is incomplete");

        // Write the remaining half. The next read_event must decode the
        // full event correctly (i.e. not interpret mid-frame bytes as a
        // bogus length header, which is the bug this test guards against).
        w.write_all(&bytes[mid..]).await.unwrap();
        let decoded = fr.read_event().await.unwrap();
        assert_eq!(decoded, event);
    }

    #[tokio::test]
    async fn write_rejects_payload_over_cap() {
        // One byte over MAX_FRAME_PAYLOAD including the type byte.
        let oversize = MAX_FRAME_PAYLOAD as usize; // type byte + payload = MAX+1
        let event = Event::ClipboardImage {
            format: String::new(),
            data: vec![0u8; oversize], // 2 (fmt_len) + 0 + 4 (data_len) + oversize bytes
        };
        let mut buf = Vec::new();
        let result = write_event(&mut buf, &event).await;
        assert!(result.is_err());
    }
}
