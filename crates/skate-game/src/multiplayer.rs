//! Transport-neutral ten-player free-skate; each player owns their simulation.
pub(crate) mod appearance;
mod appearance_transfer;
mod chat;
mod hud;
mod nametags;
mod render;
mod server_session;
mod transport;
use crate::{
    app::SimulationSet,
    physics::{network, GamePhysics, SkaterRuntime},
};
use bevy::prelude::*;
use skate_net::{
    directory::{Command as LobbyCommand, Event, Row},
    lobby::{Info, Session},
    packed::{self, BodyState, Packed},
};
use std::{
    collections::{BTreeMap, VecDeque},
    net::SocketAddr,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
pub(crate) fn unique() -> u64 {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    skate_net::hash(
        &[
            nanos.to_le_bytes().as_slice(),
            std::process::id().to_le_bytes().as_slice(),
        ]
        .concat(),
    )
}
#[derive(Default)]
pub(crate) struct Options {
    pub direct: Option<(SocketAddr, SocketAddr)>,
    pub host: Option<SocketAddr>,
    /// Authoritative session server address (`--net-server`).
    pub server: Option<SocketAddr>,
    pub session: u64,
    pub spawn_offset: f32,
    pub appearance: Option<String>,
    pub title: Option<String>,
    pub controller: Option<u32>,
}
struct VisualRoot {
    captured: f64,
    received: f64,
    pose: skate_net::Pose,
}
struct VisualPose {
    captured: f64,
    received: f64,
    bones: Vec<skate_net::Bone>,
}
struct Remote {
    roots: VecDeque<VisualRoot>,
    poses: VecDeque<VisualPose>,
    epoch: u64,
    visual_since: u64,
    body: BodyState,
    body_at: Instant,
    body_seq: u32,
    pose_seq: u32,
}
impl Remote {
    fn new(initial: BodyState) -> Self {
        Self {
            roots: VecDeque::new(),
            poses: VecDeque::new(),
            epoch: 0,
            visual_since: 0,
            body: initial,
            body_at: Instant::now(),
            body_seq: 0,
            pose_seq: 0,
        }
    }
    /// Folds one decoded body sample into the render buffer. `captured_ms` is
    /// the sender's clock, `received_ms` our session-relative arrival time.
    /// Returns false when the sample is stale.
    fn ingest_body(
        &mut self,
        seq: u32,
        captured_ms: u64,
        received_ms: u64,
        state: BodyState,
    ) -> bool {
        if seq <= self.body_seq {
            return false;
        }
        if self.roots.back().is_some_and(|p| {
            skate_net::prediction::is_discontinuity(
                &self.body,
                &state,
                (captured_ms as f64 / 1000. - p.captured) as f32,
            )
        }) {
            self.roots.clear();
            self.poses.clear();
            self.epoch += 1;
            self.visual_since = captured_ms;
        }
        self.roots.push_back(VisualRoot {
            captured: captured_ms as f64 / 1000.,
            received: received_ms as f64 / 1000.,
            pose: state.root,
        });
        while self.roots.len() > 64 {
            self.roots.pop_front();
        }
        self.body = state;
        self.body_at = Instant::now();
        self.body_seq = seq;
        true
    }
    /// Folds one decoded pose sample into the render buffer. `anchors` is the
    /// local bone set; a mismatch clears bones so the stock rig is used.
    fn ingest_pose(
        &mut self,
        seq: u32,
        captured_ms: u64,
        received_ms: u64,
        mut pose: skate_net::packed::PoseState,
        rig_matches: bool,
        anchors: &[usize],
    ) -> bool {
        if seq <= self.pose_seq {
            return false;
        }
        self.pose_seq = seq;
        if captured_ms < self.visual_since {
            return false;
        }
        if !rig_matches
            || pose
                .bones
                .iter()
                .map(|b| b.index as usize)
                .ne(anchors.iter().copied())
        {
            pose.bones.clear();
        }
        self.poses.push_back(VisualPose {
            captured: captured_ms as f64 / 1000.,
            received: received_ms as f64 / 1000.,
            bones: pose.bones,
        });
        while self.poses.len() > 64 {
            self.poses.pop_front();
        }
        true
    }
}
pub const NAME_KEY: &str = "mp:name";
const MAX_NAME: usize = 16;

/// Maximum runes in one chat line. Matches the server's `protocol.ChatMaxChars`.
pub(crate) const CHAT_MAX_CHARS: usize = 200;
/// Maximum retained chat lines in the local scrollback.
const CHAT_LOG_MAX: usize = 100;
/// Visible chat lines drawn by the overlay.
pub(crate) const CHAT_VISIBLE: usize = 8;

/// One chat line retained for the local scrollback.
#[derive(Clone, Debug)]
pub(crate) struct ChatLine {
    pub actor: u64,
    pub text: String,
}

/// Keyboard focus for the chat overlay. `open` drives both input capture and
/// the gameplay gate; `handled` marks a frame where chat consumed the keyboard
/// so the menu does not react to the same key press.
#[derive(Resource, Default)]
pub(crate) struct ChatInput {
    pub open: bool,
    pub draft: String,
    pub handled: bool,
}

#[derive(Component, Clone, Copy)]
pub(crate) struct NetworkActor(pub u64);

#[derive(Resource)]
pub(crate) struct Multiplayer {
    transport: Option<Box<dyn transport::Transport>>,
    lobby: Option<Session>,
    /// Authoritative session server connection. When present, it replaces the
    /// client-hosted `lobby` as the source of identity, roster and snapshots.
    server: Option<server_session::SkatedSession>,
    info: Info,
    schema: network::Schema,
    anchors: Vec<usize>,
    remotes: BTreeMap<u64, Remote>,
    pub status: String,
    pub join_code: String,
    pub host_code: String,
    last_body: Instant,
    last_pose: Instant,
    started: Instant,
    last_metrics: Instant,
    counts: (u64, u64),
    rates: String,
    provider_metrics: String,
    visual_status: String,
    loopback: bool,
    room: Option<(u64, u64)>,
    map_name: String,
    pub browser_rows: Vec<Row>,
    pub browser_page: usize,
    pub browser_total: usize,
    pub browser_status: String,
    pub player_name: String,
    name_path: std::path::PathBuf,
    names: BTreeMap<u64, String>,
    /// Local scrollback, oldest first.
    chat_log: VecDeque<ChatLine>,
    /// Highest chat seq seen per author, for ordering and duplicate rejection.
    last_chat_seq: BTreeMap<u64, u32>,
    /// A `dev:teleport` destination requested by the server via a slash command,
    /// consumed by `apply_dev_commands` which owns the skater runtime.
    pending_teleport: Option<[f32; 3]>,
}
impl Multiplayer {
    pub(crate) fn diagnostic_summary(&self) -> String {
        let provider = if self.server.is_some() {
            "session_server"
        } else if self.room.is_some() {
            "platform_relay"
        } else if self.transport.is_some() {
            "direct_local"
        } else {
            "inactive"
        };
        let rtt = if let Some(server) = &self.server {
            Some(server.rtt_ms)
        } else {
            self.lobby.as_ref().map(|lobby| lobby.stats.rtt_ms)
        };
        format!(
            "provider:{provider} active:{} remote_count:{} rtt_ms:{rtt:?}",
            self.active(),
            self.remotes.len()
        )
    }
    pub(crate) fn mod_identity(&self) -> (bool, u64, bool) {
        if let Some(server) = &self.server {
            // The server is the authority; no client is host.
            return (server.is_connected(), server.actor_id(), false);
        }
        self.lobby
            .as_ref()
            .map_or((false, 0, true), |l| (true, l.local, l.is_host()))
    }
    pub(crate) fn player_ids(&self) -> Vec<u64> {
        if let Some(server) = &self.server {
            let mut ids = vec![server.actor_id()];
            ids.extend(server.peers.keys().copied());
            ids.extend(self.remotes.keys().copied());
            ids.sort_unstable();
            ids.dedup();
            return ids;
        }
        let Some(lobby) = &self.lobby else {
            return Vec::new();
        };
        let mut ids = vec![lobby.local];
        ids.extend(lobby.actors.keys().copied());
        ids.extend(self.remotes.keys().copied());
        ids.sort_unstable();
        ids.dedup();
        ids
    }
    pub(crate) fn session_identity(&self) -> Option<(u64, u64, u64)> {
        if let Some(server) = &self.server {
            return server
                .is_connected()
                .then_some((server.lobby_id(), server.actor_id(), 0));
        }
        self.lobby
            .as_ref()
            .map(|l| (l.session, l.local, l.host_peer()))
    }
    pub(crate) fn host_actor(&self) -> u64 {
        // In server mode there is no client host; the server owns authority.
        if self.server.is_some() {
            return 0;
        }
        self.lobby
            .as_ref()
            .and_then(|l| l.host_actor())
            .unwrap_or(0)
    }
    /// Local actor id, valid in both lobby and server modes.
    pub(crate) fn local_actor(&self) -> u64 {
        if let Some(server) = &self.server {
            return server.actor_id();
        }
        self.lobby.as_ref().map_or(0, |l| l.local)
    }
    pub(crate) fn published_name(&self) -> String {
        sanitize_name(&self.player_name)
    }
    pub(crate) fn skater_name(&self, id: &str, local_id: &str) -> String {
        if id == local_id {
            return self.published_name();
        }
        id.parse::<u64>()
            .ok()
            .and_then(|peer| self.names.get(&peer).cloned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| id.to_owned())
    }
    pub(crate) fn actor_pose(&self, id: u64) -> Option<(Vec3, Quat)> {
        let remote = self.remotes.get(&id)?;
        let transform = Transform::from_matrix(network::matrix(remote.body.root));
        Some((transform.translation, transform.rotation.normalize()))
    }
    pub fn set_player_name(&mut self, name: String) {
        self.player_name = name.chars().take(MAX_NAME).collect();
        persist_player_name(&self.name_path, &self.player_name);
    }
    /// Chat is only wired into the authoritative session-server path.
    pub(crate) fn chat_available(&self) -> bool {
        self.server.is_some()
    }
    /// Sanitises, length-limits and sends a chat line. The server echoes it
    /// back with the authoritative actor and ordering, so nothing is appended
    /// locally here. Returns false when the line is empty or chat is offline.
    pub(crate) fn send_chat(&mut self, text: &str) -> bool {
        let text = sanitize_chat(text);
        if text.is_empty() {
            return false;
        }
        let Some(server) = &mut self.server else {
            return false;
        };
        if !server.is_connected() {
            info!("CHAT_DROP: session not connected");
            return false;
        }
        info!("CHAT_SEND chars={}", text.chars().count());
        server.publish_chat(text);
        true
    }
    /// The retained chat scrollback, oldest first.
    pub(crate) fn chat_log(&self) -> &VecDeque<ChatLine> {
        &self.chat_log
    }
    /// Display name for an actor, falling back to the local or generic name.
    pub(crate) fn chat_sender_name(&self, actor: u64) -> String {
        if actor == SERVER_CHAT_ACTOR {
            return "server".into();
        }
        if actor == self.local_actor() {
            return self.published_name();
        }
        self.names
            .get(&actor)
            .cloned()
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "Player".into())
    }
    pub(crate) fn publish_application(&mut self, key: &str, value: Vec<u8>) -> bool {
        if let Some(server) = &mut self.server {
            server.publish_application(key, value);
            return true;
        }
        let now = self.started.elapsed().as_millis() as u64;
        self.lobby
            .as_mut()
            .is_some_and(|l| l.publish_application(key, value, now))
    }
    pub(crate) fn application_records(&self) -> Vec<(u64, String, u32, Vec<u8>)> {
        if let Some(server) = &self.server {
            return server
                .applications
                .iter()
                .map(|((actor, key), (seq, value))| (*actor, key.clone(), *seq, value.clone()))
                .collect();
        }
        self.lobby.as_ref().map_or_else(Vec::new, |l| {
            l.actors
                .iter()
                .filter(|(id, _)| **id != l.local)
                .flat_map(|(&id, actor)| {
                    actor
                        .application
                        .iter()
                        .map(move |(key, r)| (id, key.clone(), r.seq, r.value.clone()))
                })
                .collect()
        })
    }
    pub fn active(&self) -> bool {
        self.lobby.is_some() || self.server.is_some()
    }
    pub fn leave(&mut self) {
        if let Some(server) = &mut self.server {
            server.goodbye();
        }
        if let (Some(lobby), Some(t)) = (&self.lobby, &mut self.transport) {
            for p in lobby.goodbye() {
                let _ = t.send(p.peer, &p.data);
            }
        }
        self.transport = None;
        self.lobby = None;
        self.server = None;
        self.remotes.clear();
        self.names.clear();
        self.host_code.clear();
        self.room = None;
        self.status = "Offline. Steam is only needed for Steam multiplayer.".into();
    }
    fn start(&mut self, transport: Box<dyn transport::Transport>, session: u64, host: Option<u64>) {
        self.leave();
        self.info.id = unique();
        self.loopback = transport.loopback();
        let mut lobby = Session::new(session, self.info, host);
        lobby.set_loopback(self.loopback);
        self.lobby = Some(lobby);
        self.transport = Some(transport);
        self.started = Instant::now();
        self.last_metrics = Instant::now();
        self.counts = (0, 0);
        self.rates.clear();
        self.provider_metrics.clear();
        self.visual_status.clear();
        self.status = "Waiting for players... (up to 10)".into();
    }
    pub fn local(&mut self, host: bool) {
        let bind = if host {
            "127.0.0.1:31030"
        } else {
            "127.0.0.1:0"
        };
        match transport::Direct::new(bind.parse().unwrap()) {
            Ok(t) => self.start(
                Box::new(t),
                480_31030,
                if host {
                    None
                } else {
                    Some(transport::endpoint("127.0.0.1:31030".parse().unwrap()).unwrap())
                },
            ),
            Err(e) => self.status = format!("Could not open local session: {e}"),
        }
    }
    /// Connects to the authoritative session server. This replaces the
    /// client-hosted lobby: identity, roster and snapshot relay come from the
    /// server, and every gameplay datagram is exchanged through it.
    pub fn session_server(&mut self, addr: std::net::SocketAddr) {
        self.leave();
        // A fresh actor id keeps this client distinct across reconnects.
        let id = unique();
        let info = server_session::client_info(
            id,
            self.info.map,
            self.info.rig,
            self.info.physics,
            self.info.appearance,
        );
        let name = self.published_name();
        match server_session::SkatedSession::connect(addr, info, name) {
            Ok(session) => {
                self.started = Instant::now();
                self.last_metrics = Instant::now();
                self.counts = (0, 0);
                self.rates.clear();
                self.provider_metrics.clear();
                self.visual_status.clear();
                self.status = format!("Contacting session server at {addr}...");
                self.server = Some(session);
            }
            Err(e) => {
                self.status = format!("Could not open session socket: {e}");
            }
        }
    }
    fn lobby_command(&mut self, command: LobbyCommand) {
        // Retry discovery after Steam was opened following an initialization failure.
        if !self.active()
            && self.transport.as_ref().is_some_and(|t| {
                let status = t.status();
                status.starts_with("ERROR")
                    || status.contains("stopped")
                    || status.contains("did not respond")
            })
        {
            self.transport = None;
        }
        if self.transport.is_none() {
            match transport::Steam::new(0, 0) {
                Ok(t) => self.transport = Some(Box::new(t)),
                Err(e) => {
                    self.status = e.clone();
                    self.browser_status = e;
                    return;
                }
            }
        }
        let result = self.transport.as_mut().unwrap().command(command);
        self.browser_status = match result {
            Ok(()) => "Contacting Steam...".into(),
            Err(e) => e,
        };
        if !self.active() {
            self.status = self.browser_status.clone();
        }
    }
    pub fn browse(&mut self, page: usize) {
        self.lobby_command(LobbyCommand::Browse {
            page,
            map: self.info.map,
            physics: self.info.physics,
        });
    }
    pub fn join_row(&mut self, index: usize) {
        let Some(row) = self.browser_rows.get(index) else {
            return;
        };
        if row.players >= row.capacity {
            self.browser_status = "Lobby is full. Refresh to check for a free slot.".into();
            return;
        }
        let lobby = row.id;
        self.leave();
        self.lobby_command(LobbyCommand::Join {
            lobby,
            map: self.info.map,
            physics: self.info.physics,
        });
    }
    pub fn steam(&mut self, host: bool) {
        if host {
            self.leave();
            self.lobby_command(LobbyCommand::Host {
                map: self.info.map,
                physics: self.info.physics,
                label: self.map_name.clone(),
            });
            return;
        }
        if let Ok(lobby) = self.join_code.trim().parse::<u64>() {
            if lobby != 0 {
                self.leave();
                self.lobby_command(LobbyCommand::Join {
                    lobby,
                    map: self.info.map,
                    physics: self.info.physics,
                });
                return;
            }
        }
        let (peer, session) = {
            let Some((id, key)) = self.join_code.trim().split_once('-') else {
                self.status = "Enter a lobby code, or select a lobby in the browser.".into();
                return;
            };
            match (id.parse::<u64>(), u64::from_str_radix(key, 16)) {
                (Ok(id), Ok(key)) if id != 0 && key != 0 => (id, key),
                _ => {
                    self.status = "Invalid join code. Expected SteamID-session.".into();
                    return;
                }
            }
        };
        match transport::Steam::new(peer, session) {
            Ok(t) => self.start(Box::new(t), session, (peer != 0).then_some(peer)),
            Err(e) => self.status = e,
        }
    }
}
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct RemoteRenderSet;

