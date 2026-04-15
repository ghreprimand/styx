# Security Model

Styx is built for the use case of sharing one Linux machine's keyboard and mouse with a nearby Mac on a trusted home network. The defaults and the feature set both assume this scope. If your scope is different, parts of this document describe how to harden accordingly; parts describe limits you will need to accept or mitigate outside styx.

## Trust boundary

Styx trusts the network segment that connects the sender (Linux) and the receiver (Mac). Everything on that segment is assumed benign. In the default setup this means:

- Your home LAN (wired and wifi), behind a consumer router that blocks inbound traffic from the internet.
- All devices on the LAN are presumed trusted (your own computers, phones, tablets, printers, IoT devices).

Styx does not attempt to defend against:

- Other hosts on the same LAN that are hostile or compromised.
- Network observers positioned between the sender and receiver (tap point on the router, compromised switch, someone running tcpdump on a broadcast/monitor port).
- Processes running on the same host as the sender or receiver (they can already observe the keyboard and clipboard through normal OS APIs).

If any of those threats are in scope for you, the mitigations are outside styx: segment the LAN (VLANs), tunnel styx inside a VPN (Tailscale, WireGuard), or do not run styx until you move to a trusted network.

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

### Restricting the bind surface

The recommended hardening for laptops that sometimes join untrusted networks is to bind only to specific home-network IPs, using `listen_hosts` in the receiver config:

```toml
[receiver]
listen_hosts = ["192.168.1.10", "192.168.1.11"]
listen_port = 4242
```

The receiver attempts to bind to every host in the list, logs warnings for the ones it cannot bind (interface not up, address not configured), and fails cleanly if none bind. The operational pattern is:

- Reserve your Mac's ethernet and wifi IPs with DHCP on your home router so those addresses are stable.
- List both in `listen_hosts`.
- On your home network, at least one interface has one of those IPs and the receiver binds. The receiver is reachable only on those IPs; a device using a different IP on the same LAN cannot connect.
- On any other network (public wifi, hotspot, someone else's home), neither reservation matches the current DHCP lease. Every bind fails, the receiver exits cleanly, and there is nothing listening.

This does not defend against hostile devices on your home LAN itself. It defends against hostile devices on networks styx was not designed for.

### Port scans

A port scanner on the same LAN will notice TCP/4242 open as long as the receiver is bound. If you would rather the port be completely invisible to LAN scans, the macOS built-in firewall can drop unsolicited connections while still allowing sessions the receiver initiated — but styx is a listening service by design, so this only works if the sender's specific IP is in an allowlist. `listen_hosts` is simpler and accomplishes the same goal with less configuration.

## Recommendations, ordered by how paranoid you want to be

**Default home-only use.** `listen_host = "0.0.0.0"` is fine if your home LAN is trusted and the laptop never leaves it. No action needed.

**Laptop that travels.** Set `listen_hosts` to the Mac's home ethernet and wifi IPs (DHCP-reserved). Receiver exits cleanly on any other network. This is the recommended 0.5.0+ configuration for most users. See `docs/clipboard-sync.md` and the README config section.

**Distrust the home LAN.** Run styx inside a VPN tunnel (Tailscale, WireGuard). Set `listen_hosts` to the VPN interface's IP. The TCP port is only reachable through the tunnel. The tunnel encrypts everything on the wire, so both the injection risk and the passive-sniffing risk are eliminated for devices outside the VPN.

**Distrust everything on the LAN including VPN members.** Not yet supported. Planned for a future release via TLS with client-cert auth — see the roadmap in `docs/handoff-0.5.0.md` (or the corresponding file for the current release). If you need this today, tunnel styx over SSH: the `ssh -L` port-forward pattern terminates the TCP connection on each side and negotiates its own encryption.

## Reporting security issues

For problems that should not be discussed in public: open a GitHub security advisory on the styx repository at `https://github.com/ghreprimand/styx/security/advisories/new`. For problems that are routine (config confusion, "how do I harden X"): use regular GitHub issues.

## Release history relevant to security

- **0.5.0**: Added `listen_hosts` array so the receiver can bind to specific home IPs and exit cleanly on networks without them.
- **0.4.0**: Added the proactive clipboard poll. Clipboard contents now fly every ~100 ms while content changes rather than only at crossover. This increases what a passive observer sees if they are positioned to sniff the wire.
- **0.3.0**: Added text clipboard sync. Until this release, no clipboard content was ever on the wire.
- Pre-0.3.0: Input events only. No clipboard exposure.
