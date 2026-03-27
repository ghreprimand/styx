use keycode::{KeyMap, KeyMapping};

// evdev modifier scancodes.
pub const KEY_LEFT_CTRL: u32 = 29;
pub const KEY_LEFT_SHIFT: u32 = 42;
pub const KEY_LEFT_ALT: u32 = 56;
pub const KEY_LEFT_META: u32 = 125;
pub const KEY_RIGHT_CTRL: u32 = 97;
pub const KEY_RIGHT_SHIFT: u32 = 54;
pub const KEY_RIGHT_ALT: u32 = 100;
pub const KEY_RIGHT_META: u32 = 126;

pub const MODIFIER_KEYS: &[u32] = &[
    KEY_LEFT_CTRL,
    KEY_LEFT_SHIFT,
    KEY_LEFT_ALT,
    KEY_LEFT_META,
    KEY_RIGHT_CTRL,
    KEY_RIGHT_SHIFT,
    KEY_RIGHT_ALT,
    KEY_RIGHT_META,
];

pub fn is_modifier(evdev_code: u32) -> bool {
    MODIFIER_KEYS.contains(&evdev_code)
}

pub fn evdev_to_macos(evdev_code: u16) -> Option<u16> {
    KeyMap::from_key_mapping(KeyMapping::Evdev(evdev_code))
        .map(|km| km.mac)
        .ok()
        .filter(|&code| code != 0xFFFF)
}

pub fn macos_to_evdev(mac_code: u16) -> Option<u16> {
    KeyMap::from_key_mapping(KeyMapping::Mac(mac_code))
        .map(|km| km.evdev)
        .ok()
        .filter(|&code| code != 0xFFFF)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_keys_map() {
        // A key: evdev 30, macOS 0x00
        assert_eq!(evdev_to_macos(30), Some(0x00));
        // S key: evdev 31, macOS 0x01
        assert_eq!(evdev_to_macos(31), Some(0x01));
        // D key: evdev 32, macOS 0x02
        assert_eq!(evdev_to_macos(32), Some(0x02));
        // Space: evdev 57, macOS 0x31
        assert_eq!(evdev_to_macos(57), Some(0x31));
        // Return: evdev 28, macOS 0x24
        assert_eq!(evdev_to_macos(28), Some(0x24));
        // Escape: evdev 1, macOS 0x35
        assert_eq!(evdev_to_macos(1), Some(0x35));
        // Tab: evdev 15, macOS 0x30
        assert_eq!(evdev_to_macos(15), Some(0x30));
        // Backspace: evdev 14, macOS 0x33
        assert_eq!(evdev_to_macos(14), Some(0x33));
    }

    #[test]
    fn modifier_keys_map() {
        assert!(evdev_to_macos(KEY_LEFT_SHIFT as u16).is_some());
        assert!(evdev_to_macos(KEY_LEFT_CTRL as u16).is_some());
        assert!(evdev_to_macos(KEY_LEFT_ALT as u16).is_some());
        assert!(evdev_to_macos(KEY_LEFT_META as u16).is_some());
        assert!(evdev_to_macos(KEY_RIGHT_SHIFT as u16).is_some());
        assert!(evdev_to_macos(KEY_RIGHT_CTRL as u16).is_some());
        assert!(evdev_to_macos(KEY_RIGHT_ALT as u16).is_some());
    }

    #[test]
    fn f_keys_map() {
        // F1: evdev 59, macOS 0x7A
        assert_eq!(evdev_to_macos(59), Some(0x7A));
        // F12: evdev 88, macOS 0x6F
        assert_eq!(evdev_to_macos(88), Some(0x6F));
    }

    #[test]
    fn arrow_keys_map() {
        // Up: evdev 103, macOS 0x7E
        assert_eq!(evdev_to_macos(103), Some(0x7E));
        // Down: evdev 108, macOS 0x7D
        assert_eq!(evdev_to_macos(108), Some(0x7D));
        // Left: evdev 105, macOS 0x7B
        assert_eq!(evdev_to_macos(105), Some(0x7B));
        // Right: evdev 106, macOS 0x7C
        assert_eq!(evdev_to_macos(106), Some(0x7C));
    }

    #[test]
    fn number_keys_map() {
        // 1 key: evdev 2
        assert!(evdev_to_macos(2).is_some());
        // 0 key: evdev 11
        assert!(evdev_to_macos(11).is_some());
    }

    #[test]
    fn round_trip() {
        for evdev_code in 1u16..128 {
            if let Some(mac) = evdev_to_macos(evdev_code) {
                if let Some(back) = macos_to_evdev(mac) {
                    assert_eq!(
                        back, evdev_code,
                        "round-trip failed: evdev {evdev_code} -> mac {mac} -> evdev {back}"
                    );
                }
            }
        }
    }

    #[test]
    fn unmapped_returns_none() {
        // Code 0 is EV_SYN, not a real key.
        assert_eq!(evdev_to_macos(0), None);
    }

    #[test]
    fn is_modifier_works() {
        assert!(is_modifier(KEY_LEFT_SHIFT));
        assert!(is_modifier(KEY_RIGHT_CTRL));
        assert!(!is_modifier(30)); // 'A' key
    }
}