pub(crate) struct MultiplayerPlugin;
impl Plugin for MultiplayerPlugin {
    fn build(&self, app: &mut App) {
        let config = app.world().resource::<crate::config::Config>();
        let skater = app.world().resource::<SkaterRuntime>();
        let physics = app.world().resource::<GamePhysics>();
        let rig = skate_net::hash(
            format!(
                "{:?}{:?}",
                skater.animation.evaluator.frames.bone_names,
                skater.animation.evaluator.frames.parents
            )
            .as_bytes(),
        );
        let schema =
            network::Schema::new(physics, skater).expect("Default multiplayer collision schema");
        let mut net = Multiplayer {
            transport: None,
            lobby: None,
            server: None,
            info: Info {
                id: unique(),
                map: config.map_fingerprint,
                rig,
                physics: schema.fingerprint,
                appearance: skate_net::hash(
                    config
                        .multiplayer
                        .appearance
                        .as_deref()
                        .unwrap_or(skate_net::DEFAULT_APPEARANCE)
                        .as_bytes(),
                ),
            },
            schema,
            anchors: network::anchors(skater),
            remotes: BTreeMap::new(),
            status: "Offline. Steam is only needed for Steam multiplayer.".into(),
            join_code: String::new(),
            host_code: String::new(),
            last_body: Instant::now(),
            last_pose: Instant::now(),
            started: Instant::now(),
            last_metrics: Instant::now(),
            counts: (0, 0),
            rates: String::new(),
            provider_metrics: String::new(),
            visual_status: String::new(),
            loopback: false,
            room: None,
            map_name: config
                .map_path
                .as_ref()
                .and_then(|p| p.file_stem())
                .map(|n| skate_net::directory::label(&n.to_string_lossy()))
                .unwrap_or_else(|| "Test world".into()),
            browser_rows: vec![],
            browser_page: 0,
            browser_total: 0,
            browser_status: String::new(),
            name_path: player_name_path(&config.asset_root),
            player_name: config
                .multiplayer
                .title
                .clone()
                .filter(|name| !name.trim().is_empty())
                .unwrap_or_else(|| load_player_name(&player_name_path(&config.asset_root))),
            names: BTreeMap::new(),
            chat_log: VecDeque::new(),
            last_chat_seq: BTreeMap::new(),
            pending_teleport: None,
        };
        if let Some(server) = config.multiplayer.server {
            // The authoritative session server replaces any local or Steam path.
            net.session_server(server);
        } else if let Some(bind) = config
            .multiplayer
            .host
            .or(config.multiplayer.direct.map(|(b, _)| b))
        {
            let target = config
                .multiplayer
                .direct
                .map(|(_, p)| transport::endpoint(p).expect("IPv4 peer"));
            match transport::Direct::new(bind) {
                Ok(t) => net.start(Box::new(t), config.multiplayer.session, target),
                Err(e) => net.status = format!("Local multiplayer could not start: {e}"),
            }
        }
        app.insert_resource(net)
            .init_resource::<ChatInput>()
            .add_systems(
                PreUpdate,
                (world_changed, receive, sync_names)
                    .chain()
                    .after(crate::map_transition::MapTransitionSet),
            )
            .add_systems(
                PreUpdate,
                chat::interact
                    .after(bevy::input::InputSystems)
                    .before(crate::graphics_menu::MenuInput),
            )
            .add_systems(
                PreUpdate,
                chat::guard_menu.after(crate::graphics_menu::MenuInput),
            )
            .add_systems(Startup, (hud::setup, chat::setup))
            .add_systems(Update, (hud::draw, chat::draw))
            .add_systems(Update, send_pose)
            .add_systems(
                FixedUpdate,
                apply_dev_commands.before(SimulationSet::Physics),
            )
            .add_systems(
                Update,
                nametags::draw
                    .after(RemoteRenderSet)
                    .after(crate::modding::ModCameraSet),
            )
            .add_systems(
                FixedUpdate,
                prepare
                    .before(SimulationSet::Physics)
                    .after(SimulationSet::Controls),
            )
            .add_systems(FixedUpdate, send.after(SimulationSet::Physics))
            .init_resource::<appearance::Appearances>()
            .add_systems(Last, appearance::cleanup)
            .add_systems(Update, appearance::sync.before(render::spawn))
            .add_plugins(render::RemoteRenderPlugin);
    }
}
// A world swap replaces the local physics assembly. Never retain lobby identity
// or remote collision state from the previous map, including pending Steam joins.
fn world_changed(
    mut changed: MessageReader<crate::map_transition::WorldChanged>,
    config: Res<crate::config::Config>,
    physics: Res<GamePhysics>,
    skater: Res<SkaterRuntime>,
    mut net: ResMut<Multiplayer>,
) {
    if changed.read().count() == 0 {
        return;
    }
    net.leave();
    net.info.map = config.map_fingerprint;
    net.map_name = config
        .map_path
        .as_ref()
        .and_then(|p| p.file_stem())
        .map(|n| skate_net::directory::label(&n.to_string_lossy()))
        .unwrap_or_else(|| "Test world".into());
    if let Ok(schema) = network::Schema::new(&physics, &skater) {
        net.info.physics = schema.fingerprint;
        net.schema = schema;
    }
    net.anchors = network::anchors(&skater);
    net.browser_rows.clear();
    net.browser_page = 0;
    net.browser_total = 0;
    net.browser_status.clear();
}
fn receive(mut net: ResMut<Multiplayer>) {
    let now = net.started.elapsed().as_millis() as u64;
    if net.server.is_some() {
        receive_from_server(&mut net, now);
        return;
    }
    let net = &mut *net;
    let Some(t) = &mut net.transport else {
        return;
    };
    let packets = match t.receive() {
        Ok(p) => p,
        Err(e) => {
            net.status = format!("Connection error: {e}");
            return;
        }
    };
    let provider = t.status();
    for response in t.events() {
        match response.event {
            Event::Rows { page, total, rows } => {
                net.browser_rows = rows;
                net.browser_page = page;
                net.browser_total = total;
                net.browser_status = if total == 0 {
                    "No public lobbies found. Host one or refresh.".into()
                } else {
                    format!(
                        "{} lobbies | Page {} | select a row to join",
                        total,
                        page + 1
                    )
                };
            }
            Event::Error(e) => {
                net.browser_status = e.clone();
                if net.lobby.is_none() {
                    net.status = e;
                }
            }
            Event::Owner {
                lobby: room,
                owner,
                own,
            } => {
                if net.room != Some((room, owner)) {
                    let host = (owner != own).then_some(owner);
                    if net.room.is_some_and(|(id, _)| id == room) {
                        if let Some(session) = &mut net.lobby {
                            session.migrate(host, now);
                        }
                        info!(
                            "MULTIPLAYER_HOST_MIGRATED lobby={room} owner={owner} local_host={}",
                            host.is_none()
                        );
                    } else {
                        net.info.id = unique();
                        net.lobby = Some(Session::new(room, net.info, host));
                        net.counts = (0, 0);
                        net.last_metrics = Instant::now();
                    }
                    net.remotes.clear();
                    net.loopback = false;
                    net.room = Some((room, owner));
                    net.host_code = room.to_string();
                    net.browser_status = if host.is_none() {
                        "Hosting public lobby. Other players can join from the browser.".into()
                    } else {
                        "Lobby joined; connecting to host...".into()
                    };
                }
            }
        }
    }
    let Some(lobby) = &mut net.lobby else {
        if provider.starts_with("ERROR")
            || provider.contains("stopped")
            || provider.contains("did not respond")
        {
            net.browser_status = provider.clone();
            net.status = provider;
        }
        return;
    };
    if net.room.is_none() && lobby.is_host() {
        if let Some(id) = provider.strip_prefix("READY ") {
            net.host_code = format!("{}-{:016x}", id, lobby.session);
        }
    }
    lobby.set_congested(t.congested());
    net.provider_metrics = t.metrics();
    for (peer, p) in packets {
        lobby.receive(peer, &p, now);
    }
    for p in lobby.service(now) {
        let success = t.send(p.peer, &p.data).is_ok();
        lobby.record_send(p.data.len(), success);
    }
    net.remotes.retain(|id, _| lobby.actors.contains_key(id));
    for (&id, actor) in &lobby.actors {
        if id == lobby.local {
            continue;
        }
        let Some(body) = actor.body.latest() else {
            continue;
        };
        if now.saturating_sub(body.received) > 3500 {
            net.remotes.remove(&id);
            continue;
        }
        if let std::collections::btree_map::Entry::Vacant(entry) = net.remotes.entry(id) {
            let Some(initial) = body.state.unpack_body() else {
                continue;
            };
            let fallback = actor.info.rig != net.info.rig;
            info!("MULTIPLAYER_CONNECTED peer={id} fallback={fallback}");
            entry.insert(Remote {
                roots: VecDeque::new(),
                poses: VecDeque::new(),
                epoch: 0,
                visual_since: 0,
                body: initial,
                body_at: Instant::now(),
                body_seq: 0,
                pose_seq: 0,
            });
        }
        let remote = net.remotes.get_mut(&id).unwrap();
        // Consume every newly decoded source sample, including several delivered in one frame.
        for revision in &actor.body.history {
            if revision.seq <= remote.body_seq {
                continue;
            }
            let Some(state) = revision.state.unpack_body() else {
                continue;
            };
            if remote.roots.back().is_some_and(|p| {
                skate_net::prediction::is_discontinuity(
                    &remote.body,
                    &state,
                    (revision.state.captured as f64 / 1000. - p.captured) as f32,
                )
            }) {
                remote.roots.clear();
                remote.poses.clear();
                remote.epoch += 1;
                remote.visual_since = revision.state.captured;
            }
            remote.roots.push_back(VisualRoot {
                captured: revision.state.captured as f64 / 1000.,
                received: revision.received as f64 / 1000.,
                pose: state.root,
            });
            while remote.roots.len() > 64 {
                remote.roots.pop_front();
            }
            remote.body = state;
            remote.body_at =
                Instant::now() - Duration::from_millis(now.saturating_sub(revision.received));
            remote.body_seq = revision.seq;
        }
        for revision in &actor.pose.history {
            if revision.seq <= remote.pose_seq {
                continue;
            }
            remote.pose_seq = revision.seq;
            if revision.state.captured < remote.visual_since {
                continue;
            }
            let Some(mut pose) = revision.state.unpack_pose() else {
                continue;
            };
            if actor.info.rig != net.info.rig
                || pose
                    .bones
                    .iter()
                    .map(|b| b.index as usize)
                    .ne(net.anchors.iter().copied())
            {
                pose.bones.clear();
            }
            remote.poses.push_back(VisualPose {
                captured: revision.state.captured as f64 / 1000.,
                received: revision.received as f64 / 1000.,
                bones: pose.bones,
            });
            while remote.poses.len() > 64 {
                remote.poses.pop_front();
            }
        }
    }
    net.status = if !lobby.notice.is_empty() {
        lobby.notice.clone()
    } else if lobby.actors.len() > 1 {
        format!(
            "Connected: {}/10 players | collisions on | synced characters",
            lobby.actors.len()
        )
    } else {
        format!("{} | waiting for players (1/10)", provider)
    };
    if net.last_metrics.elapsed() >= Duration::from_secs(1) {
        let dt = net.last_metrics.elapsed().as_secs_f64();
        let s = &lobby.stats;
        net.rates = format!(
            "App up {:.1} / down {:.1} kB/s | RTT {} ms | stale {} | delta misses {} | send errors {}",
            (s.tx_bytes - net.counts.0) as f64 / dt / 1000.,
            (s.rx_bytes - net.counts.1) as f64 / dt / 1000.,
            s.rtt_ms,
            s.late,
            s.baseline_miss,
            s.send_errors
        );
        info!(
            "MULTIPLAYER_STATS players={} {} budget_skips={} {}",
            lobby.actors.len(),
            net.rates,
            s.budget_skips,
            net.provider_metrics
        );
        net.counts = (s.tx_bytes, s.rx_bytes);
        net.last_metrics = Instant::now();
    }
}
/// Server-backed receive path. The server is the hub: it assigns identity, owns
/// the roster and relays peers' full snapshots. There is no client host, no ACK
/// and no per-peer scheduling here.
fn receive_from_server(net: &mut Multiplayer, now: u64) {
    let info = {
        let server = net.server.as_ref().unwrap();
        server_session::client_info(
            server.actor_id(),
            net.info.map,
            net.info.rig,
            net.info.physics,
            net.info.appearance,
        )
    };
    let name = net.published_name();

    let polled = {
        let server = net.server.as_mut().unwrap();
        server.poll(&info, &name)
    };

    let local = net.server.as_ref().map_or(0, |s| s.actor_id());
    let connected = net.server.as_ref().is_some_and(|s| s.is_connected());

    // Admit any peer that appeared in the roster, then fold its samples.
    for (&actor, _) in net.server.as_ref().unwrap().peers.iter() {
        if net.remotes.contains_key(&actor) {
            continue;
        }
        // We need an initial body to seed the collision shape; the first
        // decoded body snapshot below will fill it.
        if let Some((_, _, _, state)) = polled.bodies.iter().find(|(a, _, _, _)| *a == actor) {
            let fallback = net
                .server
                .as_ref()
                .unwrap()
                .peers
                .get(&actor)
                .map_or(false, |p| p.rig != net.info.rig);
            info!("MULTIPLAYER_CONNECTED peer={actor} fallback={fallback}");
            net.remotes.insert(actor, Remote::new(state.clone()));
        }
    }
    // A body snapshot may arrive before the roster entry: accept either order.
    for (actor, _, _, state) in &polled.bodies {
        if !net.remotes.contains_key(actor) {
            info!("MULTIPLAYER_CONNECTED peer={actor} fallback=false");
            net.remotes.insert(*actor, Remote::new(state.clone()));
        }
    }

    for (actor, seq, captured_ms, state) in polled.bodies.clone() {
        if let Some(remote) = net.remotes.get_mut(&actor) {
            remote.ingest_body(seq, captured_ms, now, state);
        }
    }
    for (actor, seq, captured_ms, pose) in polled.poses {
        if actor == local {
            continue;
        }
        let rig_matches = net
            .server
            .as_ref()
            .and_then(|s| s.peers.get(&actor))
            .is_none_or(|p| p.rig == net.info.rig);
        let anchors = net.anchors.clone();
        if let Some(remote) = net.remotes.get_mut(&actor) {
            remote.ingest_pose(seq, captured_ms, now, pose, rig_matches, &anchors);
        }
    }

    // Fold chat lines into the local scrollback, rejecting stale/duplicate
    // sequences and keeping a bounded history.
    for (actor, seq, text) in polled.chats {
        let last = net.last_chat_seq.get(&actor).copied().unwrap_or(0);
        if seq <= last {
            continue;
        }
        info!("CHAT_RECV actor={actor} seq={seq} text={text:?}");
        net.last_chat_seq.insert(actor, seq);
        net.chat_log.push_back(ChatLine { actor, text });
        while net.chat_log.len() > CHAT_LOG_MAX {
            net.chat_log.pop_front();
        }
    }

    // Resource events are the extensible script/mod plane. Chat is handled
    // above; the two events the server produces for slash commands are folded
    // into the local view here:
    //   - `command:result` (resource "command") is rendered in the chat
    //     scrollback so a player sees the outcome of /coords, /tp, /help, …;
    //   - `dev:teleport` (resource "dev") requests a teleport, staged in
    //     `pending_teleport` and applied by `apply_dev_commands`, which is the
    //     only system holding the skater runtime.
    for event in &polled.events {
        match (event.resource.as_str(), event.name.as_str()) {
            ("chat", _) => {}
            ("command", "result") => fold_command_result(net, &event.payload),
            ("dev", "teleport") => fold_dev_teleport(net, &event.payload),
            _ => {
                info!(
                    "NETWORK_EVENT actor={} resource={} name={} bytes={}",
                    event.actor,
                    event.resource,
                    event.name,
                    event.payload.len()
                );
            }
        }
    }

    // Drop remotes no longer in the roster, and stall out silent ones.
    let roster: std::collections::BTreeSet<u64> =
        net.server.as_ref().unwrap().peers.keys().copied().collect();
    net.remotes.retain(|id, remote| {
        if *id == local || !roster.contains(id) {
            return false;
        }
        remote.body_at.elapsed() < Duration::from_millis(3500)
    });

    // Populate display names from application records.
    for (peer, key, _, bytes) in net.application_records() {
        if key != NAME_KEY {
            continue;
        }
        if let Ok(raw) = String::from_utf8(bytes) {
            net.names.insert(peer, sanitize_name(&raw));
        }
    }
    let live: std::collections::BTreeSet<u64> = net.player_ids().into_iter().collect();
    net.names.retain(|id, _| live.contains(id));

    // Status line.
    let peer_count = net.server.as_ref().map_or(0, |s| s.peer_count());
    let notice = net
        .server
        .as_ref()
        .map(|s| s.notice().to_owned())
        .unwrap_or_default();
    net.status = if !connected {
        if notice.is_empty() {
            "Connecting to session server...".into()
        } else {
            notice
        }
    } else if peer_count > 0 {
        format!(
            "Session server: {} players | collisions on | synced characters",
            peer_count + 1
        )
    } else {
        "Session server connected | waiting for players (1/10)".into()
    };

    if net.last_metrics.elapsed() >= Duration::from_secs(1) {
        let rtt = net.server.as_ref().map_or(0, |s| s.rtt_ms);
        net.rates = format!(
            "Session server | RTT {rtt} ms | remotes {}",
            net.remotes.len()
        );
        info!("MULTIPLAYER_STATS server_peers={peer_count} {}", net.rates);
        net.last_metrics = Instant::now();
    }
}

