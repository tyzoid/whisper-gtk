//! Physical chord state, independent of X11 event delivery and key repeat.

use std::os::raw::c_char;
use std::sync::{Condvar, Mutex};

pub fn keycode_is_down(keymap: &[c_char; 32], keycode: u8) -> bool {
    keycode != 0 && (keymap[(keycode / 8) as usize] as u8 & (1 << (keycode % 8))) != 0
}

#[derive(Clone, Debug)]
pub struct HotkeyChord {
    pub keycode: u8,
    // One group per required modifier; either side satisfies a modifier, but
    // every physical key in each group must be up before output is safe.
    pub modifier_keycodes: Vec<Vec<u8>>,
}

impl HotkeyChord {
    pub fn is_down(&self, keymap: &[c_char; 32]) -> bool {
        keycode_is_down(keymap, self.keycode)
            && self
                .modifier_keycodes
                .iter()
                .all(|group| group.iter().any(|&code| keycode_is_down(keymap, code)))
    }

    fn is_released(&self, keymap: &[c_char; 32]) -> bool {
        !keycode_is_down(keymap, self.keycode)
            && self
                .modifier_keycodes
                .iter()
                .flatten()
                .all(|&code| !keycode_is_down(keymap, code))
    }
}

/// Shared by capture, transcription and output. A duration timeout may stop
/// capture, but must not bypass the physical release requirement.
#[derive(Default)]
pub struct HotkeyReleaseGate {
    held: Mutex<bool>,
    released: Condvar,
}

impl HotkeyReleaseGate {
    fn hold(&self) {
        *self.held.lock().unwrap() = true;
    }

    fn release(&self) {
        *self.held.lock().unwrap() = false;
        self.released.notify_all();
    }

    pub fn is_released(&self) -> bool {
        !*self.held.lock().unwrap()
    }

    pub fn wait_until_released(&self) {
        let _guard = self
            .released
            .wait_while(self.held.lock().unwrap(), |held| *held)
            .unwrap();
    }
}

#[derive(Default)]
pub struct HotkeyState {
    chord: Option<HotkeyChord>,
    recording: bool,
}

impl HotkeyState {
    pub fn press(&mut self, chord: HotkeyChord, gate: &HotkeyReleaseGate) -> bool {
        if self.chord.is_some() {
            return false;
        }
        gate.hold();
        self.chord = Some(chord);
        self.recording = true;
        true
    }

    pub fn stop_recording(&mut self) -> bool {
        std::mem::take(&mut self.recording)
    }

