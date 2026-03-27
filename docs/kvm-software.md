# Styx — Research & Design

Prior art, failed approaches, and design rationale for the styx project: a
purpose-built software KVM for Hyprland and macOS.

## Goal

Share a Linux machine's physical keyboard and mouse with a Mac on the same
network. Move the mouse off a configurable screen edge to seamlessly control
the Mac. Move it back off the Mac's opposite edge to return to Linux.

## The Core Problem: Hyprland Lacks InputCapture Portal

Every Synergy-family tool (Input Leap, Deskflow, Barrier, Synergy) needs the
`org.freedesktop.portal.InputCapture` XDG portal interface to detect the mouse
hitting the screen edge on Wayland. **Hyprland does not implement this portal.**

- KDE Plasma 6.1+ and GNOME 46+ support it natively
- xdg-desktop-portal-hyprland only implements Screenshot, ScreenCast, and GlobalShortcuts
- There are active draft PRs to add it:
  - [Hyprland PR #7919](https://github.com/hyprwm/Hyprland/pull/7919) — protocol-level input capture
  - [xdg-desktop-portal-hyprland PR #268](https://github.com/hyprwm/xdg-desktop-portal-hyprland/pull/268) — portal backend
- Both are close to landing but have no merge date as of March 2026

When these PRs merge, Input Leap will work out of the box on Hyprland.

## Tools Evaluated

### Input Leap 3.0.3

- **Result**: Server starts, Mac client connects and authenticates over SSL
- **Blocker**: libei/InputCapture backend fails (`CreateSession failed`) because
  Hyprland doesn't implement the portal. X11/XWayland fallback connects but
  warns "will not work as expected" — mouse edge capture doesn't work through
  XWayland because the compositor owns the pointer
- **When to revisit**: After Hyprland merges InputCapture support

### lan-mouse 0.10.0 (and git main)

- **Result**: The ONLY tool that can capture input on Hyprland (uses layer-shell
  protocol to create invisible edge surfaces — no portal needed)
- **What worked**: Linux to Mac mouse/keyboard sharing functioned on v0.10.0
- **Blockers**:
  1. **Stuck keys (v0.10.0)**: UDP protocol is unreliable. Key-release events
     get dropped or never captured, causing keys to repeat infinitely on the
     Mac. Not network packet loss — confirmed 0% packet loss on LAN. The
     layer-shell capture layer occasionally fails to generate key-up events.
     This is a fundamental protocol design issue (no acknowledgments, no
     retransmission).
  2. **DTLS handshake failure (git main)**: The git main branch added DTLS
     encryption. The handshake fails between Linux and macOS with
     `Alert is Fatal or Close Notify`. Mac to Linux DTLS works, Linux to Mac
     doesn't. Appears to be an asymmetric bug in the DTLS client/server roles.
  3. **Dual NIC confusion**: If the Mac has both WiFi and ethernet active, UDP
     responses can route out the wrong interface, breaking DTLS.
  4. **Config format instability**: v0.10.0 uses `[left]`/`[right]` sections,
     git main uses `[[clients]]` array with `position` field.
  5. **DNS resolution leaks VPN IPs**: Setting hostname to the machine name can
     resolve to VPN/Tailscale IPs via DNS, adding unwanted connection targets.
- **Build notes**: Requires stripping `-flto=auto` from CFLAGS — the `ring`
  crate's C/asm objects break with GCC LTO + Rust linker.

### Deskflow 1.26.0

- **Result**: Same InputCapture portal blocker as Input Leap (same codebase lineage)
- **Also has stuck key issues**: Multiple open GitHub issues about modifier keys

### waynergy

- **Result**: Wayland Synergy CLIENT only — can receive input but not capture/send it.
  Would require the Mac to be the server, which is the opposite direction.

## Styx Design

Since lan-mouse is the only approach that works on Hyprland (layer-shell capture)
but has fatal reliability issues, styx is a purpose-built tool that keeps what
works (layer-shell edge detection) and replaces everything that doesn't (UDP
transport, DTLS, peer-to-peer complexity).

### Why It Will Work

lan-mouse's problems are in its protocol layer, not in the capture/emulation
mechanics. A simpler tool with reliable transport can avoid every issue
encountered during evaluation.

### Architecture

```
Linux (sender)                            Mac (receiver)
+-----------------+                      +-----------------+
| layer-shell     |   TCP connection     | macOS           |
| edge detection  | ------------------> | Accessibility   |
|                 |   key/mouse events  | API injection   |
| evdev capture   |                      |                 |
| (keyboard grab) | <------------------ | edge detection  |
|                 |   "mouse returned"  | (CGEvent tap)   |
| Hyprland IPC    |                      |                 |
| (pointer warp)  |                      |                 |
+-----------------+                      +-----------------+
```

### Linux Side (Rust)

1. **Edge detection**: Create a 1px-wide layer-shell surface on the configured
   edge of the configured monitor. When the pointer enters this surface, capture
   begins. The wayland-protocols-wlr crate provides `zwlr_layer_shell_v1`.

2. **Input capture (hybrid approach)**: Use the layer-shell surface for mouse
   edge detection (triggering capture), but use **evdev** (`/dev/input/`) for
   keyboard capture during active sharing. evdev is more reliable than relying
   on the layer-shell surface to forward keyboard events — it reads directly
   from the kernel input subsystem. Grab the keyboard device exclusively
   (`EVIOCGRAB`) only while sharing is active, release the grab on return.

3. **Transport**: Send events over **TCP** (not UDP). This guarantees delivery —
   no stuck keys from dropped packets. The latency overhead of TCP vs UDP for
   HID events is negligible on a LAN (sub-millisecond). Use a simple
   length-prefixed binary protocol — no serialization library needed.

4. **Return detection**: When the Mac signals "mouse hit the opposite edge",
   release the evdev grab, release capture, and warp the pointer back onto the
   configured monitor.

5. **Modifier sync on transition**: When entering capture, send explicit
   key-down events for any modifiers currently held (shift, ctrl, alt, super)
   so the Mac starts with correct modifier state. When returning, send explicit
   key-up events for all modifiers before releasing the grab.

### Mac Side (Rust)

The Mac side is also written in Rust. lan-mouse demonstrates that the
`core-graphics` and `core-foundation` crates provide full access to macOS
CGEvent APIs from Rust, with no need for Swift.

1. **Input injection**: Use `CGEventPost` with `CGEventTapLocation::HID` to
   inject keyboard and mouse events. Requires Accessibility permission (granted
   once via System Settings > Privacy & Security > Accessibility).

2. **Edge detection**: After injecting mouse motion, check if the cursor
   position hit the configured return edge. If so, send a "return" message
   over TCP.

3. **Key release safety**: On disconnect, TCP error, or "return" signal,
   immediately release ALL currently held keys. Track held-key state locally
   so this works even if the connection drops mid-keystroke.

### Key Design Decisions

- **TCP not UDP**: Eliminates the entire stuck key category. TCP handles
  retransmission, ordering, and delivery guarantees. For HID events on a LAN,
  the latency difference is imperceptible.

- **Length-prefixed framing**: Each message is `[u16 length][payload]`. No
  need for protobuf, msgpack, or any serialization library. The event types
  are simple fixed-size structs.

- **No encryption initially**: Both machines are on the same trusted LAN.
  Can add TLS later if needed — standard `rustls` on both sides. DTLS was
  the source of major cross-platform pain in lan-mouse.

- **No peer-to-peer**: The Linux machine is always the sender, the Mac is
  always the receiver. One direction, one connection, simple state machine.

- **No clipboard sync initially**: Focus on mouse/keyboard first. Clipboard
  can be added later via a separate TCP message type.

- **Keymap translation**: Linux uses evdev scancodes, macOS uses HID usage IDs.
  The `keycode` crate provides this mapping.

- **Raw mouse deltas**: Send raw relative mouse motion from evdev. macOS
  applies its own acceleration curve via `CGEventCreateMouseEvent` with
  `kCGMouseEventDeltaX`/`kCGMouseEventDeltaY`. Sending pre-accelerated
  values would double-accelerate. Let each OS handle its own acceleration.

### Reconnection & Resilience

TCP connections will drop if either machine sleeps, the network blips, or a
process restarts. The design must handle this gracefully:

- **Auto-reconnect**: The sender retries with exponential backoff (1s, 2s, 4s,
  cap at 30s). While disconnected, input stays on the Linux host.
- **Release all keys on disconnect**: Both sides must immediately release all
  held keys when the TCP connection drops. This is the single most important
  safety behavior — it prevents the stuck-key-on-disconnect scenario that
  plagues every existing tool.
- **Heartbeat**: Send a periodic heartbeat to detect dead connections faster
  than TCP keepalive defaults. Use an **adaptive interval**: 1s while capture
  is active (typing/mousing on the Mac), 5s while idle. If 3 heartbeats are
  missed, treat the connection as dead. This keeps worst-case detection at
  3s during active use (vs 15s with a fixed 5s interval), which matters when
  a stuck connection would leave the user typing into the void.
- **Graceful shutdown**: On SIGTERM/SIGINT, release all keys before closing
  the connection.

### Dependencies (Linux side)

- `wayland-client`, `wayland-protocols-wlr` (layer-shell)
- `tokio` (async TCP, timers)
- `evdev` crate (raw input capture + exclusive grab)
- Hyprland IPC via Unix socket (pointer warp, monitor geometry)

### Dependencies (Mac side)

- `core-graphics` crate (CGEvent APIs)
- `core-foundation` crate (CFRunLoop)
- `tokio` (async TCP, timers)
- `keycode` crate (evdev to macOS keycode mapping)

### Resolved Design Questions

- **evdev vs layer-shell for keyboard**: Hybrid approach — layer-shell for
  mouse edge trigger, evdev with exclusive grab for keyboard during active
  capture. Grab only while sharing, release on return.
- **Modifier state across transitions**: Explicit sync — send modifier
  key-down events on enter, key-up events on return.
- **Mouse acceleration**: Send raw deltas, let macOS apply its own curve.
- **Multi-monitor edge**: Configurable via config file (monitor name + side).

### Future Enhancements

- Clipboard sync (separate message type over same TCP connection)
- TLS encryption (rustls on both sides)
- Bidirectional support (Mac to Linux sharing)
- Scroll wheel + trackpad gesture forwarding
- systemd service for auto-start on Linux
- launchd agent for auto-start on Mac

## Alternate Paths

1. **Hyprland merges InputCapture**: Input Leap would work out of the box. Check
   [Hyprland PR #7919](https://github.com/hyprwm/Hyprland/pull/7919) and
   [xdg-desktop-portal-hyprland PR #268](https://github.com/hyprwm/xdg-desktop-portal-hyprland/pull/268)
2. **lan-mouse v0.11+**: May fix stuck key and DTLS issues