/// Sentinel chat author for server-generated lines (command results). It is not
/// a real actor id, so it never collides with a player.
const SERVER_CHAT_ACTOR: u64 = 0;

/// Folds a `command:result` payload into the local scrollback so the player sees
/// the outcome of a slash command. The payload is the JSON the server emits:
/// `{"command","ok","message","usage","result"}`.
fn fold_command_result(net: &mut Multiplayer, payload: &[u8]) {
    let value: serde_json::Value = match serde_json::from_slice(payload) {
        Ok(v) => v,
        Err(_) => {
            info!(
                "command result: undecodable payload ({} bytes)",
                payload.len()
            );
            return;
        }
    };

    push_server_chat(net, format_command_result(&value));
}

/// Renders a decoded `command:result` payload as one chat line.
fn format_command_result(value: &serde_json::Value) -> String {
    let command = value.get("command").and_then(|v| v.as_str()).unwrap_or("");
    let ok = value.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);

    let mut text = if command.is_empty() {
        "/?".to_string()
    } else {
        format!("/{command}")
    };
    if !ok {
        let message = value
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("command failed");
        text.push_str(": ");
        text.push_str(message);
        if let Some(usage) = value.get("usage").and_then(|v| v.as_str()) {
            if !usage.is_empty() {
                text.push_str(" — usage: ");
                text.push_str(usage);
            }
        }
    } else if let Some(result) = value.get("result") {
        let rendered = render_command_result(result);
        if !rendered.is_empty() {
            text.push_str(": ");
            text.push_str(&rendered);
        }
    } else {
        text.push_str(": ok");
    }

    // The dev commands return a human-readable `text`; prefer it verbatim.
    if let Some(summary) = value
        .get("result")
        .and_then(|r| r.get("text"))
        .and_then(|v| v.as_str())
    {
        text = summary.to_string();
    }
    text
}