    /// Returns true once when capture should stop. Keep the original chord
    /// latched through partial releases, repeats, and configuration changes.
    pub fn update(&mut self, keymap: &[c_char; 32], gate: &HotkeyReleaseGate) -> bool {
        let Some(chord) = &self.chord else {
            return false;
        };
        let stop = self.recording && !chord.is_down(keymap);
        if stop {
            self.recording = false;
        }
        if chord.is_released(keymap) {
            self.chord = None;
            gate.release();
        }
        stop
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{mpsc, Arc};
    use std::time::Duration;

    const SPACE: u8 = 65;
    const CTRL_L: u8 = 37;
    const CTRL_R: u8 = 105;
    const ALT: u8 = 64;

    fn keymap(keys: &[u8]) -> [c_char; 32] {
        let mut result = [0; 32];
        for &code in keys {
            result[(code / 8) as usize] |= (1u8 << (code % 8)) as c_char;
        }
        result
    }

    fn chord() -> HotkeyChord {
        HotkeyChord {
            keycode: SPACE,
            modifier_keycodes: vec![vec![CTRL_L, CTRL_R]],
        }
    }

    #[test]
    fn waits_for_every_key_in_either_release_order() {
        for remaining in [SPACE, CTRL_L, CTRL_R] {
            let gate = HotkeyReleaseGate::default();
            let mut state = HotkeyState::default();
            assert!(state.press(chord(), &gate));
            assert!(state.update(&keymap(&[remaining]), &gate));
            assert!(!gate.is_released());
            assert!(!state.update(&keymap(&[remaining]), &gate));
            assert!(!state.update(&keymap(&[]), &gate));
            assert!(gate.is_released());
            assert!(state.press(chord(), &gate));
        }
    }

    #[test]
    fn repeats_and_repressed_trigger_do_not_rearm_partial_chord() {
        let gate = HotkeyReleaseGate::default();
        let mut state = HotkeyState::default();
        assert!(state.press(chord(), &gate));
        for _ in 0..5 {
            assert!(!state.press(chord(), &gate));
            assert!(!state.update(&keymap(&[CTRL_L, SPACE]), &gate));
        }
        assert!(state.update(&keymap(&[CTRL_L]), &gate));
        assert!(!state.press(chord(), &gate));
        assert!(!state.update(&keymap(&[CTRL_L, SPACE]), &gate));
        assert!(!state.update(&keymap(&[SPACE]), &gate));
        assert!(!gate.is_released());
        assert!(!state.update(&keymap(&[]), &gate));
        assert!(gate.is_released());
    }

    #[test]
    fn waits_for_both_modifier_sides_and_multiple_modifiers() {
        let gate = HotkeyReleaseGate::default();
        let mut state = HotkeyState::default();
        let mut chord = chord();
        chord.modifier_keycodes.push(vec![ALT]);
        assert!(chord.is_down(&keymap(&[SPACE, CTRL_R, ALT])));
        assert!(state.press(chord, &gate));
        assert!(state.update(&keymap(&[CTRL_L, CTRL_R, ALT]), &gate));
        for keys in [&[CTRL_R, ALT][..], &[ALT][..]] {
            assert!(!state.update(&keymap(keys), &gate));
            assert!(!gate.is_released());
        }
        assert!(!state.update(&keymap(&[]), &gate));
        assert!(gate.is_released());
    }

    #[test]
    fn every_release_order_of_a_multi_modifier_chord_is_safe() {
        let keys = [SPACE, CTRL_L, CTRL_R, ALT];
        for first in 0..4 {
            for second in 0..4 {
                for third in 0..4 {
                    for fourth in 0..4 {
                        let order = [first, second, third, fourth];
                        if (0..4).any(|index| order[..index].contains(&order[index])) {
                            continue;
                        }
                        let gate = HotkeyReleaseGate::default();
                        let mut state = HotkeyState::default();
                        let mut chord = chord();
                        chord.modifier_keycodes.push(vec![ALT]);
                        state.press(chord, &gate);
                        let mut held = keys.to_vec();
                        let mut stops = 0;
                        for (index, position) in order.iter().enumerate() {
                            held.retain(|code| *code != keys[*position]);
                            stops += usize::from(state.update(&keymap(&held), &gate));
                            assert_eq!(gate.is_released(), index == 3, "order: {order:?}");
                        }
                        assert_eq!(stops, 1);
                    }
                }
            }
        }
    }

    #[test]
    fn output_gate_closes_again_for_the_next_recording() {
        let gate = HotkeyReleaseGate::default();
        let mut state = HotkeyState::default();
        assert!(gate.is_released());
        state.press(chord(), &gate);
        assert!(!gate.is_released());
        assert!(state.update(&keymap(&[]), &gate));
        assert!(gate.is_released());
        state.press(chord(), &gate);
        // A late transcript from the previous capture must stay queued.
        assert!(!gate.is_released());
        state.update(&keymap(&[CTRL_L]), &gate);
        assert!(!gate.is_released());
        state.update(&keymap(&[]), &gate);
        assert!(gate.is_released());
    }

    #[test]
    fn single_key_and_unrelated_keys_do_not_delay_release() {
        let gate = HotkeyReleaseGate::default();
        let mut state = HotkeyState::default();
        let chord = HotkeyChord {
            keycode: SPACE,
            modifier_keycodes: vec![],
        };
        assert!(chord.is_down(&keymap(&[SPACE])));
        assert!(state.press(chord, &gate));
        assert!(!state.update(&keymap(&[SPACE, ALT]), &gate));
        assert!(state.update(&keymap(&[ALT]), &gate));
        assert!(gate.is_released());
    }

    #[test]
    fn changing_configuration_stops_capture_but_keeps_original_gate() {
        let gate = HotkeyReleaseGate::default();
        let mut state = HotkeyState::default();
        assert!(state.press(chord(), &gate));
        assert!(state.stop_recording());
        assert!(!state.stop_recording());
        assert!(!state.update(&keymap(&[CTRL_L]), &gate));
        let new_chord = HotkeyChord {
            keycode: ALT,
            modifier_keycodes: vec![],
        };
        assert!(!state.press(new_chord.clone(), &gate));
        assert!(!gate.is_released());
        assert!(!state.update(&keymap(&[ALT]), &gate));
        assert!(gate.is_released());
        assert!(state.press(new_chord, &gate));
    }

    #[test]
    fn timeout_worker_waits_until_physical_release() {
        let gate = Arc::new(HotkeyReleaseGate::default());
        let mut state = HotkeyState::default();
        state.press(chord(), &gate);
        let worker_gate = gate.clone();
        let (sender, receiver) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            worker_gate.wait_until_released();
            sender.send(()).unwrap();
        });
        assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
        state.update(&keymap(&[CTRL_L]), &gate);
        assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
        state.update(&keymap(&[]), &gate);
        receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn missing_modifier_mapping_cannot_activate_a_chord() {
        let chord = HotkeyChord {
            keycode: SPACE,
            modifier_keycodes: vec![vec![]],
        };
        assert!(!chord.is_down(&keymap(&[SPACE, CTRL_L])));
    }

    #[test]
    fn release_wakes_every_waiter_and_does_not_block_later_workers() {
        let gate = Arc::new(HotkeyReleaseGate::default());
        let mut state = HotkeyState::default();
        state.press(chord(), &gate);
        let (sender, receiver) = mpsc::channel();
        let workers: Vec<_> = (0..2)
            .map(|_| {
                let gate = gate.clone();
                let sender = sender.clone();
                std::thread::spawn(move || {
                    gate.wait_until_released();
                    sender.send(()).unwrap();
                })
            })
            .collect();
        assert!(receiver.recv_timeout(Duration::from_millis(20)).is_err());
        state.update(&keymap(&[]), &gate);
        for _ in 0..2 {
            receiver.recv_timeout(Duration::from_secs(1)).unwrap();
        }
        for worker in workers {
            worker.join().unwrap();
        }
        gate.wait_until_released();
    }

    #[test]
    fn unmapped_keycode_zero_is_never_down() {
        assert!(!keycode_is_down(&keymap(&[0]), 0));
        assert!(keycode_is_down(&keymap(&[255]), 255));
    }
}
