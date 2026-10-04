//! Keyboard + mouse gameplay adapter. The stock simulation only reads the
//! XInput-derived action packet (`GameplayActions`, slots 64..81), so the
//! keyboard translates into those same 18 values and `publish_actions` merges
//! them with any connected pad.
//!
//! The native mapping of the face buttons is NOT A/B/X/Y in slot order. Stock
//! `8296D5F8` assigns XInput button bits 0x1000/0x2000/0x4000/0x8000 to action
//! slots 80/81/78/79 respectively, i.e. index 14 = X, 15 = Y, 16 = A, 17 = B.
//! `riding_intentions` then reads the packed bits as:
//!   bit23 (action 78 = X) and bit21 (action 80 = A) → push / accelerate
//!   bit20 (action 81 = B)                          → brake
//!   triggers 70/71                                 → crouch (charge ollie)
//!   bit28 (action 73 = RB)                         → world grab
//!
//! Camera look is deliberately NOT part of the action packet. In stock the
//! gameplay camera reads the right stick (`OB_LookAtX/Y`) — the same axis the
//! gesture recognizer uses for trick flicks, so a mouse on that axis performs
//! tricks. `MouseStick` instead carries the mouse as a dedicated camera-look
//! channel that the frame hands to the camera directly, leaving the right stick
//! (arrow keys here) exclusively for tricks.
//!
//! Layout (see `slot` for the native index each control feeds):
//!   WASD            left stick  → move / steer
//!   Mouse motion    camera look → dedicated channel, not an action
//!   Arrow keys      right stick → trick flicks
//!   LMB / RMB       LB / RB     → grabs
//!   Shift           A           → push / accelerate
//!   Space           X           → push / ollie
//!   Q               Y           → grab / tweak variant
//!   E               B           → brake
//!   Ctrl / Z        LT / RT     → crouch (hold to charge, release to ollie)
//!   V               left stick click
//!   C               right stick click
//!   T / G / F / H   D-pad up/down/left/right → emotes
use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;

/// Native gameplay action slots registered by `82697740` (values 64..81).
/// Kept as named indices so the merge in `publish_actions` cannot drift.
pub(crate) mod slot {
    pub(crate) const LEFT_X: usize = 0;
    pub(crate) const LEFT_Y: usize = 1;
    pub(crate) const LEFT_STICK_CLICK: usize = 2;
    pub(crate) const RIGHT_X: usize = 3;
    pub(crate) const RIGHT_Y: usize = 4;
    pub(crate) const RIGHT_STICK_CLICK: usize = 5;
    pub(crate) const LEFT_TRIGGER: usize = 6;
    pub(crate) const RIGHT_TRIGGER: usize = 7;
    pub(crate) const LEFT_BUMPER: usize = 8;
    pub(crate) const RIGHT_BUMPER: usize = 9;
    pub(crate) const DPAD_UP: usize = 10;
    pub(crate) const DPAD_DOWN: usize = 11;
    pub(crate) const DPAD_LEFT: usize = 12;
    pub(crate) const DPAD_RIGHT: usize = 13;
    /// XInput X face (0x4000 → action 78).
    pub(crate) const FACE_X: usize = 14;
    /// XInput Y face (0x8000 → action 79).
    pub(crate) const FACE_Y: usize = 15;
    /// XInput A face (0x1000 → action 80).
    pub(crate) const FACE_A: usize = 16;
    /// XInput B face (0x2000 → action 81).
    pub(crate) const FACE_B: usize = 17;
}

/// Mouse-to-look sensitivity, in normalized units per pixel.
const LOOK_PER_PIXEL: f32 = 0.02;
/// Look units shed per second while no mouse motion arrives. A quick decay
/// returns the camera toward center after a flick, matching the stock
/// free-camera stick offset rather than a persistent rotation.
const LOOK_DECAY_PER_SECOND: f32 = 3.0;

/// Keyboard contribution for the current fixed tick. The merge is skipped when
/// no control is active so the pad (if any) is left untouched.
#[derive(Resource, Default, Clone, Copy, Debug)]
pub(crate) struct KeyboardInput {
    values: [f32; 18],
    present: bool,
}

impl KeyboardInput {
    pub(crate) fn values(&self) -> [f32; 18] {
        self.values
    }

    pub(crate) fn present(&self) -> bool {
        self.present
    }