/// Renders a command result object as one line: `key=value` pairs, or a compact
/// JSON dump when it is not a flat object.
fn render_command_result(result: &serde_json::Value) -> String {
    match result {
        serde_json::Value::Object(map) => {
            let mut parts = Vec::with_capacity(map.len());
            for (key, value) in map {
                // Skip the pre-rendered summary; it is already the line.
                if key == "text" {
                    continue;
                }
                parts.push(format!("{key}={}", render_scalar(value)));
            }
            parts.join(" ")
        }
        other => render_scalar(other),
    }
}

fn render_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Null => "null".into(),
        other => other.to_string(),
    }
}

/// Largest coordinate magnitude a teleport may carry. Matches
/// `skate_mods::TeleportOptions::validate` and the server's dev resource, so a
/// malformed or hostile payload cannot place the skater outside the supported
/// world.
const TELEPORT_COORD_LIMIT: f32 = 100_000.;
/// Largest `dev:teleport` payload accepted before parsing. The legit payload is
/// a few dozen bytes; anything larger is rejected without allocating a tree.
const TELEPORT_PAYLOAD_MAX: usize = 256;

/// Parses a `dev:teleport` payload `{"x","y","z"}` into a validated destination.
/// Returns `Err` with a human-readable reason when the payload is malformed,
/// out of range, or too large.
fn parse_dev_teleport(payload: &[u8]) -> Result<[f32; 3], &'static str> {
    if payload.len() > TELEPORT_PAYLOAD_MAX {
        return Err("dev:teleport: payload too large");
    }
    let value: serde_json::Value =
        serde_json::from_slice(payload).map_err(|_| "dev:teleport: malformed destination")?;
    let coord = |key: &str| value.get(key).and_then(|v| v.as_f64()).map(|v| v as f32);
    let (Some(x), Some(y), Some(z)) = (coord("x"), coord("y"), coord("z")) else {
        return Err("dev:teleport: malformed destination");
    };
    if !(x.is_finite() && y.is_finite() && z.is_finite()) {
        return Err("dev:teleport: non-finite destination");
    }
    if x.abs() > TELEPORT_COORD_LIMIT
        || y.abs() > TELEPORT_COORD_LIMIT
        || z.abs() > TELEPORT_COORD_LIMIT
    {
        return Err("dev:teleport: destination out of range");
    }
    Ok([x, y, z])
}

