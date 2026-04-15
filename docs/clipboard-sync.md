# Clipboard Sync

Reference for how styx 0.5.0 synchronises clipboard contents between the Linux sender and the macOS receiver. Covers text, PNG images, and rich-text (HTML).

## Summary

Text, rich text (HTML), and PNG images copied on one machine become available for paste on the other over the existing TCP connection. Both directions work. Copy on either machine, paste on either machine.

Clipboard sync is deliberately scoped:

- Text (UTF-8), HTML, and PNG images. TIFF on the Mac side is transparently transcoded to PNG before it reaches the wire. PDF, RTF, and custom pasteboard types are not handled.
- One-way-at-a-time: whichever side produces content first wins the round. If both users copy something at the same time on different machines, the last-written content wins on both sides after the next sync event.
- Images are capped at 32 MiB per transfer; text at 1 MiB; HTML + plain combined at the same 32 MiB frame cap. Oversized content is dropped with a log warning.
- Preference order on every read: image > HTML > plain text. If multiple types are present on the source pasteboard, the richest one wins.

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

Three event types carry clipboard content:

```
ClipboardData  (type 0x40): [u32 BE text_len][UTF-8 bytes]
ClipboardImage (type 0x41): [u16 BE format_len][format UTF-8][u32 BE data_len][image bytes]
ClipboardHtml  (type 0x42): [u32 BE html_len][html UTF-8][u32 BE plain_len][plain UTF-8]
```

Wrapped in the standard frame: `[u32 BE frame_len][type byte][payload]`.

`format` is always `image/png` for image content. The field exists so a future version can support additional image formats without a protocol bump; readers today reject anything other than `image/png` with a log warning.

`ClipboardHtml` carries the HTML representation *and* a plain-text fallback. A sender is free to leave the plain field empty when no plain-text representation is available on the source pasteboard; receivers that cannot set both MIME types simultaneously (notably `wl-copy` on Linux) must prefer the plain field, falling back to tag-stripping the HTML if plain is empty.

Payloads are capped at 32 MiB (`MAX_FRAME_PAYLOAD` in `styx-proto/src/wire.rs`, minus a small reserve for headers). The 32 MiB cap is shared across all event types, not per-event.

Protocol compatibility:

- 0.3.x used a `u16` frame length prefix and events `0x01`-`0x40`. 0.4.x+ uses `u32` and adds `ClipboardImage` (`0x41`). 0.3 ↔ 0.4 is incompatible.
- 0.5.x adds `ClipboardHtml` (`0x42`). A 0.4.x reader encountering `0x42` disconnects with an unknown-event-type error; a 0.4.x sender never emits `0x42`. Image and plain-text clipboard interop between 0.4 and 0.5 continues to work; HTML content sent from 0.5 to 0.4 is lost. Upgrade both sides together for full support.

## Deduplication

Each side keeps a single `u64` hash of the last clipboard content it sent or received. On every read-before-send, the current hash is compared against `last_clip_hash`; if they match, the send is skipped. On every received clipboard event, `last_clip_hash` is updated to the new content's hash *before* writing to the local clipboard, so the subsequent local bump does not get interpreted as fresh user input and re-sent.

The hash is computed with Rust's `DefaultHasher` over a leading kind byte plus the payload:

- `0x00 ++ text`
- `0x01 ++ format ++ image_bytes`
- `0x02 ++ html ++ plain`

The kind byte ensures text, image, and HTML hashes never collide, so the dedup state is stable even when the user rapidly alternates between types (e.g. copying a code snippet from Safari as rich text, then copying a file path as plain text, then copying a screenshot).

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

When no PNG is on the pasteboard but `NSPasteboardTypeTIFF` is present, the receiver loads the TIFF into `NSBitmapImageRep::imageRepWithData` and re-encodes it via `representationUsingType_properties(NSBitmapImageFileType::PNG, ...)`. The Linux side sees only PNG on the wire regardless of the source format.

HTML reads and writes use two pasteboard types in one `autoreleasepool`:

- Read: `dataForType(NSPasteboardTypeHTML)` for the HTML, `dataForType(NSPasteboardTypeString)` for the plain fallback. Both are UTF-8.
- Write: after `clearContents()`, `setData_forType` is called for each of the two UTIs, so rich paste targets like Mail and Pages get formatted output while plain targets like Terminal still get clean text.

All NSPasteboard calls happen on `tokio::task::spawn_blocking` to keep them off the async runtime's worker threads.

Text-only reads and writes use `pbpaste` and `pbcopy` subprocesses — same pattern as the Linux side, but without the list-types probe (macOS does not expose that directly, and the PNG and HTML reads serve as the image-presence and HTML-presence probes).

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

## Mac-to-Linux HTML asymmetry

The Linux side writes clipboard content with `wl-copy`, which accepts exactly one MIME type per invocation. There is no native way to atomically publish text/html and text/plain simultaneously from a single wl-copy call.

When the sender receives a `ClipboardHtml` event, it writes only the `plain` field via `wl-copy`. If `plain` is empty, it runs the HTML through a small tag-stripping heuristic (drop everything between `<` and `>`, decode `&amp;`/`&lt;`/`&gt;`/`&quot;`/`&apos;`/`&#39;`/`&nbsp;`, collapse whitespace) and writes that.

Practical effect:

- **Linux → Mac rich text**: works fully. wl-paste returns both `text/html` and `text/plain` when a browser puts rich text on the Wayland clipboard, the sender ships both, the Mac sets both on NSPasteboard, rich-paste targets on the Mac render the formatting.
- **Mac → Linux rich text**: plain-text only. The Mac ships both html and plain; the Linux sender writes only the plain field. Users pasting on Linux get correct text content without formatting.

If native multi-MIME write on Wayland becomes easier (for example via a `wl-clipboard` feature addition or a small styx-side persistent clipboard helper), the Mac → Linux direction can be upgraded without a protocol change.

## Out of Scope

- TIFF-only images survive via Mac-side transcoding but other non-PNG image formats (PDF, WebP, HEIC) are not read or written. Users copying from apps that only expose these formats will see the text fallback instead of the image.
- RTF clipboard content. Most mac apps pair RTF with plain text on the pasteboard, so the plain-text fallback still carries the content — but the formatting is lost.
- Clipboard history. Only the current contents are synced; there is no multi-item buffer.
- Real-time streaming clipboards. If the user changes the clipboard 15 times in a second on the Mac, the 10 Hz poll may coalesce those into one or two syncs; the latest content always wins.
- Encryption. Clipboard content travels in plaintext over the same TCP connection as input events. See `docs/security.md` for the full threat model.