    /// Rebuild the 18-slot packet from the held keys and the two mouse buttons.
    pub(crate) fn collect(&mut self, keys: &ButtonInput<KeyCode>, mouse_buttons: (bool, bool)) {
        let axis = |positive: bool, negative: bool| f32::from(positive) - f32::from(negative);
        let button = |down: bool| f32::from(down);

        // Assign by named slot so the layout stays tied to `slot` and cannot
        // silently drift if the array literal is reordered.
        let mut values = [0.0; 18];
        values[slot::LEFT_X] = axis(keys.pressed(KeyCode::KeyD), keys.pressed(KeyCode::KeyA));
        values[slot::LEFT_Y] = axis(keys.pressed(KeyCode::KeyW), keys.pressed(KeyCode::KeyS));
        values[slot::LEFT_STICK_CLICK] = button(keys.pressed(KeyCode::KeyV));
        // Right stick carries trick flicks only; the camera look is a separate
        // channel (`MouseStick`) so the mouse never feeds the recognizer.
        values[slot::RIGHT_X] = axis(
            keys.pressed(KeyCode::ArrowRight),
            keys.pressed(KeyCode::ArrowLeft),
        );
        values[slot::RIGHT_Y] = axis(
            keys.pressed(KeyCode::ArrowUp),
            keys.pressed(KeyCode::ArrowDown),
        );
        values[slot::RIGHT_STICK_CLICK] = button(keys.pressed(KeyCode::KeyC));
        values[slot::LEFT_TRIGGER] =
            button(keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight));
        values[slot::RIGHT_TRIGGER] = button(keys.pressed(KeyCode::KeyZ));
        values[slot::LEFT_BUMPER] = button(mouse_buttons.0);
        values[slot::RIGHT_BUMPER] = button(mouse_buttons.1);
        values[slot::DPAD_UP] = button(keys.pressed(KeyCode::KeyT));
        values[slot::DPAD_DOWN] = button(keys.pressed(KeyCode::KeyG));
        values[slot::DPAD_LEFT] = button(keys.pressed(KeyCode::KeyF));
        values[slot::DPAD_RIGHT] = button(keys.pressed(KeyCode::KeyH));
        values[slot::FACE_X] = button(keys.pressed(KeyCode::Space));
        values[slot::FACE_Y] = button(keys.pressed(KeyCode::KeyQ));
        values[slot::FACE_A] =
            button(keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight));
        values[slot::FACE_B] = button(keys.pressed(KeyCode::KeyE));
        self.values = values;
        self.present = self.values.iter().any(|value| *value != 0.0);
    }

    pub(crate) fn clear(&mut self) {
        self.values = [0.0; 18];
        self.present = false;
    }

    /// Sum the keyboard packet onto an existing pad packet, saturating analog
    /// sticks at their native [-1, 1] range. Used by `publish_actions`.
    pub(crate) fn merge_into(&self, values: &mut [f32; 18]) {
        if !self.present() {
            return;
        }
        for (target, keyboard) in values.iter_mut().zip(self.values()) {
            if keyboard == 0.0 {
                continue;
            }
            *target = (*target + keyboard).clamp(-1.0, 1.0);
        }
    }
}

/// Mouse-driven camera look, held as a dedicated `[x, y]` offset and advanced
/// across host frames. The simulation hands this to the camera each tick; it is
/// never merged into the gameplay action packet.
#[derive(Resource, Default, Clone, Copy, Debug)]
pub(crate) struct MouseStick {
    pub(crate) right: [f32; 2],
}

impl MouseStick {
    /// Accumulate this frame's pixel delta, then decay toward center by `dt`.
    pub(crate) fn advance(&mut self, delta: Vec2, dt: f32) {
        let scaled = delta * LOOK_PER_PIXEL;
        self.right[0] = (self.right[0] + scaled.x).clamp(-1.0, 1.0);
        self.right[1] = (self.right[1] + scaled.y).clamp(-1.0, 1.0);
        let decay = (LOOK_DECAY_PER_SECOND * dt).min(1.0);
        self.right[0] -= self.right[0] * decay;
        self.right[1] -= self.right[1] * decay;
    }
}

