# Relay Topology (design)

> **Status: design only.** Nothing in this document is implemented. It exists
> on the `feat/hybrid-relay` branch to record the intended architecture before
> code is written. `main` is unaffected; styx on `main` remains a strictly
> two-party, one-directional Linux-to-Mac KVM.

## Goal

Extend styx from a single sender/receiver pair to a three-node chain:

```
workstation (Linux, Hyprland)  -->  mac  -->  laptop (Linux)
        origin sender            relay        terminal receiver
```

with two additional requirements that constrain the whole design:

1. **The Mac can also be an origin.** When the workstation is off, the Mac's
   own keyboard and trackpad drive the laptop directly.
2. **Degradation must be invisible.** When the laptop is off, unreachable, or
   simply not configured, the workstation/Mac pair must behave *exactly* as it
   does on `main` — same edges, same clamping, same latency, no new failure
   modes. This is the highest-priority constraint in this document; every
   mechanism below is designed to be inert when the downstream link is down.

## Roles, not binaries

The central idea is that "sender" and "receiver" stop being *programs* and
become *roles a node plays on a link*. Each hop is an independent, ordinary
styx connection speaking the existing wire protocol:

| Link | Upstream role | Downstream role |
|------|---------------|-----------------|
| workstation → mac | sender | receiver |
| mac → laptop | sender | receiver |

The Mac therefore holds two link endpoints at once: a receiver endpoint facing
the workstation and a sender endpoint facing the laptop. It is a relay in the
sense that it *routes* an event stream between those two endpoints — not in the
sense that it re-captures anything.

**Consequence: the wire protocol needs no changes at all.** No new event tags,
no version bump, no compatibility break with 0.5.x. `CaptureBegin`,
`CaptureEnd`, and `ReturnToSender` already express everything a hop needs, and
the laptop cannot tell whether its upstream is a workstation or a Mac. This is
the strongest argument for this decomposition over a bespoke "multi-hop"
protocol, and it should be preserved as a hard design rule: if a proposed
feature requires a new event tag, reconsider the feature.

## Modes

The Mac is always in exactly one of four states. Mode is *derived* from link
state, never configured directly.

| Mode | Upstream link | Downstream link | Behaviour |
|------|---------------|-----------------|-----------|
| `Idle` | down | any | Waiting. Local input is untouched. |
| `Receiving` | driving | any | Identical to `main`: inject upstream events locally. |
| `Forwarding` | driving | up | Route upstream events to the laptop; inject nothing locally. |
| `Originating` | down | up | Capture local input and send it to the laptop. |

Transitions:

- `Receiving → Forwarding`: cursor reaches the Mac's **forward edge** (left)
  *and* the downstream link is healthy.
- `Forwarding → Receiving`: laptop sends `ReturnToSender` (cursor came back),
  or downstream link fails (see Recovery).
- `Receiving → Idle`: upstream disconnects.
- `Idle → Originating`: local cursor reaches the forward edge and downstream is
  healthy.
- `Originating → Idle`: laptop sends `ReturnToSender`, or downstream fails.
- **`Originating` is pre-empted by an inbound upstream connection.** If the
  workstation connects while the Mac is originating, the Mac ends local
  capture, releases its keyboard tap, and becomes `Receiving`. One physical
  keyboard drives the chain at a time; the workstation always wins, because it
  is the node holding the evdev grab and cannot be told to back off.

## Degradation: the forward edge is armed, not configured

`return_edge` (right, facing the workstation) keeps its current meaning. A new
optional `forward_edge` (left, facing the laptop) is added, and it is
**armed only while the downstream link is connected and its heartbeat is
healthy**.

When not armed, `Injector::inject_mouse_motion` must not report a forward-edge
hit, and `clamp_to_displays` pins the cursor at the left edge exactly as today.
That is the whole degradation story: an unarmed forward edge is byte-for-byte
the current behaviour, because the current behaviour *is* "no forward edge".

Practical implications:

- Laptop powered off, asleep, on another network, or not in `allowed_senders`:
  the downstream connect never completes, the edge never arms, the Mac is a
  plain receiver. No user-visible change, no error state, no cursor loss.
- Laptop disappears mid-session while the cursor is on the Mac: the heartbeat
  miss disarms the edge within one interval. The worst case is a brief window
  where the cursor is pinned at the left edge instead of crossing — which is
  the desired failure direction.