/// Folds a `dev:teleport` payload into a pending teleport.
fn fold_dev_teleport(net: &mut Multiplayer, payload: &[u8]) {
    match parse_dev_teleport(payload) {
        Ok(position) => net.pending_teleport = Some(position),
        Err(reason) => push_server_chat(net, reason.into()),
    }
}

/// Appends a server-authored line to the bounded local scrollback.
fn push_server_chat(net: &mut Multiplayer, text: String) {
    info!("COMMAND_RESULT {text:?}");
    net.chat_log.push_back(ChatLine {
        actor: SERVER_CHAT_ACTOR,
        text,
    });
    while net.chat_log.len() > CHAT_LOG_MAX {
        net.chat_log.pop_front();
    }
}

/// Applies a teleport requested by a server slash command. This is the only
/// system that owns both the pending request and the skater runtime; `receive`
/// cannot touch the world. Runs after the map is ready and before physics so
/// the teleport lands on the next simulate step.
pub(crate) fn apply_dev_commands(mut net: ResMut<Multiplayer>, mut skater: ResMut<SkaterRuntime>) {
    let Some(position) = net.pending_teleport.take() else {
        return;
    };
    let transform = crate::modding::session::spawn_matrix(position, None);
    match skater.travel(transform, None) {
        Ok(()) => push_server_chat(
            &mut net,
            format!(
                "teleported to x={:.2} y={:.2} z={:.2}",
                position[0], position[1], position[2]
            ),
        ),
        Err(error) => push_server_chat(&mut net, format!("teleport failed: {error}")),
    }
}
pub(crate) fn prepare(
    net: Res<Multiplayer>,
    mut physics: ResMut<GamePhysics>,
    skater: Res<SkaterRuntime>,
    mods: Option<Res<crate::modding::Mods>>,
) {
    physics.network_active = net.active();
    physics.network_contacts = 0;
    let mut proxies = std::mem::take(&mut physics.network_proxies);
    proxies.bodies.clear();
    proxies.volumes.clear();
    proxies.solids.clear();
    proxies.groups.clear();
    proxies.actors.clear();
    proxies.dynamics_before.clear();
    proxies.dynamics_deltas.clear();
    for (peer, remote) in &net.remotes {
        if mods
            .as_ref()
            .is_some_and(|m| crate::modding::peer_suspended(m, *peer))
        {
            continue;
        }
        if let Some(prediction) =
            skate_net::prediction::CollisionPrediction::at(remote.body_at.elapsed().as_secs_f32())
        {
            proxies.append(
                *peer,
                &remote.body,
                &net.schema,
                &physics,
                &skater,
                prediction,
            );
        }
    }
    physics.network_proxies = proxies;
}
fn send(
    mut net: ResMut<Multiplayer>,
    physics: Res<GamePhysics>,
    skater: Res<SkaterRuntime>,
    mods: Option<Res<crate::modding::Mods>>,
) {
    if !net.active() || skater.pose_generation == 0 {
        return;
    }
    let now = net.started.elapsed().as_millis() as u64;
    if net.last_body.elapsed() >= Duration::from_millis(49) {
        let mut state = network::capture_body(&physics, &skater);
        if let Some(root) = mods
            .as_ref()
            .and_then(|m| crate::modding::attachment::local_root(m))
        {
            state.root = network::pose(root.to_matrix());
            state.enabled = if mods
                .as_ref()
                .is_some_and(|m| crate::modding::player_attached(m))
            {
                1u64 << 62
            } else {
                0
            };
        }
        if mods
            .as_ref()
            .is_some_and(|m| crate::modding::player_suspended(m))
        {
            state.enabled = 0;
        }
        if let Some(server) = &mut net.server {
            server.maybe_send_body(&state, now);
        } else if let Some(p) = Packed::body(&state) {
            net.lobby.as_mut().unwrap().publish(packed::BODY, p, now);
        }
        net.last_body = Instant::now();
    }
}
pub(crate) fn send_pose(
    mut net: ResMut<Multiplayer>,
    skater: Res<SkaterRuntime>,
    mods: Option<Res<crate::modding::Mods>>,
) {
    if !net.active() || skater.pose_generation == 0 {
        return;
    }
    let now = net.started.elapsed().as_millis() as u64;
    if net.last_pose.elapsed() >= Duration::from_millis(if net.loopback { 49 } else { 99 }) {
        let mut pose = network::capture_pose(&skater, &net.anchors);
        if let Some(root) = mods
            .as_ref()
            .and_then(|m| crate::modding::attachment::local_root(m))
        {
            pose.root = network::pose(root.to_matrix());
        }
        if let Some(server) = &mut net.server {
            server.maybe_send_pose(&pose, now);
        } else if let Some(p) = Packed::pose(&pose) {
            net.lobby.as_mut().unwrap().publish(packed::POSE, p, now);
        }
        net.last_pose = Instant::now();
    }
}

