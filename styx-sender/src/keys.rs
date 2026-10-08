//! Held-key bookkeeping across several captured keyboards.
//!
//! The protocol carries no device identity: the receiver keeps one held
//! state per keycode. When more than one keyboard is captured, the sender
//! has to reconcile ownership itself so that, e.g., releasing Shift on one
//! keyboard does not clear Shift on the receiver while another keyboard
//! still holds it.

use std::collections::{BTreeMap, BTreeSet};

use styx_proto::Event;

/// Identifies one captured keyboard for the lifetime of the sender.
pub type DeviceId = u64;

/// A key transition read from one keyboard.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyInput {
    Press(u32),
    Release(u32),
    /// Kernel auto-repeat.
    Repeat(u32),
}

/// Which keyboards hold each key that has been forwarded to the receiver.
#[derive(Default)]
pub struct KeyOwnership {
    owners: BTreeMap<u32, BTreeSet<DeviceId>>,
}

impl KeyOwnership {
    pub fn new() -> Self {
        Self::default()
    }

    /// Translate one key transition from `dev` into the event to forward,
    /// if any.
    pub fn apply(&mut self, dev: DeviceId, input: KeyInput) -> Option<Event> {
        match input {
            KeyInput::Press(code) => {
                let owners = self.owners.entry(code).or_default();
                let first = owners.is_empty();
                owners.insert(dev);
                // A modifier already held by another keyboard is not pressed
                // again: a duplicate modifier-down on macOS triggers
                // unintended shortcuts, the same reason repeats are
                // suppressed below.
                if styx_keymap::is_modifier(code) && !first {
                    None
                } else {
                    Some(Event::KeyPress { code })
                }
            }
            KeyInput::Repeat(code) => {
                // Forward as another key press since macOS doesn't repeat
                // programmatically posted events. Suppress repeats for
                // modifier keys -- they cause duplicate modifier-down events
                // on macOS which triggers unintended shortcuts and special
                // characters.
                if styx_keymap::is_modifier(code) {
                    None
                } else {
                    self.owners.entry(code).or_default().insert(dev);
                    Some(Event::KeyPress { code })
                }
            }
            KeyInput::Release(code) => match self.owners.get_mut(&code) {
                Some(owners) => {
                    owners.remove(&dev);
                    if owners.is_empty() {
                        self.owners.remove(&code);
                        Some(Event::KeyRelease { code })
                    } else {
                        // Another keyboard still holds this key.
                        None
                    }
                }
                // Not tracked (e.g. pressed before capture began): forward
                // the release so the receiver can't be left holding it.
                None => Some(Event::KeyRelease { code }),
            },
        }
    }

    /// Record modifiers `dev` already holds when it is first captured --
    /// at crossover, or when a keyboard is added mid-capture. Returns
    /// presses for the ones the receiver doesn't already have down.
    pub fn seed(&mut self, dev: DeviceId, held_modifiers: &[u32]) -> Vec<Event> {
        let mut events = Vec::new();
        for &code in held_modifiers {
            let owners = self.owners.entry(code).or_default();
            if owners.is_empty() {
                events.push(Event::KeyPress { code });
            }
            owners.insert(dev);
        }
        events
    }

    /// Drop every key `dev` holds, e.g. because the device disconnected.
    /// Returns releases for keys no remaining keyboard holds.
    pub fn remove_device(&mut self, dev: DeviceId) -> Vec<Event> {
        let mut events = Vec::new();
        self.owners.retain(|&code, owners| {
            if !owners.remove(&dev) || !owners.is_empty() {
                return true;
            }
            events.push(Event::KeyRelease { code });
            false
        });
        events
    }

