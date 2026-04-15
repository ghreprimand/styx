# Security Model

Styx is built for the use case of sharing one Linux machine's keyboard and mouse with a nearby Mac on a trusted home network. The defaults and the feature set both assume this scope. If your scope is different, parts of this document describe how to harden accordingly; parts describe limits you will need to accept or mitigate outside styx.

## Trust boundary

Styx trusts the network segment that connects the sender (Linux) and the receiver (Mac). Everything on that segment is assumed benign. In the default setup this means:

- Your home LAN (wired and wifi), behind a consumer router that blocks inbound traffic from the internet.
- All devices on the LAN are presumed trusted (your own computers, phones, tablets, printers, IoT devices).

Styx's in-network-layer defenses (`listen_hosts`, `allowed_senders`) cover:

- Complete absence of exposure on networks without any configured home IP (public wifi, coworking networks, hotels).
- Hostile or compromised hosts on the same LAN that attempt to initiate a TCP connection to the receiver: rejected at accept time by source-IP allowlist.
- The edge case of a foreign network coincidentally assigning your Mac one of its `listen_hosts` IPs: the receiver binds, but `allowed_senders` rejects every connection because no peer on that network has your sender's specific IP.

Styx does not defend against:

- **Passive network observers** positioned between the sender and receiver (tap point on the router, compromised switch, someone running tcpdump on a broadcast/monitor port while you are actively typing or copying). All traffic is plaintext TCP. This is realistic on compromised wired LANs and far harder on wifi (WPA2/WPA3 encrypts the over-the-air segment); an attacker on the same open wifi would still be blocked from connecting by `allowed_senders` but could observe a legitimate session in flight.
- **Processes running on the same host as the sender or receiver**: they can already observe the keyboard and clipboard through normal OS APIs.
- **Active IP spoofing** on a switched LAN by a sophisticated attacker who can convince the switch to forward traffic addressed to your sender's IP to their own machine. Non-trivial on most home networks but theoretically possible.

If any of those threats are in scope for you, the mitigation is TLS with client-cert auth (planned for 0.6.0) or tunneling styx inside a VPN (WireGuard, Tailscale) so the traffic is encrypted on the wire.

## What styx puts on the wire

Everything styx sends over TCP is unencrypted. The traffic consists of:

- **Mouse and keyboard events.** Keycodes, modifiers, button numbers, motion deltas. Observable but low-value individually; in aggregate, a passive observer could reconstruct what you typed, including passwords.
- **Clipboard text.** Every copy operation on either machine. An observer sees passwords you paste, URLs with session tokens, addresses, personal notes, code.
- **Clipboard images.** PNG bytes. Screenshots of whatever you copied.
- **Clipboard HTML** (0.5.0+). Rich text with formatting.
- **Heartbeats.** Empty pings; uninteresting.

## Network exposure

### Default bind

The receiver listens on the TCP port configured by `listen_port` (default `4242`). The interface is controlled by `listen_host` (default `"0.0.0.0"`, meaning every network interface).

On `0.0.0.0`, the receiver accepts a connection from any device that can reach the port. No authentication. Any connected sender can:

1. Inject arbitrary keystrokes and mouse clicks. On a Mac with Accessibility granted to the receiver app, this is equivalent to the attacker sitting at the keyboard: they can open Terminal, type commands, exfiltrate data.
2. Receive every clipboard update the receiver pushes out. The receiver's 10 Hz `NSPasteboard.changeCount` poll (added in 0.4.0) forwards new clipboard content to whoever is connected.
3. Write arbitrary content (text, HTML, PNG) to the clipboard that will be pasted on the next `Cmd+V` of an unsuspecting user.

A passive observer on the LAN — without connecting — can still see every packet of an existing connection. All clipboard contents fly in plaintext.

### Layer 1: `listen_hosts` (bind surface)

Restrict which interfaces the receiver listens on. Added in 0.5.0.

```toml
[receiver]
listen_hosts = ["192.168.1.10", "192.168.1.11"]
listen_port = 4242
```

The receiver attempts to bind to every host in the list, logs warnings for the ones it cannot bind (interface not up, address not configured), and fails cleanly if none bind. The operational pattern is:

- Reserve your Mac's ethernet and wifi IPs with DHCP on your home router so those addresses are stable.
- List both in `listen_hosts`.
- On your home network, at least one interface has one of those IPs and the receiver binds. The receiver is reachable only on those IPs.
- On any other network (public wifi, hotspot, someone else's home), neither reservation matches the current DHCP lease. Every bind fails, the receiver exits cleanly, and there is nothing listening.

What `listen_hosts` alone does not cover:

- A hostile device on your own home LAN that can route packets to one of your `listen_hosts` IPs: it can connect and inject keystrokes or read the clipboard.
- The residual case where a foreign network coincidentally uses the same subnet (`192.168.1.0/24` is common) and DHCP happens to assign you one of your listed IPs: the receiver binds and is exposed on that foreign network.

### Layer 2: `allowed_senders` (peer allowlist)

Restrict which peers are allowed to connect. Added in 0.5.1.

```toml
[receiver]
listen_hosts = ["192.168.1.10", "192.168.1.11"]
allowed_senders = ["192.168.1.12"]
listen_port = 4242
```

The receiver accepts the TCP handshake from any peer (you cannot avoid that — the kernel does it automatically once `listen` is called), but immediately drops the connection if the peer's IP is not in `allowed_senders`, before any styx events are read. Connections from non-allowed peers see the TCP `SYN/ACK` go through and then a `RST` or `FIN` arrive, and get no further.

With both layers configured:

- Public wifi without any home IP on a live interface: receiver exits, nothing listens. Covered by `listen_hosts`.
- Public wifi with coincidentally-matching IP on a live interface: receiver binds, but any attacker connecting has a different source IP than your sender and gets rejected. Covered by `allowed_senders`.
- Home LAN with hostile peer (compromised IoT, guest laptop, neighbor on wifi): attacker tries to connect, source IP does not match, rejected. Covered by `allowed_senders`.
- Home LAN with legitimate sender: matches both `listen_hosts` (for the bind) and `allowed_senders` (for the accept). Works normally.

DHCP-reserve the sender's IP on your home router so the allowlist entry stays valid.

### Port scans

A port scanner on the same LAN will notice TCP/4242 open as long as the receiver is bound. If you would rather the port be completely invisible to LAN scans, the macOS built-in firewall can drop unsolicited connections while still allowing sessions the receiver initiated — but styx is a listening service by design, so this only works if the sender's specific IP is in an allowlist. `listen_hosts` is simpler and accomplishes the same goal with less configuration.

## Recommendations, ordered by how paranoid you want to be

**Default home-only use.** `listen_host = "0.0.0.0"` is fine if your home LAN is small, trusted, and the laptop never leaves it. No action needed.

**Laptop that travels and home LAN you partially trust.** This is the recommended 0.5.1+ configuration for most users:

```toml
[receiver]
listen_hosts = ["<mac-ethernet-ip>", "<mac-wifi-ip>"]
allowed_senders = ["<sender-ip>"]
listen_port = 4242
```

Receiver exits cleanly on any network where none of your `listen_hosts` IPs have a live interface, and rejects every peer except your sender on networks where they do. All three realistic threats (public-wifi exposure, subnet-collision exposure, hostile-LAN peer) are covered. Passive sniffing of legitimate sessions is still possible for someone with wire-tap access to your home LAN, but rarely a realistic threat in a home environment.

**Distrust wire-level observers on your home LAN.** Tunnel styx inside a VPN (WireGuard). Set `listen_hosts` to the VPN interface's IP. The TCP port is only reachable through the tunnel; the tunnel encrypts everything on the wire. Combined with `allowed_senders` scoped to the sender's VPN-side IP, this defeats both injection and passive-sniffing.

**Distrust everything on the LAN including VPN members.** Not yet supported natively. Planned for 0.6.0 via TLS with client-cert auth. If you need this today, tunnel styx over SSH: `ssh -L` port-forward terminates the TCP on each side and negotiates its own encryption.

## Reporting security issues

For problems that should not be discussed in public: open a GitHub security advisory on the styx repository at `https://github.com/ghreprimand/styx/security/advisories/new`. For problems that are routine (config confusion, "how do I harden X"): use regular GitHub issues.

## Release history relevant to security

- **0.5.1**: Added `allowed_senders` array. The receiver rejects every TCP connection whose peer IP is not on the list, before any styx events are read. Closes the hostile-LAN-peer gap that `listen_hosts` alone did not cover.
- **0.5.0**: Added `listen_hosts` array so the receiver can bind to specific home IPs and exit cleanly on networks without them.
- **0.4.0**: Added the proactive clipboard poll. Clipboard contents now fly every ~100 ms while content changes rather than only at crossover. This increases what a passive observer sees if they are positioned to sniff the wire.
- **0.3.0**: Added text clipboard sync. Until this release, no clipboard content was ever on the wire.
- Pre-0.3.0: Input events only. No clipboard exposure.