fn player_name_path(asset_root: &std::path::Path) -> std::path::PathBuf {
    asset_root
        .parent()
        .unwrap_or(asset_root)
        .join("settings/player.json")
}

fn load_player_name(path: &std::path::Path) -> String {
    let Ok(bytes) = std::fs::read(path) else {
        return "Player".into();
    };
    let value: serde_json::Value =
        serde_json::from_slice(&bytes).unwrap_or(serde_json::Value::Null);
    sanitize_name(
        value
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("Player"),
    )
}

fn persist_player_name(path: &std::path::Path, name: &str) {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(
        path,
        serde_json::to_vec_pretty(&serde_json::json!({ "name": name })).unwrap_or_default(),
    );
}

pub(crate) fn sanitize_name(raw: &str) -> String {
    let mut out = String::new();
    for ch in raw.chars() {
        if out.chars().count() >= MAX_NAME {
            break;
        }
        if ch.is_ascii_alphanumeric() || matches!(ch, ' ' | '-' | '_') {
            out.push(ch);
        }
    }
    let trimmed = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if trimmed.is_empty() {
        "Player".into()
    } else {
        trimmed
    }
}

/// Mirrors the server's chat sanitation: control characters are dropped, runs
/// of whitespace collapse to single spaces, and the result is trimmed and
/// truncated to CHAT_MAX_CHARS runes.
pub(crate) fn sanitize_chat(raw: &str) -> String {
    let mut out = String::new();
    let mut count = 0usize;
    let mut pending_space = false;
    for ch in raw.chars() {
        if ch.is_whitespace() {
            pending_space = count > 0;
            continue;
        }
        if ch.is_control() {
            continue;
        }
        if pending_space && count < CHAT_MAX_CHARS {
            out.push(' ');
            count += 1;
            pending_space = false;
        }
        if count >= CHAT_MAX_CHARS {
            break;
        }
        out.push(ch);
        count += 1;
    }
    out.trim().to_owned()
}

