# Clipboard Sync Design

Text-only clipboard sharing between the Linux sender and macOS receiver. Copy on either machine, paste on either machine.

## How It Works

Clipboard syncs automatically on capture transitions:

1. **Cursor crosses from Linux to Mac** (CaptureBegin): the sender reads the Wayland clipboard and sends it to the receiver. The receiver writes it to the macOS pasteboard. The user can now paste on the Mac what they copied on Linux.

2. **Cursor crosses from Mac to Linux** (ReturnToSender): the receiver reads the macOS pasteboard and sends it to the sender. The sender writes it to the Wayland clipboard via `wl-copy`. The user can now paste on Linux what they copied on the Mac.

No hotkeys, no polling. It just happens when you switch machines.

## Protocol Changes

### New event type

```
ClipboardData { text: String }   // type byte: 0x40
```

Wire format: `[u32 BE text_length][utf8 bytes]`. Payload size is `4 + text.len()`.

### Frame size expansion

The current `MAX_FRAME_PAYLOAD` is 17 bytes (sized for MouseMotion, the largest existing event). Clipboard text can be much larger.

Expand `MAX_FRAME_PAYLOAD` to 65535 (the maximum a u16 length prefix can address). This allows clipboard payloads up to ~64KB in a single frame, which covers virtually all real-world clipboard text.

The `write_event` function currently uses a fixed stack buffer (`[u8; 19]`). For clipboard events, use a heap-allocated `Vec<u8>`. Keep the stack buffer fast path for small events (all existing event types).

The `read_event` function's stack buffer (`[u8; 17]`) similarly needs a heap path for large frames. A simple branch on `frame_len <= 17` keeps the hot path unchanged.

### Size cap

Silently drop clipboard text larger than ~64KB. This avoids unbounded memory allocation from a single frame and covers all practical clipboard use. If someone copies a 100KB log file, it just doesn't sync.

## Clipboard Access

### Linux (sender)

Shell out to `wl-paste` and `wl-copy` via `tokio::process::Command`:

- **Read**: `wl-paste --no-newline --type text/plain` with a 1-second timeout. Returns `None` on error, timeout, or empty clipboard.
- **Write**: pipe text to `wl-copy` stdin. Fire-and-forget with a 1-second timeout.

`wl-paste` does not require surface focus. `wl-copy` works from the sender's Wayland environment. Both are part of the widely-available `wl-clipboard` package.

If `wl-paste`/`wl-copy` are not installed, log a warning at startup and skip clipboard sync. The feature is best-effort.

### macOS (receiver)

Shell out to `pbpaste` and `pbcopy` via `tokio::process::Command`:

- **Read**: `pbpaste` with a 1-second timeout.
- **Write**: pipe text to `pbcopy` stdin.

Both are built-in macOS utilities. No additional dependencies.

## Deduplication

Each side keeps a hash of the last clipboard text it sent (`u64` via `DefaultHasher`). If the clipboard hasn't changed since the last transition, skip sending. This avoids redundant transfers on rapid edge crossings.

## Integration Points

### Sender (`styx-sender/src/main.rs`)

On `CaptureEvent::Begin`, after sending `CaptureBegin`:
```
read clipboard -> hash -> if changed, send ClipboardData
```

On receiving `Event::ClipboardData`:
```
write text to clipboard via wl-copy
```

### Receiver (`styx-receiver/src/main.rs`)

On cursor hitting the return edge, before/alongside sending `ReturnToSender`:
```
read clipboard -> hash -> if changed, send ClipboardData
```

On receiving `Event::ClipboardData` in `handle_event`:
```
write text to clipboard via pbcopy
```

## Files to Create/Modify

| File | Change |
|------|--------|
| `styx-proto/src/wire.rs` | Add `ClipboardData` variant, expand `MAX_FRAME_PAYLOAD`, heap-allocate for large frames |
| `styx-sender/src/clipboard.rs` | New: `read_clipboard()`, `write_clipboard()` using wl-paste/wl-copy |
| `styx-sender/src/main.rs` | Send clipboard on CaptureBegin, handle incoming ClipboardData |
| `styx-sender/Cargo.toml` | Add tokio `process` feature |
| `styx-receiver/src/clipboard.rs` | New: `read_clipboard()`, `write_clipboard()` using pbpaste/pbcopy |
| `styx-receiver/src/main.rs` | Send clipboard on ReturnToSender, handle incoming ClipboardData |
| `styx-receiver/Cargo.toml` | Add tokio `process` feature |

## What This Doesn't Cover

- **Images or rich content.** Text only. MIME type negotiation and large binary transfers are out of scope.
- **Real-time sync.** Clipboard only syncs on capture transitions, not continuously.
- **Clipboard history.** Only the current clipboard is synced. No history is maintained.
- **Encryption.** Clipboard contents travel in plaintext over TCP, same as all other styx traffic. Use on a trusted network.