- No `[relay]` section in the Mac's config: the downstream endpoint is never
  constructed at all, and the binary is functionally identical to 0.5.7.

The reconnect loop for the downstream link must use the sender's existing
capped exponential backoff (`SenderTransport::connect`, 1s → 30s). A laptop
that is off for eight hours must not produce eight hours of connection spam in
`/tmp/styx-receiver.stderr.log` — the 0.5.2 log-capping work exists precisely
because that class of bug already bit once.

## Forwarding mode in detail

While forwarding, the Mac is a pure router with a small amount of per-hop
bookkeeping. What it does **not** do is significant: it does not capture, does
not inject, and does not consult its event tap. The event stream it forwards is
the stream it already receives as decoded protocol frames.

Per-frame policy — the important part is that not everything is forwarded:

| Event | Policy |
|-------|--------|
| `MouseMotion`, `MouseButton`, `MouseScroll`, `KeyPress`, `KeyRelease` | Forward verbatim. |
| `Heartbeat` / `HeartbeatAck` | **Terminate per hop.** Answer upstream locally; run an independent heartbeat downstream. |
| `CaptureBegin` / `CaptureEnd` | Consumed and regenerated per hop, not forwarded. |
| `ReturnToSender` (from laptop) | Consumed: switch to `Receiving`, place local cursor at forward edge. |
| Clipboard events | Hop-by-hop with per-link dedup (see below). |

Heartbeat termination matters more than it looks. If heartbeats were forwarded
end-to-end, the workstation's dead-peer detection would be measuring the
workstation→laptop path and would tear down a perfectly healthy
workstation→Mac link because the laptop went to sleep. Each hop must monitor
only its own liveness.

### Handoff bookkeeping

Crossing Mac → laptop:

1. Capture the currently-held key set from `Injector::held_keys` **before**
   releasing.
2. `Injector::release_all_keys()` locally, so nothing is left stuck on the Mac.
3. Send the held set to the laptop as `KeyPress` events, then `CaptureBegin`
   with the cursor's `from_bottom` / `source_height`. This mirrors what
   `styx-sender` already does with `EvdevCapture::held_modifiers()`.

The physical keyboard is still grabbed by the workstation's evdev grab
throughout, so key-up events for anything held across the handoff will arrive
and be forwarded correctly. Step 3 exists to seed state the laptop cannot
otherwise know.

Crossing laptop → Mac: the laptop sends `ReturnToSender`; the Mac sends
`CaptureEnd` downstream (the laptop's receiver already releases all keys on
`CaptureEnd`) and resumes injecting locally.

### Recovery from a mid-chain failure

These are the cases that will actually bite in daily use, so they need explicit
handling rather than falling out of the reconnect loop:

- **Downstream dies while the cursor is on the laptop.** The cursor is
  stranded: the workstation still holds the evdev grab and is streaming events
  into a router with nowhere to route them. The Mac must synthesise a return —
  place its own cursor at the forward edge, send `CaptureEnd` into the void,
  and switch to `Receiving`. Input resumes on the Mac. The user sees a cursor
  jump, which is the correct outcome; silently swallowing input is not.
- **Upstream dies while forwarding.** The Mac sends `CaptureEnd` downstream,
  releases all keys on both hops, and returns to `Idle`. The laptop must not be
  left with held modifiers.
- **Both links flap simultaneously.** The per-peer force-release backoff on the
  sender side (`force_release_streak`, 300ms → 30s) has an analogue here: mode
  transitions need a cooldown so a flapping laptop cannot cause hundreds of
  handoffs per second.

## Originating mode: the genuinely new component

Everything above is plumbing over existing parts. Originating mode is the one
place that needs a new capability — capturing local input on macOS — and it
should be sequenced last for that reason.

Mechanism: a `CGEventTap` created with `CGEventTapOptions::Default` (the
suppressing variant; `ListenOnly` cannot swallow events) at
`CGEventTapLocation::HID`, head-inserted.

### Permissions: a second TCC grant is required

An earlier draft of this document claimed the existing Accessibility grant
covers the tap and no new prompt appears. **That is wrong.** Accessibility
(`kTCCServicePostEvent`) authorises *posting* events, which is what the
receiver does today. Creating a tap that *observes* events system-wide is a
distinct TCC service, Input Monitoring (`kTCCServiceListenEvent`), granted
separately in System Settings › Privacy & Security › **Input Monitoring**. A
suppressing tap generally needs both: Input Monitoring to see events,
Accessibility to alter or swallow them.