fn sync_names(mut net: ResMut<Multiplayer>, mut ping_sent: Local<Option<Instant>>) {
    let name = net.published_name();
    let local = net.local_actor();
    if local != 0 {
        net.names.insert(local, name.clone());
    }
    if net.active() {
        let _ = net.publish_application(NAME_KEY, name.into_bytes());
        // Each player advertises their measured server RTT through the existing
        // actor-owned metadata stream, so clients can display the whole roster.
        if ping_sent.is_none_or(|sent| sent.elapsed() >= Duration::from_secs(1)) {
            let ping = if let Some(server) = &net.server {
                Some(server.rtt_ms)
            } else {
                net.lobby.as_ref().and_then(|l| {
                    if l.is_host() {
                        Some(0)
                    } else {
                        (l.stats.rtt_ms > 0).then_some(l.stats.rtt_ms)
                    }
                })
            };
            if let Some(ping) = ping {
                let _ = net.publish_application(hud::PING_KEY, ping.to_le_bytes().to_vec());
            }
            *ping_sent = Some(Instant::now());
        }
        // The lobby path reads records here; the server path fills names in
        // receive_from_server, so skip to avoid duplicated work.
        if net.server.is_none() {
            let records = net.application_records();
            for (peer, key, _, bytes) in records {
                if key != NAME_KEY {
                    continue;
                }
                if let Ok(raw) = String::from_utf8(bytes) {
                    net.names.insert(peer, sanitize_name(&raw));
                }
            }
            let live: std::collections::BTreeSet<u64> = net.player_ids().into_iter().collect();
            net.names.retain(|id, _| live.contains(id));
        }
    }
}

