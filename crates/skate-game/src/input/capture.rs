//! Default-on mouse capture for gameplay. The cursor is hidden and locked
//! during play; ALT briefly releases it while held, scripts may release either
//! device through `sdk.input.capture`, and any cursor-driven UI (menu, chat,
//! replay, free camera) releases the mouse automatically so it can be clicked.
//!
//! A released device stops feeding the skater: the mouse no longer drives the
//! camera look and the keyboard no longer merges into the gameplay action
//! packet. Their events (`ButtonInput`, keyboard messages) stay available, so
//! scripts and UI still read keys and pointer normally.
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

#[derive(Resource)]
pub(crate) struct InputCapture {
    /// Script-requested capture, independent per device.
    pub mouse: bool,
    pub keyboard: bool,
    /// Resolved this frame after ALT and UI gating: what gameplay consumes.
    mouse_effective: bool,
}

impl Default for InputCapture {
    fn default() -> Self {
        Self {
            mouse: true,
            keyboard: true,
            mouse_effective: true,
        }
    }
}

impl InputCapture {
    /// Whether the mouse currently drives gameplay (camera look).
    pub(crate) fn mouse_captured(&self) -> bool {
        self.mouse_effective
    }

    /// Whether the keyboard currently drives gameplay (movement/actions).
    pub(crate) fn keyboard_captured(&self) -> bool {
        self.keyboard
    }

    /// Apply a script request. Absent fields are left unchanged.
    pub(crate) fn set(&mut self, mouse: Option<bool>, keyboard: Option<bool>) {
        if let Some(mouse) = mouse {
            self.mouse = mouse;
        }
        if let Some(keyboard) = keyboard {
            self.keyboard = keyboard;
        }
    }
}

/// Resolve capture each frame and mirror it into the window cursor. Runs
/// ungated so menus and overlays always regain a visible, clickable pointer.
#[allow(clippy::too_many_arguments)]
pub(crate) fn apply_capture(
    keys: Res<ButtonInput<KeyCode>>,
    mut capture: ResMut<InputCapture>,
    mut cursors: Query<&mut CursorOptions, With<PrimaryWindow>>,
    menu: Option<Res<crate::graphics_menu::Menu>>,
    chat: Option<Res<crate::multiplayer::ChatInput>>,
    debug: Res<crate::debug_cam::DebugCam>,
    camera: Res<crate::camera::CameraRuntime>,
    customiser: Option<Res<crate::customiser::Customiser>>,
    travel: Option<Res<crate::teleport_menu::Travel>>,
    mods: Option<Res<crate::modding::ModMenu>>,
    custom_models: Option<Res<crate::custom_models::CustomModels>>,
    replay: Res<crate::replay::Replay>,
) {
    // ALT is a momentary release: held shows the pointer, released recaptures.
    let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
    let ui_open = !crate::graphics_menu::gameplay_active(menu)
        || chat.is_some_and(|c| c.open)
        || debug.suppress_gameplay(&camera)
        || customiser.is_some_and(|c| c.open)
        || travel.is_some_and(|t| t.open)
        || mods.is_some_and(|m| m.open)
        || custom_models.is_some_and(|m| m.open)
        || replay.active;
    let mouse = capture.mouse && !alt && !ui_open;
    capture.mouse_effective = mouse;

    let (visible, grab_mode) = if mouse {
        (false, CursorGrabMode::Locked)
    } else {
        (true, CursorGrabMode::None)
    };
    for mut cursor in &mut cursors {
        if cursor.visible != visible || cursor.grab_mode != grab_mode {
            cursor.visible = visible;
            cursor.grab_mode = grab_mode;
        }
    }
}
