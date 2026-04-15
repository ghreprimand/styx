# Wire Protocol

Reference for the TCP framing and event encoding used by styx 0.5.0. Covers enough detail that a compliant sender or receiver could be implemented in another language.

## Framing

Every message on the wire is a single frame:

```
[u32 BE frame_len][u8 event_type][payload bytes]
```

- `frame_len` is a 4-byte big-endian unsigned integer. It counts the bytes after itself: `1 + payload_len`.
- `event_type` is a 1-byte tag identifying the payload layout.
- `payload bytes` is a per-type byte sequence, described below.

The maximum frame payload is `32 MiB` (`MAX_FRAME_PAYLOAD` in `styx-proto/src/wire.rs`). A sender whose payload would exceed this returns an error before writing. A reader rejects a frame whose declared length exceeds it.

## Byte-order and type conventions

- All multi-byte integers are big-endian.
- All floats are IEEE 754 `f64` in big-endian byte order (8 bytes).
- All strings are UTF-8. A string field is always preceded by an explicit byte count; it is never null-terminated. Invalid UTF-8 is replaced lossily on decode.

## Event catalogue

| Tag | Name | Payload |
|-----|------|---------|
| `0x01` | MouseMotion | `f64 dx`, `f64 dy` |
| `0x02` | MouseButton | `u32 button`, `u8 state` |
| `0x03` | MouseScroll | `u8 axis`, `f64 value` |
| `0x10` | KeyPress | `u32 code` |
| `0x11` | KeyRelease | `u32 code` |
| `0x20` | CaptureBegin | `f64 from_bottom`, `f64 source_height` |
| `0x21` | CaptureEnd | (no payload) |
| `0x22` | ReturnToSender | `f64 from_bottom`, `f64 source_height` |
| `0x30` | Heartbeat | (no payload) |
| `0x31` | HeartbeatAck | (no payload) |
| `0x40` | ClipboardData | `u32 text_len`, UTF-8 text |
| `0x41` | ClipboardImage | `u16 format_len`, format UTF-8, `u32 data_len`, image bytes |
| `0x42` | ClipboardHtml | `u32 html_len`, html UTF-8, `u32 plain_len`, plain UTF-8 |

### Motion and buttons

`MouseMotion.dx` and `MouseMotion.dy` are relative pixel deltas from the source. The receiver applies macOS's own acceleration curve to these raw deltas; the sender does not pre-accelerate.

`MouseButton.button` is a Linux `BTN_*` constant (`BTN_LEFT = 0x110`, `BTN_RIGHT = 0x111`, `BTN_MIDDLE = 0x112`, and so on). `state` is `1` for press, `0` for release.

`MouseScroll.axis` is `0` for vertical, `1` for horizontal. `value` follows the evdev convention: positive values scroll up or right.

### Key events

`KeyPress.code` and `KeyRelease.code` are evdev scan codes (the `KEY_*` constants from `linux/input-event-codes.h`). The receiver translates these to macOS HID usage IDs via `styx-keymap`.

### Capture transitions

`CaptureBegin` fires when the cursor crosses from the sender into the receiver. `ReturnToSender` fires when the cursor crosses back from the receiver into the sender. Both carry the same two fields:

- `from_bottom`: the pixel distance from the bottom of the source monitor at the moment of crossing.
- `source_height`: the total pixel height of the source monitor.

The receiving side uses these to map the cursor's vertical position proportionally onto its own displays. See the README "Cursor Position Mapping" section for the full description.

`CaptureEnd` is sent by the sender after it releases the grab (after receiving `ReturnToSender` and completing its key-release sweep). Receiver uses it as a synchronisation barrier; the receiver's post-crossover loop discards buffered mouse motion until either `CaptureEnd` arrives (clean end) or a new `CaptureBegin` arrives (immediate re-cross).

### Heartbeats

Sender periodically sends `Heartbeat`; receiver responds with `HeartbeatAck`. The sender's interval adapts: 1 s during active capture, 5 s while idle. Three missed acks trigger a reconnect.

### Clipboard

