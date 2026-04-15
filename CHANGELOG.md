# Changelog

All notable changes to styx are documented here. Versions follow semantic versioning: the major version tracks wire-protocol compatibility, the minor version tracks feature additions, the patch version tracks bug fixes and non-breaking tweaks.

## 0.4.0 — 2026-04-15

### Added

- **PNG image clipboard sync** in both directions. Copying an image in Preview, macOS screenshots, GIMP, Firefox's "Copy Image", or any other app that places `image/png` on the clipboard now syncs to the other machine in the same way text does. Capped at 32 MiB per transfer.
- **Proactive macOS clipboard sync.** The receiver polls `NSPasteboard.changeCount` at 10 Hz in a background task and pushes new content to the sender as soon as it lands on the pasteboard. Reduces the worst-case latency between `Cmd+C` on the Mac and `Ctrl+V` on Linux to roughly one poll interval plus network round-trip (~100–150 ms on a LAN).
- **Deferred-to-pbpaste fallback** for lazy pasteboard providers. When the PNG on the macOS pasteboard has not changed since the last sync, the receiver falls through to `pbpaste`, which materialises lazy text providers (e.g. Ghostty `Cmd+C`) correctly via its own `NSRunLoop`.
- **Cancellation-safe frame reader.** `FrameReader` in `styx-proto/src/wire.rs` accumulates bytes in a persistent buffer so a `tokio::select!` cancelling the recv future mid-frame (e.g. when a heartbeat tick fires during a large image transfer) does not lose bytes or desynchronise the stream.
- **Compositor grab-suppression cooldown** on the Linux sender. If Hyprland enters a tight loop pulling the pointer grab back from the edge surface (possible when the Mac receiver is restarting while the cursor is parked on the edge), the sender arms an exponential backoff (300 ms, 1 s, 5 s, 30 s) and silences its Wayland `Enter` handler to prevent the lock/unlock thrash that was starving keyboard input.

### Changed

- **Breaking:** the TCP frame length prefix widened from `u16` big-endian (max 64 KiB) to `u32` big-endian (max 32 MiB). A 0.3.x peer cannot exchange frames with a 0.4.x peer; both the sender and receiver must be upgraded together.
- The receiver no longer reads the pasteboard on cursor-hits-return-edge. The proactive poll has already synced the content by the time the user crosses, and reading on edge-cross introduced a race where `Cmd+C` immediately before crossing would ship stale content (the pasteboard had not finished settling).
- Clipboard hashing now incorporates a leading kind byte (`0x00` for text, `0x01` for image) so text and image hashes never collide in the dedup state.

### Fixed

- Stuck-pointer-grab loop in the sender that caused perceptible keyboard input drops (~10 s of missed keystrokes) when Hyprland force-released the pointer faster than the main loop could drain the resulting `Released` events. The rc4 cooldown gated the evdev keyboard grab but not the Wayland pointer lock; rc7 extends the cooldown into `capture.rs` so the Wayland `Enter` handler short-circuits during the backoff window.
- Transient "Connection reset by peer" after large (>128 KiB) image transfers. Traced to `tokio::io::AsyncReadExt::read_exact` inside a `tokio::select!`: the heartbeat arm could cancel the recv future mid-frame, and the next read interpreted the remaining bytes of the image as a fresh frame length. The new `FrameReader` closes this window.

### Requirements

No new user-visible dependencies. The receiver links `objc2`, `objc2-app-kit` (with the `NSPasteboard` feature), and `objc2-foundation` to read and write PNG data via AppKit; these are compile-time only.

## 0.3.0 — 2026-03-26

### Added

- Text clipboard sync on capture transitions (Linux to Mac on crossover, Mac to Linux on return). UTF-8 only, capped at ~64 KiB, deduplicated against the last-sent hash on each side.
- Multi-monitor crossover edge: the sender can span `monitors = ["HDMI-A-1", "DP-1"]` in config and the receiver unions any displays whose return edge lines up with the outermost edge (within 64 points of tolerance).

## 0.2.5 — 2026-03-18

### Added

- Display wake on input: the receiver declares user activity via `IOPMAssertionDeclareUserActivity` on every injected event so a Mac whose external display has slept via idle timeout wakes on the first crossover.

### Fixed

- Double-click tracking: per-button click counts within a 500 ms interval so double- and triple-clicks register correctly on macOS.

## 0.2.4 — 2026-03-10

### Fixed

- Cursor position on scaled Hyprland monitors. All cursor math now uses logical coordinates so HiDPI setups (2x scaling) place the pointer correctly on entry.

## 0.2.3 and earlier

Initial public releases. Bidirectional edge-based mouse/keyboard sharing between Hyprland and macOS, TCP transport, evdev keyboard grab, layer-shell mouse edge detection, adaptive heartbeat.
