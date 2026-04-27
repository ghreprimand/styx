# Styx Sender GUI

`styx-sender-gui` is a small GTK settings utility for the Linux sender. It edits the sender section of `~/.config/styx/config.toml` and can restart the `styx-sender.service` user service after saving.

The GUI is a convenience wrapper around the existing config file. `styx-sender` remains the runtime service.

## Requirements

- Linux with Hyprland
- Python 3.11 or newer
- GTK 4
- PyGObject
- `hyprctl` available in the user session
- `systemctl --user` if using the restart buttons

On Arch Linux, the GUI dependencies are:

```
pacman -S gtk4 python-gobject
```

## Installation

The Arch package installs:

- `/usr/bin/styx-sender-gui`
- `/usr/share/applications/styx-sender-gui.desktop`

For a source checkout without packaging, install it for the current user:

```
install -Dm755 tools/styx-sender-gui "$HOME/.local/bin/styx-sender-gui"
install -Dm644 dist/styx-sender-gui.desktop "$HOME/.local/share/applications/styx-sender-gui.desktop"
```

If the desktop entry was installed from a source checkout and your launcher does not inherit `$HOME/.local/bin`, edit the desktop entry's `Exec=` line to use the absolute path to `styx-sender-gui`.

## Launching

Run it directly:

```
styx-sender-gui
```

Or open **Styx Sender** from the application launcher.

By default, it edits:

```
~/.config/styx/config.toml
```

Use another config file with:

```
styx-sender-gui --config /path/to/config.toml
```

## Settings

### Receiver hosts

The Mac receiver addresses the sender should try, one address per line. The GUI writes these as `receiver_hosts`.

### Receiver port

The TCP port used by the receiver. This must match the receiver's `listen_port`.

### Linux crossover edge

The Linux-side monitor edge that triggers crossover: `left`, `right`, `top`, or `bottom`.

All selected Linux crossover monitors use the same edge.

### Linux crossover monitors

The Hyprland outputs where styx should create invisible crossover surfaces, one monitor per line.

Use **Refresh Outputs** to list active Hyprland outputs. Use **Use Checked Outputs** to copy the checked outputs into the config. The GUI writes output descriptions when available because descriptions are usually more stable than connector names across reboots and hotplug events.

### Keyboard device

The evdev keyboard device path. Leave this empty to let `styx-sender` auto-detect a keyboard. Use **Detect Keyboard** to populate the first matching `/dev/input/by-id/*event*kbd*` device.

### Heartbeat settings

Controls the sender/receiver connection heartbeat:

- `active_interval_ms`
- `idle_interval_ms`
- `miss_threshold`

The defaults are suitable for normal LAN use.

## Crossover Geometry

The GUI does not define a separate virtual monitor layout. Styx reads the current monitor geometry from Hyprland.

For `left` and `right` crossover edges, styx combines the selected monitors' vertical spans. For `top` and `bottom`, it combines their horizontal spans. Each selected monitor gets a 1-pixel invisible surface on the configured edge.

Hyprland remains the source of truth for monitor position, scale, and transform. If monitor connectors change after styx has started, restart `styx-sender.service` so the sender recreates its Wayland layer surfaces on the current outputs.

The receiver side performs a similar automatic span calculation for the configured Mac return edge. Displays whose return edge lines up with the outermost edge are treated as one combined return span.

Custom per-monitor ranges, dead zones, and styx-specific virtual layout overrides are not currently part of the config schema.

## Saving

On save, the GUI:

1. Reads the current config.
2. Writes the sender settings back to the config file.
3. Preserves known receiver settings if they are present in the same file.
4. Creates a `.bak` copy beside the config before replacing it.

Use **Save & Restart** to write the config and restart `styx-sender.service`.
