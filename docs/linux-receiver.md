# Linux Receiver

> **Branch-only feature.** The Linux receiver lives on `feat/hybrid-relay`. It
> is not on `main`, where `styx-receiver` is macOS-only. See
> [relay-topology.md](relay-topology.md) for the three-node design this is the
> first step toward.

Styx on `main` sends input one way: Hyprland Linux to macOS. This branch adds a
**Linux injection backend** to `styx-receiver`, so a second Linux machine can
be the receiving end of an ordinary styx link.

That alone is useful — a Linux workstation can now drive a Linux laptop with no
Mac involved. It is also the prerequisite for the relay work: a chain cannot
reach the laptop until the laptop can receive.

## What changed

`styx-receiver` builds on both platforms now. Shared logic stayed shared; only
the two genuinely platform-bound pieces were split.

| Module | macOS | Linux |
|--------|-------|-------|
| `geometry.rs` | shared | shared |
| `transport.rs`, `edge.rs` | shared | shared |
| injection | `inject.rs` (CoreGraphics `CGEvent`) | `inject_linux.rs` (`/dev/uinput`) |
| clipboard | `clipboard.rs` + `clipboard_image.rs` (`pbcopy`, `NSPasteboard`) | `clipboard_linux.rs` (`wl-copy`/`wl-paste`) |

Cargo resolves the Apple framework crates only under
`cfg(target_os = "macos")` and `evdev` only under `cfg(target_os = "linux")`,
so neither platform pulls the other's dependencies.

**All cursor math is shared.** `geometry.rs` holds the union clamping, the
per-display edge resolution, and the crossover placement that the macOS backend
already used — extracted unchanged, not reimplemented. The hard-won fixes from
0.5.4 and 0.5.5 (the set-back stacked-monitor dead zone, the phantom-band Dock
bug) therefore apply to the Linux backend for free.

A side benefit: that logic used to be testable only on a Mac. It now has 20+
unit tests that run on Linux.

## How injection works

The macOS backend posts `CGEvent`s at absolute screen coordinates. Wayland has
no equivalent "put the cursor at this position" API that works without
compositor cooperation, so the Linux backend takes a different route: it
creates a virtual **absolute** pointing device via `/dev/uinput` — one that
reports positions rather than deltas, like the tablet devices QEMU and
VirtualBox expose to guests.

Two consequences worth understanding:

- **Compositor-agnostic.** No Hyprland IPC, no wlr protocols, no portal. The
  receiver works on Hyprland, sway, GNOME, KDE, and X11 alike. Only the
  *sender* needs Hyprland, because capture is the hard part on Wayland — not
  injection.
- **Precise crossover placement.** Relative motion cannot put the cursor at a
  specific spot on entry; absolute positioning can, which is what makes the
  proportional edge mapping work identically to the Mac.

Two devices are created rather than one — a pointer and a keyboard. A single
device carrying both `ABS_X`/`ABS_Y` and the full key range invites
misclassification by libinput; splitting them matches how real hardware
presents itself.

Keycodes need **no translation**. The wire protocol already carries evdev
scancodes, and this is an evdev device. `styx-keymap`'s macOS conversion exists
only to reach macOS virtual keycodes; Linux-to-Linux is the identity mapping.

## Setup

### 1. uinput access

The receiver needs write access to `/dev/uinput`.

```
sudo modprobe uinput
```

Persist the module across reboots:

```
echo uinput | sudo tee /etc/modules-load.d/uinput.conf
```

Grant your user access with a udev rule rather than running as root:

```
echo 'KERNEL=="uinput", GROUP="input", MODE="0660", OPTIONS+="static_node=uinput"' \
  | sudo tee /etc/udev/rules.d/99-styx-uinput.rules
sudo udevadm control --reload-rules && sudo udevadm trigger
sudo usermod -aG input $USER
```

Log out and back in for the group change to take effect. The receiver reports
permission and missing-module failures with actionable messages, so a bad setup
is loud rather than silent.

### 2. wl-clipboard

Install `wl-clipboard` for clipboard sync (`wl-paste`, `wl-copy`). Without it
the receiver still handles input; it logs a warning and disables clipboard.

### 3. Config

The Linux backend needs one thing macOS does not: **the display layout**.
CoreGraphics enumerates displays for free, but the Linux receiver holds no
display-server connection, so the layout must be declared.