/// Host-frame sampling of the keyboard and mouse, mirroring `poll_controllers`.
/// A released mouse stops driving the camera look; the pointer is free for UI.
pub(crate) fn poll_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    mouse: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    time: Res<Time<Real>>,
    capture: Res<super::InputCapture>,
    mut stick: ResMut<MouseStick>,
    mut keyboard: ResMut<KeyboardInput>,
) {
    if capture.mouse_captured() {
        let dt = time.delta_secs().min(0.1);
        stick.advance(motion.delta, dt);
    } else {
        // A free pointer must not rotate the camera; drop any retained look.
        *stick = MouseStick::default();
    }
    keyboard.collect(
        &keys,
        (
            mouse.pressed(MouseButton::Left),
            mouse.pressed(MouseButton::Right),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn keys(pressed: &[KeyCode]) -> ButtonInput<KeyCode> {
        let mut input = ButtonInput::default();
        for key in pressed {
            input.press(*key);
        }
        input
    }

    #[test]
    fn wasd_maps_to_left_stick_axes() {
        let mut keyboard = KeyboardInput::default();
        keyboard.collect(&keys(&[KeyCode::KeyW, KeyCode::KeyD]), (false, false));
        assert_eq!(keyboard.values()[slot::LEFT_X], 1.0);
        assert_eq!(keyboard.values()[slot::LEFT_Y], 1.0);
        keyboard.collect(&keys(&[KeyCode::KeyA, KeyCode::KeyS]), (false, false));
        assert_eq!(keyboard.values()[slot::LEFT_X], -1.0);
        assert_eq!(keyboard.values()[slot::LEFT_Y], -1.0);
    }

    #[test]
    fn opposing_axes_cancel() {
        let mut keyboard = KeyboardInput::default();
        keyboard.collect(&keys(&[KeyCode::KeyW, KeyCode::KeyS]), (false, false));
        assert_eq!(keyboard.values()[slot::LEFT_Y], 0.0);
        assert!(!keyboard.present());
    }

    #[test]
    fn arrow_keys_flick_right_stick_to_full_deflection() {
        let mut keyboard = KeyboardInput::default();
        keyboard.collect(
            &keys(&[KeyCode::ArrowRight, KeyCode::ArrowUp]),
            (false, false),
        );
        assert_eq!(keyboard.values()[slot::RIGHT_X], 1.0);
        assert_eq!(keyboard.values()[slot::RIGHT_Y], 1.0);
        keyboard.collect(
            &keys(&[KeyCode::ArrowLeft, KeyCode::ArrowDown]),
            (false, false),
        );
        assert_eq!(keyboard.values()[slot::RIGHT_X], -1.0);
        assert_eq!(keyboard.values()[slot::RIGHT_Y], -1.0);
    }

    #[test]
    fn mouse_never_enters_the_action_packet() {
        // The mouse look is a separate channel; the keyboard packet stays zero.
        let mut keyboard = KeyboardInput::default();
        keyboard.collect(&keys(&[]), (false, false));
        assert_eq!(keyboard.values()[slot::RIGHT_X], 0.0);
        assert_eq!(keyboard.values()[slot::RIGHT_Y], 0.0);
        assert!(!keyboard.present());
    }

    #[test]
    fn mouse_drives_camera_look_and_decays() {
        let mut stick = MouseStick::default();
        stick.advance(Vec2::new(50.0, -50.0), 0.0);
        assert!((stick.right[0] - 1.0).abs() < 1e-6);
        assert!((stick.right[1] + 1.0).abs() < 1e-6);
        stick.advance(Vec2::ZERO, 1.0);
        assert_eq!(stick.right, [0.0, 0.0]);
    }

    #[test]
    fn mouse_buttons_become_bumpers() {
        let mut keyboard = KeyboardInput::default();
        keyboard.collect(&keys(&[]), (true, true));
        assert_eq!(keyboard.values()[slot::LEFT_BUMPER], 1.0);
        assert_eq!(keyboard.values()[slot::RIGHT_BUMPER], 1.0);
    }

    #[test]
    fn shift_pushes_and_ctrl_crouches() {
        let mut keyboard = KeyboardInput::default();
        keyboard.collect(&keys(&[KeyCode::ShiftLeft]), (false, false));
        assert_eq!(keyboard.values()[slot::FACE_A], 1.0);
        assert_eq!(keyboard.values()[slot::LEFT_TRIGGER], 0.0);
        keyboard.collect(&keys(&[KeyCode::ControlLeft]), (false, false));
        assert_eq!(keyboard.values()[slot::LEFT_TRIGGER], 1.0);
        assert_eq!(keyboard.values()[slot::FACE_A], 0.0);
    }

    #[test]
    fn merge_saturates_without_touching_unused_slots() {
        let mut keyboard = KeyboardInput::default();
        keyboard.collect(&keys(&[KeyCode::KeyD, KeyCode::Space]), (false, false));
        let mut values = [0.0; 18];
        values[slot::LEFT_X] = 0.75;
        keyboard.merge_into(&mut values);
        assert_eq!(values[slot::LEFT_X], 1.0);
        assert_eq!(values[slot::FACE_X], 1.0);
        assert_eq!(values[slot::LEFT_Y], 0.0);
    }
}