impl Multiplayer {
    pub(crate) fn debug_sections(&self) -> [String; 3] {
        [
            format!("CONNECTION\n{}\n{}", self.status, self.diagnostic_summary()),
            format!(
                "TRAFFIC & TRANSPORT\n{}\n{}",
                if self.rates.is_empty() {
                    "No traffic samples yet"
                } else {
                    &self.rates
                },
                if self.provider_metrics.is_empty() {
                    "No provider metrics"
                } else {
                    &self.provider_metrics
                }
            ),
            format!(
                "INTERPOLATION\n{}",
                if self.visual_status.is_empty() {
                    "No remote playback samples"
                } else {
                    &self.visual_status
                }
            ),
        ]
    }
}

#[cfg(test)]
mod chat_tests {
    use super::*;

    #[test]
    fn sanitize_chat_strips_and_limits() {
        assert_eq!(sanitize_chat("hello"), "hello");
        assert_eq!(sanitize_chat("a\x00b\nc\td"), "ab c d");
        assert_eq!(sanitize_chat("  spaced   out  "), "spaced out");
        assert_eq!(sanitize_chat("\u{1}\u{2}"), "");
        let long: String = "x".repeat(CHAT_MAX_CHARS + 40);
        assert_eq!(sanitize_chat(&long).chars().count(), CHAT_MAX_CHARS);
    }

    #[test]
    fn command_result_success_prefers_text_summary() {
        let value = serde_json::json!({
            "command": "coords",
            "ok": true,
            "result": { "actor": 7, "x": 1.5, "y": -2.0, "z": 30.0, "text": "Rider (7): x=1.50 y=-2.00 z=30.00" }
        });
        assert_eq!(
            format_command_result(&value),
            "Rider (7): x=1.50 y=-2.00 z=30.00"
        );
    }

    #[test]
    fn command_result_success_without_text_renders_pairs() {
        let value = serde_json::json!({
            "command": "tp",
            "ok": true,
            "result": { "teleport": { "x": 1.0, "y": 2.0, "z": 3.0 } }
        });
        let line = format_command_result(&value);
        assert!(line.starts_with("/tp: "), "unexpected line: {line}");
        assert!(line.contains("teleport="));
    }

    #[test]
    fn command_result_failure_includes_message_and_usage() {
        let value = serde_json::json!({
            "command": "tp",
            "ok": false,
            "message": "usage: /tp <x> <y> <z>",
            "usage": "/tp <x> <y> <z>"
        });
        let line = format_command_result(&value);
        assert!(
            line.contains("usage: /tp <x> <y> <z>"),
            "unexpected line: {line}"
        );
        assert!(
            line.contains("/tp <x> <y> <z>"),
            "usage hint missing: {line}"
        );
    }

    #[test]
    fn command_result_empty_command_is_reported() {
        let value = serde_json::json!({ "ok": false, "message": "unknown command" });
        assert!(format_command_result(&value).starts_with("/?: "));
    }

    #[test]
    fn dev_teleport_accepts_in_range_destination() {
        let position = parse_dev_teleport(br#"{"x":1.5,"y":-2,"z":30}"#).unwrap();
        assert_eq!(position, [1.5, -2.0, 30.0]);
    }

    #[test]
    fn dev_teleport_rejects_malformed_payloads() {
        assert!(parse_dev_teleport(b"not json").is_err());
        assert!(parse_dev_teleport(br#"{"x":1,"y":2}"#).is_err());
        assert!(parse_dev_teleport(br#"{"x":"a","y":2,"z":3}"#).is_err());
    }

    #[test]
    fn dev_teleport_rejects_out_of_range_and_non_finite() {
        assert!(parse_dev_teleport(br#"{"x":100001,"y":0,"z":0}"#).is_err());
        assert!(parse_dev_teleport(br#"{"x":0,"y":-200000,"z":0}"#).is_err());
        // A JSON string "NaN" does not parse as a number at all.
        assert!(parse_dev_teleport(br#"{"x":"NaN","y":0,"z":0}"#).is_err());
    }

    #[test]
    fn dev_teleport_rejects_oversized_payload() {
        let mut payload = String::from(r#"{"x":1,"y":2,"z":3,"pad":""#);
        payload.push_str(&"a".repeat(TELEPORT_PAYLOAD_MAX));
        payload.push_str(r#""}"#);
        assert!(parse_dev_teleport(payload.as_bytes()).is_err());
    }
}