Consequences for packaging and docs, none of them hard but all of them needing
handling:

- Origin mode triggers a **second permission prompt** the first time the tap is
  created ("would like to receive keystrokes from any application"). The
  install script and README currently document one grant; they will need a
  second, and the prompt appears at first *use*, not at install.
- TCC decisions are bound to code identity, so a rebuild that changes the
  signature can invalidate the grant. Styx already solved this for
  Accessibility with the stable `styx-cert` identity created in
  `dist/macos/install.sh`, and the same identity covers Input Monitoring — but
  the "re-grant after every rebuild" trap returns for anyone on ad-hoc signing.
- **A non-null tap is not necessarily a working tap.** Without Input
  Monitoring, `CGEventTapCreate` can return a valid-looking Mach port that
  never delivers events — indistinguishable at construction time from a healthy
  tap. The implementation must verify tap health at runtime and log clearly, or
  this failure mode presents as "origin mode does nothing" with no diagnostic.

Because this is a user-visible setup step rather than a code problem, it is a
further argument for sequencing origin mode last: steps 1–3 need no new
permissions at all.

While capturing:

- `CGAssociateMouseAndMouseCursorPosition(false)` to decouple the pointer, and
  `CGDisplayHideCursor` so the local cursor does not sit visibly frozen.
- Read raw deltas from the tap's `MOUSE_EVENT_DELTA_X` / `MOUSE_EVENT_DELTA_Y`
  fields (both present in the `core-graphics` 0.24 crate already in
  `styx-receiver`'s dependencies), not from absolute cursor positions.
- Translate macOS virtual keycodes to evdev scancodes with
  `styx_keymap::macos_to_evdev`. **This function already exists and is
  currently unused** — the reverse direction was written for symmetry when the
  keymap crate was built. Originating mode is what it was waiting for.

### Loop prevention

A tap at HID location sees synthetic events posted by `CGEvent::post`,
including styx's own injected events. Without a filter, `Receiving` mode would
feed the Mac's tap with the workstation's input and re-send it downstream — an
immediate feedback loop.

The fix is to stamp every injected event with a magic value in
`EVENT_SOURCE_USER_DATA` (field 42) at injection time, and drop any tapped
event carrying that stamp. This is the conventional technique for the problem
and is more reliable than filtering on `EVENT_SOURCE_STATE_ID`, since the stamp
is unique to styx rather than shared by every synthetic-event producer.

Additionally, the tap should be **disabled outright** whenever the Mac is not
in `Idle` or `Originating`. A suppressing tap left enabled during `Receiving`
is a liability: it adds latency to every injected event and risks swallowing
input if the stamp filter ever has a gap. Belt and braces.

### Known limitations of tap-based capture

These are properties of the mechanism, not bugs to be fixed later. All three
are scoped to **origin mode only** — they cannot affect `Receiving` or
`Forwarding`, because on those paths the keyboard is grabbed by the
workstation's evdev grab and macOS never sees a physical keystroke at all. The
existing workstation↔Mac setup is untouched by everything in this section.

#### Secure event input

`EnableSecureEventInput` is the macOS facility that lets a process protect
keystrokes from interception. Apple's technical note on it names event-tap
installation explicitly as one of the interception techniques it is designed to
defeat, so this is the OS working as intended rather than a gap to route
around. It is triggered by password fields in browsers and system dialogs, and
by Terminal's **Secure Keyboard Entry** setting.

The failure mode is what makes this worth understanding rather than merely
noting. Secure input is **system-wide and focus-driven**, not scoped to the
field being typed into. In origin mode the cursor is over on the laptop, but
the Mac still has some frontmost application, and if that application has
secure input active then the tap silently receives nothing. The user is typing
into the laptop and the keystrokes vanish.

Signature to recognise: **mouse continues working, keyboard stops.** Secure
input covers keyboard events only, so pointer motion relays normally while
every keystroke disappears. Without knowing the mechanism this looks exactly
like a stuck-key or dropped-connection bug, and would be debugged in entirely
the wrong direction.

The persistent case is worse than the transient one. A focused password field
clears when focus moves. Terminal with Secure Keyboard Entry enabled holds
secure input for as long as it is frontmost, so a user with that setting on
would find origin mode's keyboard dead every time Terminal has focus, with no
error anywhere.

Mitigation — detection, not circumvention. `IsSecureEventInputEnabled()`
(Carbon/HIToolbox) reports the current state; Hammerspoon exposes exactly this
call for exactly this reason. Poll it while origin mode is active and log a
clear line when it flips on. Converting a silent, misleading failure into a
legible one is the whole of the available fix.

#### Reserved system combinations

Some system-level shortcuts are handled above the tap and can be neither
suppressed nor forwarded. Practical effect: a handful of key combinations will
act on the Mac instead of relaying to the laptop. Minor, but it means origin
mode cannot promise full keyboard fidelity the way the evdev path can — evdev's
`EVIOCGRAB` takes the device before the compositor, which is strictly more
complete than anything a tap can do.

#### Tap disabling

macOS disables a tap whose callback is too slow, notifying via
`kCGEventTapDisabledByTimeout`; user input can also disable a tap
(`kCGEventTapDisabledByUserInput`). Both constants are already in the
`core-graphics` 0.24 enum as `CGEventType::TapDisabledByTimeout` and
`TapDisabledByUserInput`, so they arrive through the normal callback path.

Two implementation requirements follow. The callback must never block — push
onto a channel and return, never await inside it — because a slow callback is
precisely what triggers the disable. And the tap must be re-enabled on receipt
of either constant; `CGEventTap::enable()` exists in the crate for this.

One crate gap: `enable()` hardcodes `CGEventTapEnable(port, true)` and there is
no disable path. Since the design calls for disabling the tap outside `Idle` and
`Originating`, styx needs its own `extern "C"` declaration for
`CGEventTapEnable` to pass `false`. `CGEventTap::mach_port` is a public field,
so this is a few lines rather than a fork.

## Clipboard in a three-node chain

The existing dedup is a single `last_clip_hash` per node, which is correct for
two nodes and wrong for three: content arriving from upstream is written to the
Mac's pasteboard, the 10 Hz `NSPasteboard.changeCount` poll observes the change,
and forwards it back out — potentially into a ping-pong across three nodes.

Minimum viable fix: track the last-seen hash **per link** rather than per node,
and never re-emit content on the link it arrived from. The type-prefixed
hashing already in `clipboard.rs` (distinct kind bytes for text, image, HTML)
carries over unchanged.

Recommendation: **defer clipboard relay entirely for the first implementation.**
Get the input path correct with clipboard forwarding disabled on the
Mac→laptop hop, then add it as a separate change with its own tests. A stuck
key is annoying; a clipboard loop that overwrites content on three machines is
data loss.

## Prerequisite

None of this is reachable without a **Linux receiver**, which does not exist
today — `styx-receiver` is macOS-only (`core-graphics` injection,
`NSPasteboard` clipboard). The receiver needs splitting into platform-gated
inject backends with a `/dev/uinput` implementation for Linux (the `evdev`
crate already in the sender's dependencies provides `VirtualDeviceBuilder`).
The relay is useless until the laptop can receive.

## Suggested sequence

Each step is independently testable and independently useful:

1. **Linux receiver.** Verify by pointing the *existing* workstation sender at
   the laptop instead of the Mac. No relay code involved. This alone gives a
   working workstation↔laptop pair.
2. **Downstream endpoint + forward edge on the Mac, disarmed by default.**
   Ship it with no relay config present and confirm byte-identical behaviour to
   0.5.7 on the existing pair. This is where the degradation guarantee is
   proven, before any relay logic can regress it.
3. **Forwarding mode.** The full three-node chain, workstation-driven.
4. **Originating mode.** The `CGEventTap` work, plus pre-emption by upstream.
5. **Clipboard relay.** Per-link dedup, separately tested.

## Security notes

The chain changes the threat model in ways `docs/security.md` does not yet
cover, and that document must be updated alongside any implementation:

- The Mac gains the ability to inject input into a Linux machine. Styx's
  current unidirectionality guarantee ("the Mac can never type on Linux") no
  longer holds globally — it holds only per-link. This is an intentional
  change, but it is a change.
- The laptop's receiver needs its own `listen_hosts` / `allowed_senders`, with
  the Mac's IP as the permitted peer. A travelling laptop needs the same
  public-network protection the Mac already has.
- All three links remain plaintext TCP. A three-node chain doubles the wire
  exposure of every keystroke typed on the laptop, since it traverses two hops.
  The planned TLS work becomes more valuable, not less.
