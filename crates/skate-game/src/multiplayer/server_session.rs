//! Authoritative session client: speaks the Protobuf `skated` protocol over a
//! single UDP socket. The server is the hub — every snapshot this client sends
//! goes to the server, and the server relays peers' snapshots back. The client
//! never needs peer addresses.
//!
//! This module is deliberately transport-only: it converts between the engine's
//! `Pose`/`Body`/`Bone` types and the generated prost types, and exposes a
//! simple `send`/`poll` surface. It does not touch Bevy.
use prost::Message;
use skate_net::{Body, Bone, Pose};
use skate_proto::v1;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

/// Fixed snapshot rates in the server mode. BODY carries the 33 rigid bodies,
/// POSE the animation anchors; both are full snapshots (no delta).
const BODY_INTERVAL: Duration = Duration::from_millis(50);
const POSE_INTERVAL: Duration = Duration::from_millis(100);
/// Handshake retry cadence before Welcome arrives.
const HELLO_RETRY: Duration = Duration::from_millis(500);
/// Liveness ping cadence once connected.
const PING_INTERVAL: Duration = Duration::from_millis(500);
/// A hello that never gets a welcome times out.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(15);
/// Retry cadence and budget for reliable client->server resource events.
const EVENT_RETRY: Duration = Duration::from_millis(500);
const EVENT_MAX_ATTEMPTS: u32 = 8;
/// How many recent reliable server events to remember for deduplication.
const REMOTE_EVENT_HISTORY: usize = 256;

/// A remote member as advertised by the server roster.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PeerInfo {
    pub actor: u64,
    pub rig: u64,
    pub display_name: String,
}

/// One script/mod event received this frame.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct EventNotice {
    /// Envelope actor. For server-originated events this is the recipient (the
    /// local actor); a future peer relay would carry the origin actor here.
    pub actor: u64,
    pub resource: String,
    pub name: String,
    /// Opaque payload, JSON by convention.
    pub payload: Vec<u8>,
}

/// What the session produced this frame.
#[derive(Default)]
pub(crate) struct Polled {
    /// Decoded body snapshots received this frame, keyed by origin actor.
    pub bodies: Vec<(u64, u32, u64, skate_net::packed::BodyState)>,
    /// Decoded pose snapshots received this frame, keyed by origin actor.
    pub poses: Vec<(u64, u32, u64, skate_net::packed::PoseState)>,
    /// Application records (names, mod state) received this frame.
    pub applications: Vec<(u64, String, u32, Vec<u8>)>,
    /// Chat lines received this frame as (origin actor, seq, text).
    pub chats: Vec<(u64, u32, String)>,
    /// Resource events received this frame, deduplicated.
    pub events: Vec<EventNotice>,
}

/// One outbound reliable event awaiting an ack. The encoded envelope is kept
/// so a retransmission is byte-identical to the original.
struct PendingEvent {
    envelope: Vec<u8>,
    sent: Instant,
    attempts: u32,
}

/// Client state for one connection to the server.
pub(crate) struct SkatedSession {
    socket: UdpSocket,
    /// Server address, resolved once.
    server: std::net::SocketAddr,

    session_id: u64,
    actor_id: u64,
    lobby_id: u64,
    epoch: u64,
    connected: bool,
    notice: String,

    started: Instant,
    last_hello: Instant,
    last_ping: Instant,
    last_body: Instant,
    last_pose: Instant,
    nonce: u32,

    body_seq: u32,
    pose_seq: u32,
    app_seq: u32,
    chat_seq: u32,
    // Resource-event send state. It is exercised by the scripting bridge
    // (`sdk.net.emit_server`) once installed; until then the client can still
    // receive and acknowledge server events through `Polled::events`.
    #[allow(dead_code)]
    event_seq: u32,
    #[allow(dead_code)]
    next_event_id: u64,
    /// Outbound reliable events awaiting a server ack, keyed by message id.
    pending_events: std::collections::BTreeMap<u64, PendingEvent>,
    /// Recently seen reliable server event ids, for deduplication.
    remote_event_seen: std::collections::HashSet<u64>,
    remote_event_order: std::collections::VecDeque<u64>,

