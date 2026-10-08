# Styx Relay setup

Styx Relay is a separate edition of Styx, published as regular `relay-v*`
releases from `feat/hybrid-relay`. Standard `v*` releases remain on `main`
and retain the GitHub **Latest** label.

Relay adds a Linux receiver, receiver-to-receiver input forwarding, and
clipboard synchronization across the chain. A Linux sender can also control
a Linux receiver directly, without an intermediate Mac.

## Machine roles

For a workstation → Mac → laptop arrangement:

| Machine | Program | Role |
|---------|---------|------|
| Hyprland Linux workstation | `styx-sender` | Captures the physical keyboard and mouse and sends input to the Mac |
| Mac | `styx-receiver` with `[receiver.relay]` | Receives input locally, then forwards it to the laptop when the cursor reaches the configured forward edge |
| Linux laptop | `styx-receiver` | Injects input locally and returns control to the Mac through its configured return edge |

Input originates at the workstation. Capturing input from the Mac's own
keyboard and mouse remains unimplemented. Clipboard changes can originate
on any connected node, independently of which node is receiving input.

## Install the Relay edition

Choose a **Styx Relay** release on the [Releases page](https://github.com/ghreprimand/styx/releases).
Use that edition on all three machines to keep versions and clipboard behavior
consistent. For Relay 0.5.8, check out the published tag in a separate directory:

```bash
git clone --branch relay-v0.5.8 --depth 1 https://github.com/ghreprimand/styx.git styx-relay-0.5.8
cd styx-relay-0.5.8
```

On the **Linux workstation**, build the sender:

```bash
cargo build --release --locked -p styx-sender
```

For Arch packaging, this checkout's `dist/PKGBUILD` builds `styx-sender-relay`
and conflicts with the standard `styx-sender` package. On other systems,
follow the source/package installation process for that distribution.
See the [sender installation instructions](../README.md#sender-linux) for
the GUI and user service. Preserve your existing sender configuration and
service overrides when upgrading.

On the **Mac**, use the app-bundle installer from this Relay checkout:

```bash
./dist/macos/install.sh
"/Applications/Styx Receiver.app/Contents/MacOS/styx-receiver" --version
```

Reuse the existing `styx-cert` signing identity when upgrading. The installer
creates the app bundle and launchd agent; verify Accessibility permission
for **Styx Receiver.app** in System Settings. The standard
`dist/homebrew/styx-receiver.rb` formula installs the standard edition and
does not enable relay forwarding.

On the **Linux laptop**, build the receiver and follow the
[Linux receiver guide](linux-receiver.md) for `/dev/uinput` access, clipboard
tools, and display layout:

```bash
cargo build --release --locked -p styx-receiver
```

Release downloads also provide `styx-sender-linux-x86_64`,
`styx-receiver-linux-x86_64`, and `styx-receiver-macos-arm64`. Rename them to
`styx-sender` or `styx-receiver` when installing them at the usual executable
paths. A bare macOS receiver download does not create the signed app bundle
or launchd setup. Intel Macs build the receiver from source.

Both executables should report `0.5.8+relay` for this release. Stop manually
launched test instances before starting the installed service; keep one
sender or receiver instance for each configured role.

## Configure each node

Each machine uses its own `~/.config/styx/config.toml`. Preserve existing
working settings and add the relay configuration where needed. The examples
below assume the workstation is left of the Mac and the laptop is right of
the Mac. Choose edges matching your actual arrangement, replace every IP
placeholder, and use the correct monitor names and logical display geometry.

**Linux workstation:** send to the Mac and cross its right edge.

```toml
[sender]
receiver_host = "<mac-ip>"
receiver_port = 4242
monitor = "DP-1"
edge = "right"
```

**Mac:** receive from the workstation, return through the left edge, and
forward through the right edge to the laptop.

```toml
[receiver]
listen_hosts = ["<mac-ip>"]
allowed_senders = ["<workstation-ip>"]
listen_port = 4242
return_edge = "left"

[receiver.relay]
hosts = ["<laptop-ip>"]
port = 4242
forward_edge = "right"
forward_displays = "all"
```

`forward_edge` must differ from `return_edge`. `host` accepts one downstream
IP; `hosts` accepts multiple IPs tried in order. `forward_displays = "all"`
unions the displays sharing the forward edge. Set it to `"primary"` to
restrict forwarding to the display at the global origin `(0, 0)`, for
example when another monitor is stacked above it. Omit `[receiver.relay]`
on a receiver that should not forward onward.

**Linux laptop:** allow the Mac as its immediate sender and return through
the left edge. Replace the illustrative display size with the laptop's
actual logical layout.

```toml
[receiver]
listen_hosts = ["<laptop-ip>"]
allowed_senders = ["<mac-ip>"]
listen_port = 4242
return_edge = "left"

[[receiver.display]]
x = 0
y = 0
width = 1920
height = 1080
```

Each receiver's allowlist names the machine directly connected to it: the
workstation for the Mac, and the Mac for the laptop. Apply the existing
[network trust guidance](security.md#relay-edition) to every hop.

## Start and verify

Start the laptop receiver first, then the Mac receiver, then the workstation
sender. Keep the installed service paths consistent with the programs you
just upgraded.

1. Cross from the workstation to the Mac, then through the Mac's forward edge
   to the laptop. Verify typing, pointer movement, and clicks at both receivers.
2. Cross back from the laptop to the Mac and then to the workstation. Check
   cursor placement and that modifiers do not remain held.
3. Copy text and an image on each node and check that they reach the other nodes.
4. With the laptop disconnected, verify the Mac still works as a local receiver.
   The forward edge stays disarmed while its downstream link is unhealthy.
5. Remove and reconnect a configured workstation monitor. Crossing should
   recover without restarting the sender; removing the monitor during active
   capture should restore local input.

For implementation details and the remaining originating-input design, see
[Relay topology](relay-topology.md).