    /// Release everything, e.g. at the end of a capture.
    pub fn release_all(&mut self) -> Vec<Event> {
        std::mem::take(&mut self.owners)
            .into_keys()
            .map(|code| Event::KeyRelease { code })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use styx_keymap::{KEY_LEFT_ALT, KEY_LEFT_CTRL, KEY_LEFT_SHIFT};

    const A: DeviceId = 1;
    const B: DeviceId = 2;
    const C: DeviceId = 3;
    const KEY_Q: u32 = 16;

    fn press(code: u32) -> Event {
        Event::KeyPress { code }
    }

    fn release(code: u32) -> Event {
        Event::KeyRelease { code }
    }

    // Regression: a modifier held before startup/crossover is forwarded
    // from kernel key state rather than from a key-down event. Removing
    // that keyboard while another stays connected must still release it,
    // because capture continues and no CaptureEnd clears the receiver.
    #[test]
    fn modifier_held_at_crossover_is_released_when_its_keyboard_is_removed() {
        let mut keys = KeyOwnership::new();
        // Crossover: A holds Ctrl, B holds nothing.
        assert_eq!(keys.seed(A, &[KEY_LEFT_CTRL]), vec![press(KEY_LEFT_CTRL)]);
        assert_eq!(keys.seed(B, &[]), vec![]);

        // A is unplugged; B remains and capture continues.
        assert_eq!(keys.remove_device(A), vec![release(KEY_LEFT_CTRL)]);

        // Nothing is left held for B to leak at capture end.
        assert_eq!(keys.release_all(), vec![]);
    }

    // Regression: the same modifier held on two keyboards must stay down
    // on the receiver until neither holds it -- whether one keyboard
    // releases it or disconnects.
    #[test]
    fn shared_modifier_stays_held_until_last_keyboard_releases_it() {
        let mut keys = KeyOwnership::new();
        assert_eq!(
            keys.apply(A, KeyInput::Press(KEY_LEFT_SHIFT)),
            Some(press(KEY_LEFT_SHIFT))
        );
        // Second Shift-down is not forwarded: the receiver already has it.
        assert_eq!(keys.apply(B, KeyInput::Press(KEY_LEFT_SHIFT)), None);

        // A lets go; B still holds Shift, so nothing is forwarded.
        assert_eq!(keys.apply(A, KeyInput::Release(KEY_LEFT_SHIFT)), None);
        // B lets go; now the receiver is told.
        assert_eq!(
            keys.apply(B, KeyInput::Release(KEY_LEFT_SHIFT)),
            Some(release(KEY_LEFT_SHIFT))
        );
    }

    #[test]
    fn shared_modifier_stays_held_when_one_keyboard_disconnects() {
        let mut keys = KeyOwnership::new();
        assert_eq!(
            keys.apply(A, KeyInput::Press(KEY_LEFT_SHIFT)),
            Some(press(KEY_LEFT_SHIFT))
        );
        assert_eq!(keys.apply(B, KeyInput::Press(KEY_LEFT_SHIFT)), None);

        // A is unplugged while B still holds Shift.
        assert_eq!(keys.remove_device(A), vec![]);
        assert_eq!(
            keys.apply(B, KeyInput::Release(KEY_LEFT_SHIFT)),
            Some(release(KEY_LEFT_SHIFT))
        );
    }

    #[test]
    fn shared_modifier_seeded_on_both_keyboards_at_crossover() {
        let mut keys = KeyOwnership::new();
        assert_eq!(keys.seed(A, &[KEY_LEFT_SHIFT]), vec![press(KEY_LEFT_SHIFT)]);
        assert_eq!(keys.seed(B, &[KEY_LEFT_SHIFT]), vec![]);

        assert_eq!(keys.remove_device(A), vec![]);
        assert_eq!(keys.remove_device(B), vec![release(KEY_LEFT_SHIFT)]);
    }

    // Regression: a keyboard added during an active capture with a
    // modifier already held must have that modifier pressed on the
    // receiver, and released again when the key or keyboard goes away.
    #[test]
    fn keyboard_added_mid_capture_with_modifier_held_is_synchronized() {
        let mut keys = KeyOwnership::new();
        assert_eq!(keys.seed(A, &[]), vec![]);

        // C appears mid-capture with Alt already down.
        assert_eq!(keys.seed(C, &[KEY_LEFT_ALT]), vec![press(KEY_LEFT_ALT)]);
        assert_eq!(
            keys.apply(C, KeyInput::Release(KEY_LEFT_ALT)),
            Some(release(KEY_LEFT_ALT))
        );

        // Same again, but C is unplugged instead of releasing the key.
        assert_eq!(keys.seed(C, &[KEY_LEFT_ALT]), vec![press(KEY_LEFT_ALT)]);
        assert_eq!(keys.remove_device(C), vec![release(KEY_LEFT_ALT)]);
    }

    #[test]
    fn keyboard_added_mid_capture_with_modifier_already_held_elsewhere() {
        let mut keys = KeyOwnership::new();
        assert_eq!(
            keys.apply(A, KeyInput::Press(KEY_LEFT_CTRL)),
            Some(press(KEY_LEFT_CTRL))
        );

        // C appears holding Ctrl too: the receiver already has it down.
        assert_eq!(keys.seed(C, &[KEY_LEFT_CTRL]), vec![]);
        assert_eq!(keys.apply(A, KeyInput::Release(KEY_LEFT_CTRL)), None);
        assert_eq!(
            keys.apply(C, KeyInput::Release(KEY_LEFT_CTRL)),
            Some(release(KEY_LEFT_CTRL))
        );
    }

    // Non-modifier press and repeat behaviour is unchanged.
    #[test]
    fn non_modifier_presses_and_repeats_are_forwarded() {
        let mut keys = KeyOwnership::new();
        assert_eq!(keys.apply(A, KeyInput::Press(KEY_Q)), Some(press(KEY_Q)));
        assert_eq!(keys.apply(A, KeyInput::Repeat(KEY_Q)), Some(press(KEY_Q)));
        assert_eq!(keys.apply(A, KeyInput::Repeat(KEY_Q)), Some(press(KEY_Q)));
        assert_eq!(
            keys.apply(A, KeyInput::Release(KEY_Q)),
            Some(release(KEY_Q))
        );
    }

    #[test]
    fn modifier_repeats_are_suppressed() {
        let mut keys = KeyOwnership::new();
        assert_eq!(
            keys.apply(A, KeyInput::Press(KEY_LEFT_CTRL)),
            Some(press(KEY_LEFT_CTRL))
        );
        assert_eq!(keys.apply(A, KeyInput::Repeat(KEY_LEFT_CTRL)), None);
        assert_eq!(
            keys.apply(A, KeyInput::Release(KEY_LEFT_CTRL)),
            Some(release(KEY_LEFT_CTRL))
        );
    }

    #[test]
    fn release_of_untracked_key_is_forwarded() {
        // e.g. a key held before crossover that isn't a modifier, released
        // during capture: forwarded as before.
        let mut keys = KeyOwnership::new();
        assert_eq!(
            keys.apply(A, KeyInput::Release(KEY_Q)),
            Some(release(KEY_Q))
        );
    }

    #[test]
    fn release_all_clears_every_owner() {
        let mut keys = KeyOwnership::new();
        keys.seed(A, &[KEY_LEFT_CTRL]);
        keys.apply(B, KeyInput::Press(KEY_LEFT_CTRL));
        keys.apply(B, KeyInput::Press(KEY_Q));

        // Released in keycode order: Q (16) before left Ctrl (29).
        assert_eq!(
            keys.release_all(),
            vec![release(KEY_Q), release(KEY_LEFT_CTRL)]
        );
        assert_eq!(keys.release_all(), vec![]);
    }
}