```toml
[receiver]
listen_hosts = ["192.168.1.50"]
allowed_senders = ["192.168.1.10"]
listen_port = 4242
return_edge = "right"

# One entry per display, in the compositor's logical layout coordinates.
[[receiver.display]]
x = 0
y = 0
width = 1920
height = 1080
```

Values come straight from `hyprctl monitors -j` — `x`, `y`, and the
scale-adjusted `width`/`height`. For a scaled display, divide the native
dimensions by `scale`: a 3840x2160 panel at `scale = 1.5` is `2560` x `1440`.

**The declared extent must match the compositor's real layout.** The virtual
pointer's absolute axes are mapped onto it, so a mismatch puts the cursor in
the wrong place — proportionally offset, which looks like drift rather than an
obvious error. The receiver logs the computed extent at startup; check it
against `hyprctl monitors` if the cursor lands oddly.

On Hyprland, generate the blocks rather than doing the arithmetic by hand.
This handles both scaling and rotation (90°/270° transforms swap width and
height):

```
hyprctl monitors -j | jq -r '.[] |
  (if .transform == 1 or .transform == 3
   then {w: .height, h: .width}
   else {w: .width,  h: .height} end) as $d |
  "[[receiver.display]]\nx = \(.x)\ny = \(.y)\nwidth = \(($d.w / .scale) | round)\nheight = \(($d.h / .scale) | round)\n"'
```

On other compositors, `wlr-randr` (wlroots) or `xrandr` (X11) report the same
logical geometry; transcribe position and size the same way.

`swap_alt_cmd` is accepted but ignored, with a warning. It exists to reconcile
PC and Mac modifier positions; both ends of a Linux-to-Linux link use the same
evdev codes.

### 4. Sender

No sender changes. Point `receiver_host` at the Linux receiver and set `edge`
to the side facing it — the sender cannot tell what OS is on the other end.

## Testing it

Run the receiver in the foreground first:

```
RUST_LOG=info cargo run --release -p styx-receiver
```

Expect `uinput devices created` plus the computed desktop extent. Then start
the sender on the workstation and cross the configured edge.

Confirm the virtual devices registered with the kernel:

```
grep -i styx /proc/bus/input/devices
```

Expect `styx-receiver pointer` and `styx-receiver keyboard`; both disappear
when the process exits. (`/dev/input/by-id/` is not useful here — it carries
symlinks for physical USB devices, not virtual uinput ones. On a machine also
running the sender you will additionally see `styx-synth`, which is the
sender's own device and unrelated.)

### Logging

Logs go to stderr, so `journalctl --user -u styx-receiver` works normally under
systemd and foreground runs print to the terminal. The macOS build redirects
stderr to `/tmp/styx-receiver.stderr.log` and self-truncates it, because launchd
pre-opens that descriptor and offers no rotation; that behaviour is gated to
macOS and does not apply here.

## Limitations

- **Display layout is static.** Hotplugging a monitor or changing resolution
  requires editing the config and restarting the receiver. macOS re-enumerates
  every 30 seconds; the Linux backend has nothing to re-enumerate.
- **HTML clipboard degrades to plain text.** `wl-copy` accepts one MIME type
  per invocation, the same asymmetry documented in
  [clipboard-sync.md](clipboard-sync.md) for the Mac-to-Linux direction.
- **Clipboard change detection is polled, not evented.** macOS has
  `NSPasteboard.changeCount`, a free monotonic counter. Wayland has no
  equivalent, so the backend hashes the clipboard's offered types and text
  content, throttled to 2 Hz to avoid spawning `wl-paste` twenty times a
  second. Cost: up to ~500 ms before a local copy propagates — invisible next
  to the time it takes to move the cursor.
- **Not yet a relay.** This is the terminal end of a link. Forwarding onward,
  and the Mac's relay/origin modes, are later steps in
  [relay-topology.md](relay-topology.md).

## Security

The Linux receiver has the same exposure as the macOS one and the same
controls: `listen_hosts` to avoid binding on untrusted networks,
`allowed_senders` to reject unknown peers at accept time. Both are strongly
recommended on a laptop that travels — see [security.md](security.md).

One change worth stating plainly: a machine running this receiver **accepts
injected input from the network**. Anyone who can connect to the port without
being filtered can type on it. That was already true of the Mac receiver, but
it is newly true of a Linux box that previously only ever sent.
