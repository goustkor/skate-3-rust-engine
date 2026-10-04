//! Platform input adapter; no animation or physics state mutation here.
use crate::app::SimulationSet;
use bevy::prelude::*;

pub(crate) mod capture;
mod controllers;
pub(crate) mod gesture_catalog;
pub(crate) mod gesture_input;
pub(crate) mod gesture_mapping;
mod gesture_mapping_data;
pub(crate) mod keyboard;
pub(crate) mod platform;
pub(crate) use capture::InputCapture;
pub(crate) use controllers::{ControllerInput, ControllerStatus, RawInput};
use keyboard::{poll_keyboard, KeyboardInput, MouseStick};
use skate_core::input::tick::TickInput;

#[derive(Resource, Clone, Copy, Debug)]
pub(crate) struct PublishedTickInput(pub TickInput);

impl Default for PublishedTickInput {
    fn default() -> Self {
        Self(TickInput::new(
            0,
            skate_core::input::gameplay_map::GameplayActions::from_values([0.0; 18]),
            false,
        ))
    }
}

pub(crate) struct InputPlugin;
impl Plugin for InputPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ControllerInput>()
            .init_resource::<KeyboardInput>()
            .init_resource::<MouseStick>()
            .init_resource::<InputCapture>()
            .init_resource::<PublishedTickInput>()
            .add_systems(PreUpdate, capture::apply_capture)
            .add_systems(
                PreUpdate,
                (poll_keyboard, poll_controllers)
                    .run_if(crate::graphics_menu::gameplay_active)
                    .after(capture::apply_capture),
            )
            .add_systems(FixedUpdate, publish_actions.in_set(SimulationSet::Input));
    }
}

pub(crate) fn poll_controllers(
    mut input: ResMut<ControllerInput>,
    config: Res<crate::config::Config>,
    net: Option<Res<crate::multiplayer::Multiplayer>>,
    windows: Query<&Window>,
    mut capabilities: Local<[platform::CapabilityCache; 4]>,
) {
    let previous = input.status;
    let focused = windows.iter().any(|w| w.focused);
    let active = net.is_some_and(|n| n.active());
    input.collect(std::array::from_fn(|slot| {
        if active
            && ((!focused && config.multiplayer.controller.is_none())
                || config
                    .multiplayer
                    .controller
                    .is_some_and(|selected| selected as usize != slot))
        {
            capabilities[slot].invalidate();
            Err(platform::DeviceError::Disconnected)
        } else {
            platform::poll_cached(slot, &mut capabilities[slot])
        }
    }));
    for (index, (&before, &after)) in previous.iter().zip(&input.status).enumerate() {
        if before != after {
            match after {
                ControllerStatus::Ready => info!("Controller {index}: raw XInput ready"),
                ControllerStatus::Unavailable(platform::DeviceError::Disconnected) => {
                    info!("Controller {index}: disconnected");
                }
                _ => warn!("Controller {index}: {after:?}"),
            }
        }
    }
}

pub(crate) fn publish_actions(
    mut input: ResMut<ControllerInput>,
    mut published: ResMut<PublishedTickInput>,
    mut keyboard: ResMut<KeyboardInput>,
    capture: Res<InputCapture>,
    menu: Option<Res<crate::graphics_menu::Menu>>,
    chat: Option<Res<crate::multiplayer::ChatInput>>,
    debug: Res<crate::debug_cam::DebugCam>,
    camera: Res<crate::camera::CameraRuntime>,
    mods: Option<Res<crate::modding::Mods>>,
) {
    let chat_open = chat.is_some_and(|c| c.open);
    let blocked = !crate::graphics_menu::gameplay_active(menu)
        || chat_open
        || debug.suppress_gameplay(&camera)
        // A released keyboard stops driving the skater; its events remain
        // readable by scripts and UI.
        || !capture.keyboard_captured();
    if blocked {
        input.discard_gameplay();
        // A key held while the menu/chat owns the keyboard must not leak into
        // the first resumed tick.
        keyboard.clear();
    }
    input.publish_actions();
    let tick = input.tick_input();
    let mut values = *tick.actions().values();
    if !blocked {
        // Keyboard/mouse are a second device: merge onto the pad packet before
        // mod overrides so a mod's explicit action still wins.
        keyboard.merge_into(&mut values);
        crate::modding::override_actions(mods.as_deref(), &mut values);
    }
    let tick = TickInput::new(
        tick.tick(),
        skate_core::input::gameplay_map::GameplayActions::from_values(values),
        tick.controller_available(),
    );
    published.0 = if debug.suppress_gameplay(&camera) {
        TickInput::new(
            tick.tick(),
            skate_core::input::gameplay_map::GameplayActions::from_values([0.0; 18]),
            tick.controller_available(),
        )
    } else {
        tick
    };
}