    pub peers: std::collections::BTreeMap<u64, PeerInfo>,
    /// Latest application records seen per (actor, key), for names and mod state.
    pub applications: std::collections::BTreeMap<(u64, String), (u32, Vec<u8>)>,
    /// Peers joined since the last time the game drained this flag.
    pub roster_changed: bool,
    pub rtt_ms: u64,
}

impl SkatedSession {
    /// Binds a local socket and starts the handshake with `server`.
    pub fn connect(
        server: std::net::SocketAddr,
        info: v1::ClientInfo,
        display_name: String,
    ) -> std::io::Result<Self> {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        skate_net::socket::configure(&socket)?;
        let now = Instant::now();
        let mut session = Self {
            socket,
            server,
            session_id: 0,
            actor_id: 0,
            lobby_id: 0,
            epoch: 0,
            connected: false,
            notice: "Connecting to session server...".into(),
            started: now,
            last_hello: now - HELLO_RETRY,
            last_ping: now,
            last_body: now,
            last_pose: now,
            nonce: 0,
            body_seq: 0,
            pose_seq: 0,
            app_seq: 0,
            chat_seq: 0,
            event_seq: 0,
            next_event_id: 0,
            pending_events: std::collections::BTreeMap::new(),
            remote_event_seen: std::collections::HashSet::new(),
            remote_event_order: std::collections::VecDeque::new(),
            peers: std::collections::BTreeMap::new(),
            applications: std::collections::BTreeMap::new(),
            roster_changed: false,
            rtt_ms: 0,
        };
        session.send_hello(&info, &display_name);
        Ok(session)
    }

    pub fn is_connected(&self) -> bool {
        self.connected
    }
    pub fn actor_id(&self) -> u64 {
        self.actor_id
    }
    pub fn lobby_id(&self) -> u64 {
        self.lobby_id
    }
    pub fn notice(&self) -> &str {
        &self.notice
    }
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    fn next_nonce(&mut self) -> u32 {
        self.nonce = self.nonce.wrapping_add(1);
        self.nonce
    }

    fn send_hello(&mut self, info: &v1::ClientInfo, display_name: &str) {
        let envelope = v1::Envelope {
            protocol_version: skate_proto::PROTOCOL_VERSION,
            session_id: 0,
            actor_id: 0,
            nonce: self.next_nonce(),
            message: Some(v1::envelope::Message::Hello(v1::Hello {
                protocol_version: skate_proto::PROTOCOL_VERSION,
                build: "skate3rust".into(),
                lobby_id: 0,
                info: Some(info.clone()),
                display_name: display_name.to_owned(),
            })),
        };
        self.raw_send(&envelope);
        self.last_hello = Instant::now();
    }

    fn raw_send(&self, envelope: &v1::Envelope) {
        let mut bytes = Vec::with_capacity(128);
        if envelope.encode(&mut bytes).is_ok() {
            let _ = self.socket.send_to(&bytes, self.server);
        }
    }

    fn send_envelope(&mut self, message: v1::envelope::Message) {
        let envelope = v1::Envelope {
            protocol_version: skate_proto::PROTOCOL_VERSION,
            session_id: self.session_id,
            actor_id: self.actor_id,
            nonce: self.next_nonce(),
            message: Some(message),
        };
        self.raw_send(&envelope);
    }