- **`ClipboardData`** carries plain UTF-8 text. `text_len` is a `u32` so the same event type works for the short-string case and the larger (tens of kilobytes) case a user might copy from a code editor or long chat message. Maximum text length is effectively `MAX_FRAME_PAYLOAD - 5` bytes.
- **`ClipboardImage`** carries a MIME-type string and raw image bytes. In 0.4.0+, the format is always `image/png` on the wire; the field exists so future versions can add types without a protocol bump. `data_len` is `u32`; payloads up to 32 MiB are permitted.
- **`ClipboardHtml`** (new in 0.5.0) carries an HTML rendering in `html` and a plain-text fallback in `plain`. Both are `u32`-length-prefixed UTF-8. Either field may be empty; a sender with plain-only content uses `ClipboardData` instead of shipping `ClipboardHtml` with an empty HTML field.

Clipboard preference on reading: `ClipboardImage` > `ClipboardHtml` > `ClipboardData`. Whichever type the source pasteboard exposes first is what ships.

Clipboard dedup: each side keeps a `u64` hash of the last clipboard content it sent or received, prefixed with a one-byte "kind" tag (`0x00` text, `0x01` image, `0x02` html) so cross-type transitions never collide.

Clipboard write notes:

- macOS can set both HTML and plain on the pasteboard at the same time, so `ClipboardHtml` is faithfully reproduced when the Mac receives it.
- Linux's `wl-copy` accepts a single MIME type per invocation. When the sender receives `ClipboardHtml`, it writes only the `plain` field to the Wayland clipboard. If `plain` is empty, the sender strips HTML tags with a simple heuristic (drop everything between `<` and `>`, decode the five XML entities, collapse whitespace) and writes the result.

## Cancellation safety

`FrameReader` in `styx-proto/src/wire.rs` wraps any `AsyncRead` and accumulates bytes in an internal `Vec<u8>`. It reads in chunks of up to 16 KiB via `AsyncReadExt::read` (not `read_exact`), which returns as soon as any bytes are available and is safe to cancel mid-call.

A caller that uses `FrameReader::read_event` inside a `tokio::select!` and has its future cancelled because another arm fires keeps every byte already consumed from the underlying stream in the buffer. The next call to `read_event` resumes from where the cancellation left off, so the stream never desynchronises.

The older `wire::read_event` free function uses `read_exact` and is not cancellation-safe. It is kept for simple blocking uses (tests, direct socket reads) but transports should prefer `FrameReader`.

The `try_decode_event(&[u8])` free function is a pure parser: given a buffer, it returns either `Ok(Some((event, bytes_consumed)))` if a complete frame is present, `Ok(None)` if more bytes are needed, or `Err(DecodeError)` for a malformed frame.

## Versioning

The major version (`0`) is reserved for the pre-1.0 era and will not bump until styx reaches stable API. The minor version (`4`, `5`, …) bumps whenever the wire protocol changes in a way that breaks compatibility with prior versions.

- **0.3.x** used a `u16` frame length prefix (max payload ~64 KiB) and events `0x01`-`0x40`.
- **0.4.x** widened the prefix to `u32` (max payload 32 MiB) and added `ClipboardImage` (`0x41`). 0.3.x peers cannot exchange frames with 0.4.x peers.
- **0.5.x** added `ClipboardHtml` (`0x42`). A 0.4.x reader encountering `0x42` responds with `DecodeError::UnknownEventType` and disconnects; a 0.4.x sender never emits 0x42. Clipboard interop between 0.4 and 0.5 works for text and images but falls back to text when the 0.5 side has HTML-only content.

Both sender and receiver must be upgraded together across a minor-version change. Sender and receiver are not required to have matching patch versions.

## Writing a compatible client

A minimal compliant receiver needs:

1. A TCP listener on the configured port.
2. A `FrameReader` (or equivalent length-prefixed reader) that decodes into the `Event` enum.
3. Per-event handlers for at least `MouseMotion`, `MouseButton`, `KeyPress`, `KeyRelease`, `Heartbeat` (respond with `HeartbeatAck`), and `CaptureBegin` / `CaptureEnd`.
4. Handlers for clipboard events if you want clipboard sync.
5. A release-all-keys mechanism that fires on disconnect or graceful shutdown so held keys do not get stuck.

A minimal compliant sender needs:

1. A TCP client that reconnects with backoff on failure.
2. An event writer using `write_event` or a direct `write_all` of `[u32 BE len][type][payload]`.
3. Edge detection of some form (styx uses wlr-layer-shell; any mechanism works as long as it produces the right events).
4. A heartbeat timer and a missed-heartbeat threshold.
5. A key-release sweep on disconnect and on `ReturnToSender`.

All unit tests in `styx-proto/src/wire.rs` exercise the framing end-to-end on a `tokio::io::duplex` pair; they are the clearest executable reference for the protocol.
