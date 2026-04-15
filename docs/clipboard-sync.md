# Clipboard Sync

Reference for how styx 0.4.0 synchronises clipboard contents between the Linux sender and the macOS receiver. Covers both text and PNG images.

## Summary

Text and PNG images copied on one machine become available for paste on the other over the existing TCP connection. Both directions work. Copy on either machine, paste on either machine.

Clipboard sync is deliberately scoped:

- Text (UTF-8) and PNG images. No TIFF, PDF, HTML, RTF, or custom pasteboard types.
- One-way-at-a-time: whichever side produces content first wins the round. If both users copy something at the same time on different machines, the last-written content wins on both sides after the next sync event.
- Images are capped at 32 MiB per transfer, text at roughly 64 KiB. Oversized content is dropped with a log warning.
- Preference order on every read: image first, text second. If both are present on the source pasteboard, the image wins.

## Trigger Points

Two paths drive a clipboard send:

### Linux to Mac: edge-triggered

When the cursor hits the sender's configured edge surface and capture begins, the sender reads the Wayland clipboard and forwards it to the receiver. The receiver writes the content to `NSPasteboard` before handling the first injected event. By the time the user pastes on the Mac, the content is already there.

Why edge-triggered and not polling on the Linux side: `wl-paste` requires shelling out to a subprocess, which is too expensive to run 10 times a second. Crossover is a natural sync point and happens often enough to feel instant.

### Mac to Linux: changeCount polling

A background `tokio` task on the receiver polls `NSPasteboard.changeCount` at 10 Hz. `changeCount` is a monotonic integer the macOS pasteboard server bumps on every mutation (`clearContents`, `setData:forType:`, `declareTypes:owner:`). Reading it is a cheap IPC round-trip, so polling is effectively free when nothing has changed.

When `changeCount` advances, the receiver reads the pasteboard (`NSPasteboardTypePNG` first, `pbpaste` for text as fallback), deduplicates against the last synced hash, and sends the content to the sender over TCP. The sender writes it to the Wayland clipboard via `wl-copy`.

Because sync is continuous, the Linux clipboard is usually already populated by the time the user crosses back. Worst-case latency between `Cmd+C` on the Mac and `Ctrl+V` being ready on Linux is one poll interval (~100 ms) plus TCP round-trip plus `wl-copy` spawn — typically under 150 ms on a LAN.

## Wire Protocol

Two event types carry clipboard content:

```
ClipboardData  (type 0x40): [u32 BE text_len][UTF-8 bytes]
ClipboardImage (type 0x41): [u16 BE format_len][format UTF-8][u32 BE data_len][image bytes]
```

Wrapped in the standard frame: `[u32 BE frame_len][type byte][payload]`.

`format` is always `image/png` for image content. The field exists so a future version can support additional formats without a protocol bump; readers today reject anything other than `image/png` with a log warning.

Payloads are capped at 32 MiB (`MAX_FRAME_PAYLOAD` in `styx-proto/src/wire.rs`, minus a small reserve for headers). The 32 MiB cap is shared across all event types, not per-event.

The `u32` length prefix is a breaking change from 0.3.x, which used `u16` (64 KiB maximum frame). A 0.3.x peer cannot exchange frames with a 0.4.x peer; both sides must be upgraded together.

## Deduplication

Each side keeps a single `u64` hash of the last clipboard content it sent or received. On every read-before-send, the current hash is compared against `last_clip_hash`; if they match, the send is skipped. On every received clipboard event, `last_clip_hash` is updated to the new content's hash *before* writing to the local clipboard, so the subsequent local bump does not get interpreted as fresh user input and re-sent.

The hash is computed with Rust's `DefaultHasher` over a leading kind byte plus the payload:

- `0x00 ++ text`
- `0x01 ++ format ++ image_bytes`

The kind byte ensures text and image hashes never collide, so the dedup state is stable even when the user rapidly alternates between text and image copies.

## macOS Lazy Pasteboard Providers

Some macOS apps (notably terminal emulators like Ghostty) register themselves as lazy pasteboard providers via `declareTypes:owner:` rather than writing the content up front with `setString:forType:`. When a consumer asks for the data, AppKit calls back into the provider to materialise it.

On a full `NSRunLoop`, this works transparently. From a `tokio` worker thread calling `dataForType` directly, the materialisation callback does not always fire and the read returns `nil` even though the pasteboard claims the type is present.

The receiver works around this two ways:

1. **Defer to `pbpaste` when the PNG has not changed.** If the current PNG matches the last one we saw or wrote (`LAST_PNG_HASH` in `styx-receiver/src/clipboard_image.rs`), the image read returns `None` and the text path runs via `pbpaste`. `pbpaste` is a separate process with its own runloop and materialises lazy providers correctly.
2. **Fall back to `pbpaste` whenever the image path yields nothing.** `pbpaste` additionally handles RTF→plain-text conversion and a few other pasteboard quirks without the caller having to enumerate every UTI.

The net effect: text from lazy-provider apps always makes it to Linux on the next poll tick or the next crossover, whichever comes first.

## Compositor Side (Linux)

`wl-paste` and `wl-copy` (from the `wl-clipboard` package) are the read and write paths. Both are invoked via `tokio::process::Command` with a 1-second timeout; if either fails or times out, the send is skipped with a log warning rather than blocking the event loop.

`wl-paste --list-types` is used to probe for `image/png` before attempting an image read. If the list-types call fails, the sender falls back to text.

Neither `wl-paste` nor `wl-copy` requires surface focus, so they work from the sender's detached Wayland context.

## NSPasteboard Side (macOS)

PNG reads and writes use `objc2-app-kit` and `objc2-foundation`:

- Read: `NSPasteboard.generalPasteboard().dataForType(NSPasteboardTypePNG)`, copied into a `Vec<u8>` inside an `autoreleasepool`.
- Write: `NSPasteboard.clearContents()`, then `setData_forType(NSData::with_bytes(&bytes), NSPasteboardTypePNG)`.
- Change detection: `NSPasteboard.changeCount()` returns `NSInteger` (`isize` in Rust).

All NSPasteboard calls happen on `tokio::task::spawn_blocking` to keep them off the async runtime's worker threads.

Text reads and writes use `pbpaste` and `pbcopy` subprocesses — same pattern as the Linux side, but without the list-types probe (macOS does not expose that directly, and the PNG read serves as the image-presence probe).

## Echo Prevention

Echoing (a PNG we wrote to one machine coming back from the other as "new" content and being re-synced) is prevented by the `last_clip_hash` state plus a local `LAST_PNG_HASH` atomic on the mac side.

Sequence when the sender writes an image to the mac:

1. Sender reads Wayland clipboard on crossover, sends `ClipboardImage { format, data }`.
2. Receiver updates `last_clip_hash` to `hash_image(format, data)`.
3. Receiver writes to `NSPasteboard`, which also updates `LAST_PNG_HASH` to `hash_bytes(data)`.
4. The write bumps `changeCount`. The poll task notices.
5. Poll task reads the pasteboard. `read_clipboard_image` sees the current bytes hash to `LAST_PNG_HASH` and returns `None`. The text fallback also sees the same content (the PNG was written, no text was added) and returns `None`. No event is sent.

Sequence when the user overwrites with fresh content on the mac:

1. User copies new text in a mac app.
2. `changeCount` bumps. Poll reads.
3. `read_clipboard_image` returns `None` because the PNG (if any stale one remains) matches `LAST_PNG_HASH`.
4. `pbpaste` returns the new text. Hash differs from `last_clip_hash`. Event sent.

## Files

| File | Role |
|------|------|
| `styx-proto/src/wire.rs` | `ClipboardData` / `ClipboardImage` encoding, frame size cap, cancellation-safe `FrameReader` |
| `styx-sender/src/clipboard.rs` | Linux read/write via `wl-paste`/`wl-copy`, hash helpers |
| `styx-sender/src/main.rs` | Edge-trigger clipboard send on `CaptureBegin`, inbound `ClipboardData`/`ClipboardImage` write |
| `styx-receiver/src/clipboard.rs` | macOS text read/write via `pbpaste`/`pbcopy`, hash helpers |
| `styx-receiver/src/clipboard_image.rs` | macOS image read/write via `NSPasteboard`, `changeCount` reader, `LAST_PNG_HASH` echo guard |
| `styx-receiver/src/main.rs` | Proactive `proactive_clipboard_poll` task, inbound `ClipboardData`/`ClipboardImage` write |

## Out of Scope

- TIFF, PDF, HTML, RTF, and other non-PNG image formats. Users copying from apps that only expose TIFF (for example, some legacy image editors) will see the text fallback instead of the image.
- Clipboard history. Only the current contents are synced; there is no multi-item buffer.
- Real-time streaming clipboards (e.g. `TEAK` or similar protocols). If the user changes the clipboard 15 times in a second on the Mac, the poll may coalesce those into one or two syncs; the latest content always wins.
- Encryption. Clipboard content travels in plaintext over the same TCP connection as input events. Use on a trusted network or behind a VPN.