    /// Services the socket: retries the handshake, keeps liveness, and decodes
    /// any incoming envelopes. Call once per frame.
    pub fn poll(&mut self, info: &v1::ClientInfo, display_name: &str) -> Polled {
        self.retry_handshake(info, display_name);
        self.keepalive();
        self.retry_events();

        let mut out = Polled::default();
        // A relayed BODY snapshot (33 bodies with full poses and rates) can be
        // several KiB. A small buffer makes Windows fail the read with
        // WSAEMSGSIZE and the datagram is lost, so size it above any snapshot.
        let mut buffer = [0u8; 64 * 1024];
        for _ in 0..256 {
            match self.socket.recv_from(&mut buffer) {
                Ok((n, from)) => {
                    if from != self.server {
                        continue;
                    }
                    if let Ok(envelope) = v1::Envelope::decode(&buffer[..n]) {
                        self.handle(envelope, &mut out);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(_) => break,
            }
        }
        out
    }

    fn retry_handshake(&mut self, info: &v1::ClientInfo, display_name: &str) {
        if self.connected {
            return;
        }
        if self.started.elapsed() > HANDSHAKE_TIMEOUT {
            self.notice = "Session server did not respond. Check the address and firewall.".into();
        }
        if self.last_hello.elapsed() >= HELLO_RETRY {
            self.send_hello(info, display_name);
        }
    }

    fn keepalive(&mut self) {
        if !self.connected || self.last_ping.elapsed() < PING_INTERVAL {
            return;
        }
        self.send_ping();
    }

    fn send_ping(&mut self) {
        let nonce = u64::from(self.next_nonce());
        let client_time_ms = self.started.elapsed().as_millis() as u64;
        self.send_envelope(v1::envelope::Message::Ping(v1::Ping {
            nonce,
            client_time_ms,
        }));
        self.last_ping = Instant::now();
    }

    fn handle(&mut self, envelope: v1::Envelope, out: &mut Polled) {
        match envelope.message {
            Some(v1::envelope::Message::Reject(reject)) => {
                self.notice = format!("Session rejected: {}", reject.detail);
            }
            Some(v1::envelope::Message::Welcome(welcome)) => {
                self.session_id = welcome.session_id;
                self.actor_id = welcome.actor_id;
                self.lobby_id = welcome.lobby_id;
                self.epoch = welcome.epoch;
                self.connected = true;
                self.notice.clear();
                // A fresh session invalidates any ids/envelopes queued for the
                // previous one; the server would drop their stale session.
                self.pending_events.clear();
                self.remote_event_seen.clear();
                self.remote_event_order.clear();
                self.apply_roster(welcome.roster);
                // Announce liveness immediately: the game may block for seconds
                // loading assets before the next frame runs poll(), and the
                // server must not time the session out in that gap.
                self.send_ping();
            }
            Some(v1::envelope::Message::Roster(roster)) => {
                if roster.epoch >= self.epoch {
                    self.epoch = roster.epoch;
                    self.apply_roster(roster.entries);
                }
            }
            Some(v1::envelope::Message::Pong(pong)) => {
                let now = self.started.elapsed().as_millis() as u64;
                if pong.client_time_ms <= now {
                    self.rtt_ms = now - pong.client_time_ms;
                }
            }
            Some(v1::envelope::Message::Snapshot(snapshot)) => {
                self.apply_snapshot(snapshot, out);
            }
            Some(v1::envelope::Message::Event(event)) => {
                let reliable =
                    event.delivery == v1::EventDelivery::Reliable as i32 && event.message_id != 0;
                if reliable {
                    // Acknowledge every copy so the server can stop retransmitting,
                    // but only surface the first delivery to the game.
                    self.send_event_ack(event.message_id, true, "");
                    if !self.remember_remote_event(event.message_id) {
                        return;
                    }
                }
                // Chat is a resource event; decode it for the local scrollback
                // as well as exposing it on the generic channel.
                if event.resource == "chat" && event.name == "message" {
                    if let Ok(chat) = v1::ChatMessage::decode(event.payload.as_slice()) {
                        out.chats.push((event.origin_actor, chat.seq, chat.text));
                    }
                }
                out.events.push(EventNotice {
                    actor: event.origin_actor,
                    resource: event.resource,
                    name: event.name,
                    payload: event.payload,
                });
            }
            Some(v1::envelope::Message::EventAck(ack)) => {
                self.pending_events.remove(&ack.message_id);
            }
            _ => {}
        }
    }

    fn apply_roster(&mut self, entries: Vec<v1::RosterEntry>) {
        let mut next = std::collections::BTreeMap::new();
        for entry in entries {
            if entry.actor_id == self.actor_id {
                continue;
            }
            let rig = entry.info.as_ref().map_or(0, |i| i.rig);
            next.insert(
                entry.actor_id,
                PeerInfo {
                    actor: entry.actor_id,
                    rig,
                    display_name: entry.display_name,
                },
            );
        }
        self.roster_changed = next != self.peers;
        self.peers = next;
    }

    fn apply_snapshot(&mut self, snapshot: v1::Snapshot, out: &mut Polled) {
        match snapshot.payload {
            Some(v1::snapshot::Payload::Body(body)) => {
                if let Some(state) = unpack_body(&body) {
                    out.bodies
                        .push((snapshot.origin_actor, snapshot.seq, body.captured_ms, state));
                }
            }
            Some(v1::snapshot::Payload::Pose(pose)) => {
                if let Some(state) = unpack_pose(&pose) {
                    out.poses
                        .push((snapshot.origin_actor, snapshot.seq, pose.captured_ms, state));
                }
            }
            Some(v1::snapshot::Payload::Application(record)) => {
                let entry = self
                    .applications
                    .entry((snapshot.origin_actor, record.key.clone()))
                    .or_insert((0, Vec::new()));
                if record.seq >= entry.0 {
                    *entry = (record.seq, record.value.clone());
                }
                out.applications.push((
                    snapshot.origin_actor,
                    record.key,
                    record.seq,
                    record.value,
                ));
            }
            None => {}
        }
    }

    /// Publishes a body snapshot if the fixed interval elapsed. Returns true
    /// when a datagram was sent.
    pub fn maybe_send_body(
        &mut self,
        state: &skate_net::packed::BodyState,
        captured_ms: u64,
    ) -> bool {
        if !self.connected || self.last_body.elapsed() < BODY_INTERVAL {
            return false;
        }
        self.body_seq = self.body_seq.wrapping_add(1);
        let snapshot = v1::Snapshot {
            kind: v1::StreamKind::Body as i32,
            origin_actor: self.actor_id,
            seq: self.body_seq,
            payload: Some(v1::snapshot::Payload::Body(pack_body(state, captured_ms))),
        };
        self.send_envelope(v1::envelope::Message::Snapshot(snapshot));
        self.last_body = Instant::now();
        true
    }

    /// Publishes a pose snapshot on its own interval.
    pub fn maybe_send_pose(
        &mut self,
        state: &skate_net::packed::PoseState,
        captured_ms: u64,
    ) -> bool {
        if !self.connected || self.last_pose.elapsed() < POSE_INTERVAL {
            return false;
        }
        self.pose_seq = self.pose_seq.wrapping_add(1);
        let snapshot = v1::Snapshot {
            kind: v1::StreamKind::Pose as i32,
            origin_actor: self.actor_id,
            seq: self.pose_seq,
            payload: Some(v1::snapshot::Payload::Pose(pack_pose_state(
                state,
                captured_ms,
            ))),
        };
        self.send_envelope(v1::envelope::Message::Snapshot(snapshot));
        self.last_pose = Instant::now();
        true
    }

    /// Publishes a key/value application record (name, mod state). The server
    /// relays it to other members; repeated sends are idempotent on the client.
    pub fn publish_application(&mut self, key: &str, value: Vec<u8>) {
        if !self.connected {
            return;
        }
        self.app_seq = self.app_seq.wrapping_add(1);
        let snapshot = v1::Snapshot {
            kind: v1::StreamKind::Application as i32,
            origin_actor: self.actor_id,
            seq: self.app_seq,
            payload: Some(v1::snapshot::Payload::Application(v1::ApplicationRecord {
                key: key.to_owned(),
                seq: self.app_seq,
                value,
            })),
        };
        self.send_envelope(v1::envelope::Message::Snapshot(snapshot));
    }

    /// Publishes a chat line to the lobby as a reliable `chat:send` resource
    /// event. The server is authoritative: it re-validates, rate-limits and
    /// echoes the line back as a `chat:message` event with the assigned author,
    /// so the sender does not render it locally. Callers pass raw text; the
    /// server sanitises and length-limits it.
    pub fn publish_chat(&mut self, text: String) {
        if !self.connected {
            return;
        }
        self.chat_seq = self.chat_seq.wrapping_add(1);
        let sent_ms = self.started.elapsed().as_millis() as u64;
        let payload = v1::ChatMessage {
            seq: self.chat_seq,
            text,
            sent_ms,
        }
        .encode_to_vec();
        self.publish_event("chat", "send", payload, true);
    }

    /// Publishes a script/mod event to the server. `reliable` events are
    /// retransmitted until the server acknowledges them and are deduplicated
    /// server-side, so a handler never runs twice for the same id. Returns the
    /// message id (0 for unreliable), which the server echoes in its ack.
    ///
    /// This is the transport entry point for the scripting bridge; until that
    /// bridge lands, nothing in the engine calls it yet.
    #[allow(dead_code)]
    pub fn publish_event(
        &mut self,
        resource: &str,
        name: &str,
        payload: Vec<u8>,
        reliable: bool,
    ) -> u64 {
        if !self.connected {
            return 0;
        }
        self.event_seq = self.event_seq.wrapping_add(1);
        let message_id = if reliable {
            self.next_event_id = self.next_event_id.wrapping_add(1);
            self.next_event_id
        } else {
            0
        };
        let nonce = self.next_nonce();
        let envelope = v1::Envelope {
            protocol_version: skate_proto::PROTOCOL_VERSION,
            session_id: self.session_id,
            actor_id: self.actor_id,
            nonce,
            message: Some(v1::envelope::Message::Event(v1::ResourceEvent {
                resource: resource.to_owned(),
                name: name.to_owned(),
                message_id,
                delivery: if reliable {
                    v1::EventDelivery::Reliable as i32
                } else {
                    v1::EventDelivery::Unreliable as i32
                },
                seq: self.event_seq,
                payload,
                // Stamped by the server from the authenticated session.
                origin_actor: 0,
            })),
        };
        if let Some(bytes) = encode_envelope(&envelope) {
            let _ = self.socket.send_to(&bytes, self.server);
            if reliable {
                self.pending_events.insert(
                    message_id,
                    PendingEvent {
                        envelope: bytes,
                        sent: Instant::now(),
                        attempts: 1,
                    },
                );
            }
        }
        message_id
    }

    /// Retransmits outbound reliable events whose ack has not arrived, dropping
    /// those past the retry budget.
    fn retry_events(&mut self) {
        if self.pending_events.is_empty() {
            return;
        }
        let now = Instant::now();
        let socket = &self.socket;
        let server = self.server;
        let mut expired = Vec::new();
        for (id, ev) in self.pending_events.iter_mut() {
            if now.duration_since(ev.sent) < EVENT_RETRY {
                continue;
            }
            if ev.attempts >= EVENT_MAX_ATTEMPTS {
                expired.push(*id);
                continue;
            }
            let _ = socket.send_to(&ev.envelope, server);
            ev.sent = now;
            ev.attempts += 1;
        }
        for id in expired {
            self.pending_events.remove(&id);
        }
    }

    /// Records a reliable server event id, returning false when it is a
    /// duplicate retransmission. The history is bounded.
    fn remember_remote_event(&mut self, id: u64) -> bool {
        if !self.remote_event_seen.insert(id) {
            return false;
        }
        self.remote_event_order.push_back(id);
        while self.remote_event_order.len() > REMOTE_EVENT_HISTORY {
            if let Some(old) = self.remote_event_order.pop_front() {
                self.remote_event_seen.remove(&old);
            }
        }
        true
    }

    /// Acknowledges a reliable server event so it leaves the retransmit queue.
    fn send_event_ack(&mut self, message_id: u64, accepted: bool, detail: &str) {
        self.send_envelope(v1::envelope::Message::EventAck(v1::ResourceEventAck {
            message_id,
            accepted,
            detail: detail.to_owned(),
        }));
    }

    /// Sends a polite goodbye so the server drops this member immediately.
    pub fn goodbye(&mut self) {
        if self.connected {
            self.send_envelope(v1::envelope::Message::Goodbye(v1::Goodbye {
                detail: "client leaving".into(),
            }));
        }
    }
}

// --- Codec between engine types and prost types ------------------------------

/// Encodes an envelope once, so a reliable event can be retransmitted
/// byte-identically without re-encoding.
#[allow(dead_code)]
fn encode_envelope(envelope: &v1::Envelope) -> Option<Vec<u8>> {
    let mut bytes = Vec::with_capacity(128);
    envelope.encode(&mut bytes).ok().map(|_| bytes)
}

fn pack_pose(pose: Pose) -> v1::Pose {
    v1::Pose {
        px: pose.p[0],
        py: pose.p[1],
        pz: pose.p[2],
        qx: pose.q[0],
        qy: pose.q[1],
        qz: pose.q[2],
        qw: pose.q[3],
    }
}

fn unpack_pose_message(pose: &v1::Pose) -> Pose {
    Pose {
        p: [pose.px, pose.py, pose.pz],
        q: [pose.qx, pose.qy, pose.qz, pose.qw],
    }
}

/// Lenient pose decoding for relayed snapshots.
///
/// The owner of a skater is authoritative for its own simulation, and the
/// solver can hold quaternions that are physically valid but not unit-length
/// to within `Pose::valid`'s strict tolerance. A relayed snapshot must be
/// accepted and normalised rather than discarded wholesale, otherwise one
/// slightly-off body would drop the entire 33-body update.
fn unpack_pose_lenient(pose: &v1::Pose) -> Option<Pose> {
    let raw = unpack_pose_message(pose);
    if !raw.p.iter().all(|v| v.is_finite() && v.abs() < 100_000.) {
        return None;
    }
    if !raw.q.iter().all(|v| v.is_finite()) {
        return None;
    }
    let norm = raw.q.iter().map(|v| v * v).sum::<f32>().sqrt();
    if !(norm > 1e-6) {
        return None;
    }
    Some(Pose {
        p: raw.p,
        q: raw.q.map(|v| v / norm),
    })
}

fn pack_body(state: &skate_net::packed::BodyState, captured_ms: u64) -> v1::BodySnapshot {
    v1::BodySnapshot {
        captured_ms,
        root: Some(pack_pose(state.root)),
        enabled: state.enabled,
        bodies: state
            .bodies
            .iter()
            .map(|body| v1::Body {
                pose: Some(pack_pose(body.pose)),
                vx: body.velocity[0],
                vy: body.velocity[1],
                vz: body.velocity[2],
                wx: body.angular[0],
                wy: body.angular[1],
                wz: body.angular[2],
            })
            .collect(),
    }
}

fn unpack_body(message: &v1::BodySnapshot) -> Option<skate_net::packed::BodyState> {
    let root = message.root.as_ref().and_then(unpack_pose_lenient)?;
    let bodies = message
        .bodies
        .iter()
        .map(|body| {
            let pose = body.pose.as_ref().and_then(unpack_pose_lenient)?;
            Some(Body {
                pose,
                velocity: [body.vx, body.vy, body.vz],
                angular: [body.wx, body.wy, body.wz],
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(skate_net::packed::BodyState {
        root,
        enabled: message.enabled,
        bodies,
    })
}

fn pack_pose_state(state: &skate_net::packed::PoseState, captured_ms: u64) -> v1::PoseSnapshot {
    v1::PoseSnapshot {
        captured_ms,
        root: Some(pack_pose(state.root)),
        bones: state
            .bones
            .iter()
            .map(|bone| v1::Bone {
                index: u32::from(bone.index),
                pose: Some(pack_pose(bone.pose)),
            })
            .collect(),
    }
}

fn unpack_pose(message: &v1::PoseSnapshot) -> Option<skate_net::packed::PoseState> {
    let root = message.root.as_ref().and_then(unpack_pose_lenient)?;
    let mut bones = Vec::with_capacity(message.bones.len());
    for bone in &message.bones {
        if bone.index >= 256 {
            return None;
        }
        bones.push(Bone {
            index: bone.index as u16,
            pose: bone.pose.as_ref().and_then(unpack_pose_lenient)?,
        });
    }
    Some(skate_net::packed::PoseState { root, bones })
}
/// Builds a [`v1::ClientInfo`] from the engine's compatibility fingerprints.
pub(crate) fn client_info(
    id: u64,
    map: u64,
    rig: u64,
    physics: u64,
    appearance: u64,
) -> v1::ClientInfo {
    v1::ClientInfo {
        id,
        map,
        rig,
        physics,
        appearance,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use skate_net::packed::{BodyState, PoseState};
    use skate_net::{Body, Bone, Pose};

    fn pose(x: f32) -> Pose {
        Pose {
            p: [x, 1., 2.],
            q: [0., 0., 0., 1.],
        }
    }

    fn body_state() -> BodyState {
        BodyState {
            root: pose(0.),
            enabled: (1u64 << 33) - 1,
            bodies: (0..33)
                .map(|i| Body {
                    pose: pose(i as f32),
                    velocity: [i as f32, 0., 1.],
                    angular: [0., i as f32, 0.],
                })
                .collect(),
        }
    }

    /// The engine's body snapshot survives the Protobuf round-trip unchanged.
    /// This is the core invariant for relayed movement.
    #[test]
    fn body_round_trips_through_protobuf() {
        let state = body_state();
        let packed = pack_body(&state, 1234);
        // The source capture time must survive verbatim: relaying it as zero
        // freezes remote interpolation on the receiver.
        assert_eq!(packed.captured_ms, 1234);
        // Encode/decode the whole payload, as the wire does.
        let mut bytes = Vec::new();
        packed.encode(&mut bytes).unwrap();
        let decoded = v1::BodySnapshot::decode(bytes.as_slice()).unwrap();
        let back = unpack_body(&decoded).expect("decode");
        assert_eq!(decoded.captured_ms, 1234);
        assert_eq!(back.root, state.root);
        assert_eq!(back.enabled, state.enabled);
        assert_eq!(back.bodies.len(), 33);
        for (original, restored) in state.bodies.iter().zip(&back.bodies) {
            assert_eq!(original.pose, restored.pose);
            assert_eq!(original.velocity, restored.velocity);
            assert_eq!(original.angular, restored.angular);
        }
    }

    /// Pose snapshots (animation anchors) likewise survive intact.
    #[test]
    fn pose_round_trips_through_protobuf() {
        let state = PoseState {
            root: pose(0.),
            bones: (0..8)
                .map(|i| Bone {
                    index: i,
                    pose: pose(i as f32),
                })
                .collect(),
        };
        let packed = pack_pose_state(&state, 4321);
        assert_eq!(packed.captured_ms, 4321);
        let mut bytes = Vec::new();
        packed.encode(&mut bytes).unwrap();
        let decoded = v1::PoseSnapshot::decode(bytes.as_slice()).unwrap();
        let back = unpack_pose(&decoded).expect("decode");
        assert_eq!(back.root, state.root);
        assert_eq!(back.bones.len(), 8);
        for (original, restored) in state.bones.iter().zip(&back.bones) {
            assert_eq!(original.index, restored.index);
            assert_eq!(original.pose, restored.pose);
        }
    }

    /// A chat line travels as a `chat:send` resource event payload and keeps
    /// its author in `origin_actor` on the way back.
    #[test]
    fn chat_round_trips_as_a_resource_event() {
        let message = v1::ChatMessage {
            seq: 7,
            text: "hello lobby".into(),
            sent_ms: 99,
        };
        let event = v1::ResourceEvent {
            resource: "chat".into(),
            name: "send".into(),
            message_id: 5,
            delivery: v1::EventDelivery::Reliable as i32,
            seq: 7,
            payload: message.encode_to_vec(),
            origin_actor: 42,
        };
        let envelope = v1::Envelope {
            protocol_version: skate_proto::PROTOCOL_VERSION,
            session_id: 1,
            actor_id: 42,
            nonce: 3,
            message: Some(v1::envelope::Message::Event(event)),
        };
        let mut bytes = Vec::new();
        envelope.encode(&mut bytes).unwrap();
        let decoded = v1::Envelope::decode(bytes.as_slice()).unwrap();
        assert_eq!(decoded.actor_id, 42);
        match decoded.message {
            Some(v1::envelope::Message::Event(event)) => {
                assert_eq!(event.origin_actor, 42);
                assert_eq!(event.resource, "chat");
                let chat = v1::ChatMessage::decode(event.payload.as_slice()).unwrap();
                assert_eq!(chat, message);
            }
            other => panic!("unexpected message: {other:?}"),
        }
    }

    /// A non-normalised or out-of-range quaternion is rejected, not trusted.
    #[test]
    fn invalid_pose_is_rejected() {
        let mut message = pack_body(&body_state(), 0);
        message.root = Some(v1::Pose {
            qx: 0.,
            qy: 0.,
            qz: 0.,
            qw: 0.,
            ..Default::default()
        });
        assert!(unpack_body(&message).is_none(), "zero quaternion must fail");
    }
}
