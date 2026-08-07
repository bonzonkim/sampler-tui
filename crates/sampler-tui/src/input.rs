use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

pub const PAD_KEYS: [char; 16] = [
    '1', '2', '3', '4', 'q', 'w', 'e', 'r', 'a', 's', 'd', 'f', 'z', 'x', 'c', 'v',
];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct KeyboardCapabilities {
    pub release_events: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputAction {
    PadPress(usize),
    PadRelease(usize),
    PadStop(usize),
    BankDelta(i8),
    StopAll,
    Quit,
}

pub fn map_key(event: KeyEvent, capabilities: KeyboardCapabilities) -> Option<InputAction> {
    if event.kind == KeyEventKind::Repeat {
        return None;
    }

    let KeyCode::Char(character) = event.code else {
        return None;
    };
    let normalized = normalize_character(character);

    if event.kind == KeyEventKind::Press
        && control_only(event.modifiers)
        && matches!(normalized, 'c' | 'q')
    {
        return Some(InputAction::Quit);
    }

    if event.kind == KeyEventKind::Press && event.modifiers == KeyModifiers::NONE {
        match normalized {
            '[' => return Some(InputAction::BankDelta(-1)),
            ']' => return Some(InputAction::BankDelta(1)),
            _ => {}
        }
    }

    let pad_index = PAD_KEYS
        .iter()
        .position(|candidate| *candidate == normalized)?;
    match event.kind {
        KeyEventKind::Press if event.modifiers == KeyModifiers::NONE => {
            Some(InputAction::PadPress(pad_index))
        }
        KeyEventKind::Press if event.modifiers == KeyModifiers::SHIFT => {
            Some(InputAction::PadStop(pad_index))
        }
        KeyEventKind::Release
            if capabilities.release_events
                && matches!(event.modifiers, KeyModifiers::NONE | KeyModifiers::SHIFT) =>
        {
            Some(InputAction::PadRelease(pad_index))
        }
        _ => None,
    }
}

fn control_only(modifiers: KeyModifiers) -> bool {
    let allowed = KeyModifiers::CONTROL | KeyModifiers::SHIFT;
    modifiers.contains(KeyModifiers::CONTROL) && modifiers.difference(allowed).is_empty()
}

fn normalize_character(character: char) -> char {
    match character {
        '!' => '1',
        '@' => '2',
        '#' => '3',
        '$' => '4',
        character if character.is_ascii_uppercase() => character.to_ascii_lowercase(),
        character => character,
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

    use super::{InputAction, KeyboardCapabilities, map_key};

    const NONE: KeyModifiers = KeyModifiers::NONE;
    const SHIFT: KeyModifiers = KeyModifiers::SHIFT;
    const CONTROL: KeyModifiers = KeyModifiers::CONTROL;
    const PRESS: KeyEventKind = KeyEventKind::Press;
    const REPEAT: KeyEventKind = KeyEventKind::Repeat;
    const RELEASE: KeyEventKind = KeyEventKind::Release;

    fn key(character: char, modifiers: KeyModifiers, kind: KeyEventKind) -> KeyEvent {
        KeyEvent::new_with_kind(KeyCode::Char(character), modifiers, kind)
    }

    #[test]
    fn plain_q_triggers_pad_but_control_q_quits() {
        let caps = KeyboardCapabilities {
            release_events: true,
        };
        assert_eq!(
            map_key(key('q', NONE, PRESS), caps),
            Some(InputAction::PadPress(4))
        );
        assert_eq!(
            map_key(key('q', CONTROL, PRESS), caps),
            Some(InputAction::Quit)
        );
    }

    #[test]
    fn repeats_are_ignored_and_shifted_pads_stop() {
        let caps = KeyboardCapabilities {
            release_events: false,
        };
        assert_eq!(map_key(key('1', NONE, REPEAT), caps), None);
        assert_eq!(
            map_key(key('1', SHIFT, PRESS), caps),
            Some(InputAction::PadStop(0))
        );
    }

    #[test]
    fn release_events_are_emitted_only_when_supported() {
        let yes = KeyboardCapabilities {
            release_events: true,
        };
        let no = KeyboardCapabilities {
            release_events: false,
        };
        assert_eq!(
            map_key(key('z', NONE, RELEASE), yes),
            Some(InputAction::PadRelease(12))
        );
        assert_eq!(map_key(key('z', NONE, RELEASE), no), None);
    }

    #[test]
    fn shifted_character_forms_map_to_the_same_explicit_stop() {
        let caps = KeyboardCapabilities {
            release_events: false,
        };
        assert_eq!(
            map_key(key('Q', SHIFT, PRESS), caps),
            Some(InputAction::PadStop(4))
        );
        assert_eq!(
            map_key(key('!', SHIFT, PRESS), caps),
            Some(InputAction::PadStop(0))
        );
    }

    #[test]
    fn bank_keys_are_semantic_and_unrelated_control_chords_are_ignored() {
        let caps = KeyboardCapabilities {
            release_events: true,
        };
        assert_eq!(
            map_key(key('[', NONE, PRESS), caps),
            Some(InputAction::BankDelta(-1))
        );
        assert_eq!(
            map_key(key(']', NONE, PRESS), caps),
            Some(InputAction::BankDelta(1))
        );
        assert_eq!(
            map_key(key('c', CONTROL, PRESS), caps),
            Some(InputAction::Quit)
        );
        assert_eq!(map_key(key('w', CONTROL, PRESS), caps), None);
    }
}
