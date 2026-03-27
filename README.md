# Styx

A purpose-built software KVM for sharing a keyboard and mouse from a [Hyprland](https://hyprland.org/) Linux machine to a Mac over the local network.

Styx is narrow in scope by design. It does one thing: send keyboard and mouse input from a Hyprland Wayland compositor to macOS, with seamless edge-based transitions. It is not a general-purpose KVM, does not support Windows, does not support arbitrary Wayland compositors, and is unidirectional (Linux to Mac only). If you need broader compatibility, use [Input Leap](https://github.com/input-leap/input-leap), [Deskflow](https://github.com/deskflow/deskflow), or [lan-mouse](https://github.com/feschber/lan-mouse).

## Why This Exists

There are many keyboard/mouse sharing tools. None of them work reliably on Hyprland.

The Synergy family (Input Leap, Deskflow, Barrier) relies on the `org.freedesktop.portal.InputCapture` XDG portal to detect when the mouse hits a screen edge on Wayland. **Hyprland does not implement this portal.** KDE Plasma and GNOME do, but Hyprland's portal backend only supports Screenshot, ScreenCast, and GlobalShortcuts. There are open PRs to add InputCapture support ([hyprwm/Hyprland#7919](https://github.com/hyprwm/Hyprland/pull/7919), [hyprwm/xdg-desktop-portal-hyprland#268](https://github.com/hyprwm/xdg-desktop-portal-hyprland/pull/268)), but as of March 2026 they have not merged. When they do, Input Leap will work on Hyprland out of the box and styx will no longer be necessary.

[lan-mouse](https://github.com/feschber/lan-mouse) is the one tool that works around this limitation. It uses the wlr-layer-shell protocol to create invisible surfaces at screen edges, bypassing the need for the InputCapture portal entirely. However, in practice it has reliability issues that prevent daily use:

- **Stuck keys.** The UDP transport has no delivery guarantees. Key-release events get dropped, causing keys to repeat indefinitely on the receiving machine. This is not network packet loss -- the layer-shell capture layer itself occasionally fails to generate key-up events, and UDP provides no mechanism to recover.
- **DTLS handshake failures.** The encryption layer fails asymmetrically between Linux and macOS. One direction works, the other does not.
- **Peer-to-peer complexity.** DNS resolution pulls in VPN addresses, dual NICs route UDP responses out the wrong interface, and the configuration format changes between versions.

Styx takes the one thing that works on Hyprland -- layer-shell edge detection -- and replaces everything else with a simpler, reliable stack: TCP transport, evdev keyboard capture, and a unidirectional architecture.

## How It Works

Styx has two binaries: a **sender** that runs on the Linux machine and a **receiver** that runs on the Mac. Both machines must be on the same local network.

The sender creates a 1-pixel invisible surface on the configured screen edge using the wlr-layer-shell Wayland protocol. When the cursor enters this surface, the sender grabs the keyboard via evdev (directly from the kernel input subsystem, bypassing the compositor) and begins forwarding mouse and keyboard events to the receiver over TCP.

The receiver injects these events into macOS using the Core Graphics accessibility APIs. When the cursor hits the configured return edge of the Mac's display, the receiver signals the sender to release the grab and warp the cursor back to the Linux machine.

TCP eliminates the stuck key problem entirely. Delivery is guaranteed by the kernel -- no dropped events, no acknowledgment protocol needed. For HID events on a LAN, the latency difference between TCP and UDP is imperceptible.

```
Linux (sender)                            Mac (receiver)
+-----------------+                      +-----------------+
| layer-shell     |   TCP connection     | macOS           |
| edge detection  | ------------------> | CGEvent         |
| (mouse only)    |   key/mouse events  | injection       |
|                 |                      |                 |
| evdev capture   |                      |                 |
| (keyboard grab) | <------------------ | edge detection  |
|                 |   return signal     | (position check)|
| Hyprland IPC    |                      |                 |
| (pointer warp)  |                      |                 |
+-----------------+                      +-----------------+
```

## Requirements

**Sender (Linux):**
- Hyprland compositor with wlr-layer-shell support
- Wayland development libraries (`libwayland-dev`, `wayland-protocols`)
- evdev development library (`libevdev-dev`)
- Rust toolchain
- User must be in the `input` group for evdev access (`sudo usermod -aG input $USER`, requires re-login)

**Receiver (macOS):**
- macOS with Accessibility permission granted to the receiver binary (System Settings > Privacy & Security > Accessibility)
- Rust toolchain

## Building

```
cargo build --release -p styx-sender    # on Linux
cargo build --release -p styx-receiver  # on macOS
```

## Configuration

Create `~/.config/styx/config.toml` on each machine. A full example is at `dist/config.toml.example`.

**Sender (Linux):**

```toml
[sender]
receiver_host = "192.168.1.100"
receiver_port = 4242
monitor = "DP-1"
edge = "left"
```

| Option | Description |
|--------|-------------|
| `receiver_host` | IP address of the Mac on the local network |
| `receiver_port` | TCP port the receiver is listening on (default: 4242) |
| `monitor` | Hyprland output name where the edge surface is placed (from `hyprctl monitors`) |
| `edge` | Which side of the monitor triggers capture: `left`, `right`, `top`, `bottom` |
| `keyboard_device` | (optional) evdev device path. If omitted, auto-detects the first keyboard in `/dev/input/by-id/` |

**Receiver (macOS):**

```toml
[receiver]
listen_host = "0.0.0.0"
listen_port = 4242
return_edge = "right"
```

| Option | Description |
|--------|-------------|
| `listen_host` | Address to bind (use `0.0.0.0` for all interfaces) |
| `listen_port` | TCP port to listen on (default: 4242) |
| `return_edge` | Which display edge faces the Linux machine: `left`, `right`, `top`, `bottom` (default: `right`) |

## Running

Start the receiver first, then the sender:

```
# On Mac:
RUST_LOG=info ./styx-receiver

# On Linux:
RUST_LOG=info ./styx-sender
```

Move the cursor to the configured edge of the Linux monitor to begin controlling the Mac. Move it to the return edge on the Mac to switch back.

## Installation

**Arch Linux / pacman:** A PKGBUILD and systemd user service are provided in `dist/`.

```
systemctl --user enable --now styx-sender
```

**macOS:** Build from source or use the Homebrew formula in `dist/homebrew/`. A launchd plist is provided:

```
cp dist/styx-receiver.plist ~/Library/LaunchAgents/com.ghreprimand.styx-receiver.plist
launchctl load ~/Library/LaunchAgents/com.ghreprimand.styx-receiver.plist
```

**From source:**

```
cargo install --git https://github.com/ghreprimand/styx styx-sender   # Linux
cargo install --git https://github.com/ghreprimand/styx styx-receiver  # macOS
```

**Pre-built binaries** for Linux (x86_64) and macOS (ARM64, x86_64) are published on the [Releases](https://github.com/ghreprimand/styx/releases) page.

## Design Decisions

- **TCP, not UDP.** Eliminates stuck keys. TCP guarantees delivery and ordering. The sub-millisecond latency penalty on a LAN is imperceptible for HID events.
- **evdev for keyboard, layer-shell for mouse.** The layer-shell surface detects when the cursor hits the edge. Keyboard capture uses evdev with an exclusive grab, which reads directly from the kernel and avoids the event delivery issues in lan-mouse's layer-shell keyboard forwarding.
- **No encryption by default.** Both machines are on the same trusted network. TLS can be added later without the cross-platform pain of DTLS.
- **Unidirectional.** The Linux machine is always the sender, the Mac is always the receiver. One connection, one direction, minimal state.
- **Adaptive heartbeat.** 1-second interval during active capture, 5-second interval when idle. Three missed heartbeats trigger disconnect and key release. Worst-case detection during active use is 3 seconds.
- **Release all keys on disconnect.** Both sides track held keys and release everything immediately when the connection drops.

## Scope and Limitations

- Hyprland only. The sender depends on wlr-layer-shell and Hyprland's IPC socket. Other Wayland compositors that support wlr-layer-shell may work but are untested.
- macOS only on the receiving end. The receiver uses Core Graphics APIs that are macOS-specific.
- Linux to Mac only. There is no Mac-to-Linux direction.
- No clipboard sharing. Keyboard and mouse events only.
- No encryption. Use on a trusted network or behind a VPN.
- Single sender, single receiver. No multi-machine mesh.

## Project Structure

```
styx-proto/      Wire protocol: event types, binary encoding, TCP framing
styx-keymap/     evdev to macOS keycode translation
styx-sender/     Linux binary: layer-shell capture, evdev grab, Hyprland IPC
styx-receiver/   macOS binary: CGEvent injection, edge detection, TCP server
dist/            PKGBUILD, systemd service, launchd plist, Homebrew formula
```

## Acknowledgments

Styx's layer-shell edge detection approach is inspired by [lan-mouse](https://github.com/feschber/lan-mouse) by Ferdinand Schober. lan-mouse demonstrated that wlr-layer-shell surfaces can be used for input capture on Wayland compositors that lack the InputCapture portal, and its approach to this problem made styx possible. lan-mouse is a more capable and general-purpose tool -- if your compositor supports InputCapture or you need cross-platform/multi-directional sharing, use it instead.

## License

MIT
