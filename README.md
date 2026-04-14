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

The receiver tracks per-button click counts within a 500 ms interval so double- and triple-clicks register correctly on macOS. It also declares user activity on every injected event, so a Mac whose external display has gone to sleep via idle timeout wakes on the first crossover input -- synthesized `CGEvent` posts alone do not wake a slept display.

TCP eliminates the stuck key problem entirely. Delivery is guaranteed by the kernel -- no dropped events, no acknowledgment protocol needed. For HID events on a LAN, the latency difference between TCP and UDP is imperceptible.

Text clipboard is synced automatically on each transition. When the cursor crosses from Linux to Mac, the Linux clipboard is pushed to macOS. When it returns, the macOS clipboard is pushed back. Copy on either machine, paste on either machine. Requires `wl-clipboard` (`wl-paste`/`wl-copy`) on the Linux side; macOS uses the built-in `pbcopy`/`pbpaste`.

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

### Cursor Position Mapping

When the cursor crosses between machines, styx maps the vertical position proportionally based on pixel distance from the bottom of each monitor. Both monitors are treated as bottom-aligned:

- Cursor at the **bottom** of either monitor crosses to the **bottom** of the other.
- Cursor at the **top** of a shorter monitor crosses to the proportional height on the taller one.

For example, if the Mac display is 956 logical points tall and the Linux monitor is 1920 pixels tall (portrait), the Mac's full height maps to the bottom half of the Linux monitor. Crossing at the top of the Mac places the cursor roughly halfway up the Linux monitor.

After the first successful round-trip, the sender learns the receiver's screen height and blocks crossover above that height on the Linux monitor. This prevents the cursor from crossing into a region that has no corresponding position on the Mac.

Portrait (rotated) monitors are handled automatically -- styx accounts for Hyprland's monitor transform when computing cursor positions. Scaled (HiDPI) Linux monitors are also handled automatically -- all cursor math uses logical coordinates, consistent with Hyprland's `scale` setting.

## Requirements

**Sender (Linux):**
- Hyprland compositor with wlr-layer-shell support
- Wayland development libraries (`libwayland-dev`, `wayland-protocols`)
- evdev development library (`libevdev-dev`)
- Rust toolchain
- User must be in the `input` group for evdev access (`sudo usermod -aG input $USER`, requires re-login)
- `wl-clipboard` (`wl-paste`, `wl-copy`) for clipboard sync

**Receiver (macOS):**
- macOS Ventura (13.0) or later
- Rust toolchain
- Accessibility permission granted to the receiver app bundle

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
# or, if the Mac has multiple IPs (ethernet + wifi):
# receiver_hosts = ["192.168.1.100", "192.168.1.101"]
receiver_port = 4242
monitor = "DP-1"
edge = "left"
```

| Option | Description |
|--------|-------------|
| `receiver_host` | IP address of the Mac on the local network |
| `receiver_hosts` | (optional) list of IP addresses to try in order, e.g. `["192.168.1.100", "192.168.1.101"]`. Use this when the Mac has multiple network interfaces (ethernet + wifi). At least one of `receiver_host` or `receiver_hosts` must be set. |
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
# swap_alt_cmd = true
```

| Option | Description |
|--------|-------------|
| `listen_host` | Address to bind (use `0.0.0.0` for all interfaces) |
| `listen_port` | TCP port to listen on (default: 4242) |
| `return_edge` | Which display edge faces the Linux machine: `left`, `right`, `top`, `bottom` (default: `right`) |
| `swap_alt_cmd` | (optional) Swap Alt and Super so physical key positions match the macOS Control/Option/Command layout (default: `false`) |

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

### Linux (Arch Linux)

A PKGBUILD and systemd user service are provided in `dist/`.

```
systemctl --user enable --now styx-sender
```

### macOS

The recommended installation method is the install script, which builds the receiver, creates a signed `.app` bundle, and configures launchd for autostart:

```
./dist/macos/install.sh
```

The script will:
1. Build `styx-receiver` in release mode.
2. Create `/Applications/Styx Receiver.app` with the binary and metadata.
3. Sign the app with a `styx-cert` code signing certificate (falls back to ad-hoc if not found).
4. Install a launchd agent that starts the receiver on login and restarts on failure.

After installation, grant Accessibility permission:
1. Open **System Settings > Privacy & Security > Accessibility**.
2. Click the `+` button and add `/Applications/Styx Receiver.app`.
3. Enable the toggle.

The receiver will start automatically on login. Logs are at `/tmp/styx-receiver.stderr.log`.

#### Code Signing Certificate

For Accessibility permission to persist across rebuilds, create a self-signed code signing certificate:

1. Open **Keychain Access**.
2. Go to **Keychain Access > Certificate Assistant > Create a Certificate**.
3. Name: `styx-cert`, Identity Type: **Self Signed Root**, Certificate Type: **Code Signing**.
4. Create the certificate.

Without `styx-cert`, the install script falls back to ad-hoc signing. Ad-hoc signatures change on every build, so you may need to re-grant Accessibility permission after each rebuild.

### From Source

```
cargo install --git https://github.com/ghreprimand/styx styx-sender   # Linux
cargo install --git https://github.com/ghreprimand/styx styx-receiver  # macOS
```

**Pre-built binaries** for Linux (x86_64) and macOS (ARM64, x86_64) are published on the [Releases](https://github.com/ghreprimand/styx/releases) page.

## Troubleshooting

**Cursor doesn't appear on Mac after crossing:**
- Check that Accessibility permission is granted to `Styx Receiver.app` (not to a terminal or bare binary).
- Check logs: `tail -f /tmp/styx-receiver.stderr.log`. The line `accessibility: granted` should appear at startup. If it says `NOT GRANTED`, re-add the app in System Settings.
- If you rebuilt the receiver, you may need to remove and re-add the Accessibility entry (especially with ad-hoc signing).

**Receiver doesn't start on login:**
- Verify the launchd agent is loaded: `launchctl print gui/$(id -u)/com.ghreprimand.styx-receiver`
- Re-run `./dist/macos/install.sh` to reinstall.

**Connection drops repeatedly:**
- Both sides must be on the same protocol version. Rebuild and restart both sender and receiver after pulling updates.
- Check for duplicate sender instances: `pgrep -c styx-sender` should return 1.

**Cursor position is wrong on portrait monitors:**
- Styx accounts for Hyprland monitor transforms (90/270 rotation). If positions are still wrong, check that `hyprctl -j monitors` shows the correct `transform` value for your portrait monitor.

**Keys trigger wrong shortcuts on Mac:**
- By default, Linux Left Alt maps to macOS Option and Linux Super maps to macOS Command. Set `swap_alt_cmd = true` in the receiver config to swap these so the physical key positions match the standard macOS layout (Super becomes Option, Alt becomes Command).

## Design Decisions

- **TCP, not UDP.** Eliminates stuck keys. TCP guarantees delivery and ordering. The sub-millisecond latency penalty on a LAN is imperceptible for HID events.
- **evdev for keyboard, layer-shell for mouse.** The layer-shell surface detects when the cursor hits the edge. Keyboard capture uses evdev with an exclusive grab, which reads directly from the kernel and avoids the event delivery issues in lan-mouse's layer-shell keyboard forwarding.
- **No encryption by default.** Both machines are on the same trusted network. TLS can be added later without the cross-platform pain of DTLS.
- **Unidirectional.** The Linux machine is always the sender, the Mac is always the receiver. One connection, one direction, minimal state.
- **Adaptive heartbeat.** 1-second interval during active capture, 5-second interval when idle. Three missed heartbeats trigger disconnect and key release. Worst-case detection during active use is 3 seconds.
- **Release all keys on disconnect.** Both sides track held keys and release everything immediately when the connection drops.
- **Bottom-aligned cursor mapping.** Monitors of different heights are treated as bottom-aligned. The wire protocol sends the pixel distance from the bottom and the source monitor's height, so each side can map proportionally without needing to know the other's resolution in advance.

## Scope and Limitations

- Hyprland only. The sender depends on wlr-layer-shell and Hyprland's IPC socket. Other Wayland compositors that support wlr-layer-shell may work but are untested.
- macOS only on the receiving end. The receiver uses Core Graphics APIs that are macOS-specific.
- Linux to Mac only. There is no Mac-to-Linux direction.
- No encryption. Use on a trusted network or behind a VPN.
- Single sender, single receiver. No multi-machine mesh.

## Project Structure

```
styx-proto/      Wire protocol: event types, binary encoding, TCP framing
styx-keymap/     evdev to macOS keycode translation
styx-sender/     Linux binary: layer-shell capture, evdev grab, Hyprland IPC
styx-receiver/   macOS binary: CGEvent injection, edge detection, TCP server
dist/            PKGBUILD, systemd service, launchd plist, install scripts
```

## Acknowledgments

Styx's layer-shell edge detection approach is inspired by [lan-mouse](https://github.com/feschber/lan-mouse) by Ferdinand Schober. lan-mouse demonstrated that wlr-layer-shell surfaces can be used for input capture on Wayland compositors that lack the InputCapture portal, and its approach to this problem made styx possible. lan-mouse is a more capable and general-purpose tool -- if your compositor supports InputCapture or you need cross-platform/multi-directional sharing, use it instead.

## License

MIT
