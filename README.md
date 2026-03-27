# Styx

A software KVM for sharing a keyboard and mouse from a Hyprland Linux machine to a Mac over the network. Move the cursor off a screen edge to control the Mac; move it back to return.

## Why

There are many keyboard/mouse sharing tools -- Synergy, Barrier, Input Leap, Deskflow, lan-mouse. None of them work reliably on Hyprland.

The Synergy family (Input Leap, Deskflow, Barrier) relies on the `org.freedesktop.portal.InputCapture` XDG portal to detect when the mouse hits a screen edge on Wayland. Hyprland does not implement this portal. KDE Plasma and GNOME do, but Hyprland's portal backend only supports Screenshot, ScreenCast, and GlobalShortcuts. There are open PRs to add InputCapture support ([Hyprland #7919](https://github.com/hyprwm/Hyprland/pull/7919), [xdg-desktop-portal-hyprland #268](https://github.com/hyprwm/xdg-desktop-portal-hyprland/pull/268)), but they have not merged.

lan-mouse is the one tool that works around this limitation. It uses the wlr-layer-shell protocol to create invisible surfaces at screen edges, which does not require the InputCapture portal. However, lan-mouse has reliability problems that make it unusable for daily use:

- **Stuck keys.** The UDP transport drops key-release events with no retransmission. Keys repeat infinitely on the receiving machine. This is not packet loss -- the layer-shell capture layer itself occasionally fails to generate key-up events, and UDP has no mechanism to recover.
- **DTLS handshake failures.** The encryption layer fails asymmetrically between Linux and macOS. One direction works, the other does not.
- **Peer-to-peer complexity.** DNS resolution pulls in VPN addresses, dual NICs route UDP responses out the wrong interface, and the configuration format changes between versions.

Styx keeps what works from lan-mouse (layer-shell edge detection) and replaces everything that does not.

## How It Works

Styx has two binaries: a **sender** that runs on the Linux machine and a **receiver** that runs on the Mac.

The sender creates a 1-pixel invisible surface on the configured screen edge using the wlr-layer-shell Wayland protocol. When the cursor enters this surface, the sender grabs the keyboard via evdev (directly from the kernel input subsystem, bypassing the compositor) and begins forwarding mouse and keyboard events to the receiver over TCP.

The receiver injects these events into macOS using the Core Graphics accessibility APIs. When the cursor hits the opposite edge of the Mac's display, the receiver signals the sender to release the grab and warp the cursor back to the Linux machine.

TCP eliminates the stuck key problem entirely. Delivery is guaranteed by the kernel -- no dropped events, no acknowledgment protocol needed. For HID events on a LAN, the latency difference between TCP and UDP is imperceptible.

### Architecture

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

## Building

Styx is written entirely in Rust. The sender builds on Linux; the receiver builds on macOS. The shared crates (protocol and keymap) are platform-agnostic.

### Sender (Linux)

Requires `libwayland-dev`, `wayland-protocols`, and `libevdev-dev`.

```
cargo build --release -p styx-sender
```

### Receiver (macOS)

```
cargo build --release -p styx-receiver
```

The receiver requires Accessibility permission on macOS. On first run, grant access in System Settings > Privacy & Security > Accessibility.

## Configuration

Create `~/.config/styx/config.toml` on each machine. An example is included at `dist/config.toml.example`.

**Sender (Linux):**

```toml
[sender]
receiver_host = "192.168.1.100"
receiver_port = 4242
monitor = "DP-1"
edge = "left"
keyboard_device = "/dev/input/by-id/usb-Example_Keyboard-event-kbd"
```

- `monitor`: the Hyprland output name (from `hyprctl monitors`) where the edge surface is created.
- `edge`: which side of the monitor triggers capture (`left`, `right`, `top`, `bottom`).
- `keyboard_device`: the evdev device path for the keyboard. List candidates with `ls /dev/input/by-id/ | grep kbd`.

**Receiver (macOS):**

```toml
[receiver]
listen_host = "0.0.0.0"
listen_port = 4242
```

## Installation

### Arch Linux / pacman

A PKGBUILD is provided in `dist/`. Copy it to a build directory and run `makepkg -si`.

A systemd user service is included:

```
systemctl --user enable --now styx-sender
```

### macOS

Build from source or install via Homebrew with the formula in `dist/homebrew/`.

A launchd plist is provided at `dist/styx-receiver.plist`:

```
cp dist/styx-receiver.plist ~/Library/LaunchAgents/com.ghreprimand.styx-receiver.plist
launchctl load ~/Library/LaunchAgents/com.ghreprimand.styx-receiver.plist
```

### From source (any platform)

```
cargo install --git https://github.com/ghreprimand/styx styx-sender   # Linux
cargo install --git https://github.com/ghreprimand/styx styx-receiver  # macOS
```

### Pre-built binaries

Release binaries for Linux (x86_64) and macOS (ARM64, x86_64) are published on the [GitHub Releases](https://github.com/ghreprimand/styx/releases) page.

## Design Decisions

- **TCP, not UDP.** Eliminates stuck keys. TCP guarantees delivery and ordering. The sub-millisecond latency penalty on a LAN is imperceptible for HID events.
- **evdev for keyboard, layer-shell for mouse.** The layer-shell surface detects when the cursor hits the edge (mouse only). Keyboard capture uses evdev with an exclusive grab, which reads directly from the kernel and avoids the event delivery issues in lan-mouse's layer-shell keyboard forwarding.
- **No encryption by default.** Both machines are on the same trusted network. TLS can be added later without the cross-platform pain of DTLS.
- **Unidirectional.** The Linux machine is always the sender, the Mac is always the receiver. One connection, one direction, minimal state.
- **Adaptive heartbeat.** 1-second interval during active capture, 5-second interval when idle. Three missed heartbeats trigger disconnect and key release. Worst-case detection during active use is 3 seconds.
- **Release all keys on disconnect.** Both sides track held keys and release everything immediately when the connection drops. This is the single most important safety behavior.

## Project Structure

```
styx-proto/      Wire protocol: event types, binary encoding, TCP framing
styx-keymap/     evdev to macOS keycode translation
styx-sender/     Linux binary: layer-shell capture, evdev grab, Hyprland IPC
styx-receiver/   macOS binary: CGEvent injection, edge detection, TCP server
dist/            PKGBUILD, systemd service, launchd plist, Homebrew formula
```

## License

MIT
