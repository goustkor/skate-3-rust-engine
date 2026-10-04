//! Lobby chat overlay: a bottom-left scrollback plus a single-line composer.
//!
//! Only wired into the authoritative session-server path. The server echoes the
//! sender's own line, so this module never appends locally; it renders whatever
//! the server relayed and keeps a bounded scrollback in [`Multiplayer`].
use super::{ChatInput, Multiplayer, CHAT_MAX_CHARS, CHAT_VISIBLE};
use bevy::prelude::*;

#[derive(Component)]
pub(super) struct ChatRoot;
#[derive(Component)]
pub(super) struct ChatLogText;
#[derive(Component)]
pub(super) struct ChatInputRow;
#[derive(Component)]
pub(super) struct ChatInputText;

pub(super) fn setup(mut commands: Commands) {
    commands
        .spawn((
            ChatRoot,
            GlobalZIndex(4),
            bevy::ui::FocusPolicy::Pass,
            Node {
                position_type: PositionType::Absolute,
                left: px(24),
                bottom: px(24),
                width: px(520),
                max_width: percent(60),
                flex_direction: FlexDirection::Column,
                padding: UiRect::all(px(10)),
                row_gap: px(4),
                display: Display::None,
                ..default()
            },
            BackgroundColor(Color::srgba(0.02, 0.035, 0.055, 0.62)),
        ))
        .with_children(|root| {
            root.spawn((
                ChatLogText,
                Text::new(""),
                TextFont {
                    font_size: 17.,
                    ..default()
                },
                TextColor(Color::srgba(0.95, 0.97, 1., 0.95)),
                Node::default(),
            ));
            root.spawn((
                ChatInputRow,
                Node {
                    display: Display::None,
                    width: percent(100),
                    padding: UiRect::axes(px(10), px(6)),
                    ..default()
                },
                BackgroundColor(Color::srgba(0.035, 0.065, 0.10, 0.9)),
            ))
            .with_children(|row| {
                row.spawn((
                    ChatInputText,
                    Text::new(""),
                    TextFont {
                        font_size: 18.,
                        ..default()
                    },
                    TextColor(Color::WHITE),
                ));
            });
        });
}

/// Reads the keyboard. Opening is gated on a closed menu/overlay so Enter keeps
/// its gameplay meaning while chat is hidden. While open, chat consumes the
/// keyboard for the frame so the menu does not react to the same key press.
pub(super) fn interact(
    keys: Res<ButtonInput<KeyCode>>,
    mut typing: MessageReader<bevy::input::keyboard::KeyboardInput>,
    menu: Option<Res<crate::graphics_menu::Menu>>,
    travel: Res<crate::teleport_menu::Travel>,
    customiser: Res<crate::customiser::Customiser>,
    custom_models: Res<crate::custom_models::CustomModels>,
    mods: Res<crate::modding::ModMenu>,
    mut chat: ResMut<ChatInput>,
    mut net: ResMut<Multiplayer>,
) {
    chat.handled = false;
    if !net.chat_available() {
        chat.open = false;
        chat.draft.clear();
        return;
    }

    let menu_open = menu.as_ref().is_some_and(|m| m.open);
    let overlay_open = menu_open
        || travel.open
        || travel.closed_this_frame
        || customiser.open
        || custom_models.open
        || mods.open;

    let events: Vec<_> = typing.read().collect();
    if chat.open {
        let mut close = false;
        for event in events {
            if !event.state.is_pressed() {
                continue;
            }
            match event.key_code {
                KeyCode::Escape => {
                    close = true;
                    break;
                }
                KeyCode::Enter => {
                    if !chat.draft.trim().is_empty() {
                        let draft = std::mem::take(&mut chat.draft);
                        net.send_chat(&draft);
                    }
                    close = true;
                    break;
                }
                KeyCode::Backspace => {
                    chat.draft.pop();
                }
                _ => {
                    if let Some(text) = &event.text {
                        for ch in text.chars() {
                            if ch.is_control() {
                                continue;
                            }
                            if chat.draft.chars().count() >= CHAT_MAX_CHARS {
                                break;
                            }
                            chat.draft.push(ch);
                        }
                    }
                }
            }
        }
        chat.handled = true;
        if close {
            chat.open = false;
            chat.draft.clear();
        }
    } else if !overlay_open && keys.just_pressed(KeyCode::Enter) {
        chat.open = true;
        chat.draft.clear();
        chat.handled = true;
    }
}

/// Keeps the full-screen menu closed while chat owns the keyboard.
///
/// `graphics_menu::interact` runs in `MenuInput` (after `chat::interact`) and
/// would otherwise toggle the menu on the same Escape that closes chat. This
/// runs after `MenuInput` and reconciles, so chat keeps exclusive focus.
pub(super) fn guard_menu(
    chat: Res<ChatInput>,
    mut menu: Option<ResMut<crate::graphics_menu::Menu>>,
) {
    if chat.open || chat.handled {
        if let Some(menu) = menu.as_mut() {
            menu.open = false;
        }
    }
}

pub(super) fn draw(
    net: Res<Multiplayer>,
    chat: Res<ChatInput>,
    windows: Query<&Window>,
    mut nodes: Query<
        (
            &mut Node,
            &mut BackgroundColor,
            Option<&ChatRoot>,
            Option<&ChatInputRow>,
        ),
        Or<(With<ChatRoot>, With<ChatInputRow>)>,
    >,
    mut labels: Query<
        (
            &mut Text,
            &mut TextFont,
            Option<&ChatLogText>,
            Option<&ChatInputText>,
        ),
        Or<(With<ChatLogText>, With<ChatInputText>)>,
    >,
) {
    let scale = windows
        .iter()
        .next()
        .map_or(1., |w| (w.height() / 1080.).clamp(0.65, 2.));

    let mut lines = Vec::new();
    if net.chat_available() {
        let recent: Vec<_> = net.chat_log().iter().rev().take(CHAT_VISIBLE).collect();
        for line in recent.into_iter().rev() {
            lines.push(format!(
                "{}: {}",
                net.chat_sender_name(line.actor),
                line.text
            ));
        }
    }
    let visible = chat.open || !lines.is_empty();

    for (mut node, mut background, root, row) in &mut nodes {
        if root.is_some() {
            node.display = if visible {
                Display::Flex
            } else {
                Display::None
            };
            node.left = px(24. * scale);
            node.bottom = px(24. * scale);
            node.width = px(520. * scale);
            node.padding = UiRect::all(px(10. * scale));
            node.row_gap = px(4. * scale);
            let alpha = if lines.is_empty() { 0.55 } else { 0.66 };
            background.0 = Color::srgba(0.02, 0.035, 0.055, alpha);
        } else if row.is_some() {
            node.display = if chat.open {
                Display::Flex
            } else {
                Display::None
            };
            node.padding = UiRect::axes(px(10. * scale), px(6. * scale));
        }
    }

    for (mut text, mut font, log, input) in &mut labels {
        if log.is_some() {
            font.font_size = 17. * scale;
            text.0 = lines.join("\n");
        } else if input.is_some() {
            font.font_size = 18. * scale;
            text.0 = if chat.open {
                format!("> {}_", chat.draft)
            } else {
                String::new()
            };
        }
    }
}
