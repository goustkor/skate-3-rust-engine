//! Generated Protobuf bindings for the authoritative session protocol.
//!
//! These types are compiled by `build.rs` from the shared `.proto` sources in
//! `server-go/proto/skatev1`, which are also used by the Go `skated` server.
//! Both sides therefore share one schema; a change to the `.proto` files
//! rebuilds both.
//!
//! The module layout mirrors the proto package `skate.v1`: [`v1`] contains the
//! envelope and its message bodies.

#![allow(clippy::all)]

/// Types from package `skate.v1`.
pub mod v1 {
    include!(concat!(env!("OUT_DIR"), "/skate.v1.rs"));
}

/// Protocol revision this build speaks. Must match `protocol.Version` in the
/// Go server and [`crate::envelope`]'s documented value. v2 moved chat onto the
/// resource event plane (`chat:send` / `chat:message`).
pub const PROTOCOL_VERSION: u32 = 2;

#[cfg(test)]
mod tests {
    use super::v1;
    use crate::PROTOCOL_VERSION;
    use prost::Message;

    #[test]
    fn envelope_round_trips_through_protobuf() {
        let envelope = v1::Envelope {
            protocol_version: PROTOCOL_VERSION,
            session_id: 42,
            actor_id: 7,
            nonce: 9,
            message: Some(v1::envelope::Message::Hello(v1::Hello {
                protocol_version: PROTOCOL_VERSION,
                build: "test".into(),
                lobby_id: 0,
                info: Some(v1::ClientInfo {
                    id: 7,
                    map: 1,
                    rig: 2,
                    physics: 3,
                    appearance: 4,
                }),
                display_name: "Skater".into(),
            })),
        };
        let mut bytes = Vec::new();
        envelope.encode(&mut bytes).expect("encode");
        let decoded = v1::Envelope::decode(bytes.as_slice()).expect("decode");
        assert_eq!(decoded, envelope);
    }

    #[test]
    fn resource_event_round_trips() {
        let event = v1::ResourceEvent {
            resource: "race".into(),
            name: "join".into(),
            message_id: 42,
            delivery: v1::EventDelivery::Reliable as i32,
            seq: 7,
            payload: b"{\"lap\":1}".to_vec(),
            origin_actor: 9,
        };
        let ack = v1::ResourceEventAck {
            message_id: 42,
            accepted: true,
            detail: String::new(),
        };
        let envelope = v1::Envelope {
            protocol_version: PROTOCOL_VERSION,
            session_id: 1,
            actor_id: 7,
            nonce: 2,
            message: Some(v1::envelope::Message::Event(event.clone())),
        };
        let mut bytes = Vec::new();
        envelope.encode(&mut bytes).expect("encode");
        match v1::Envelope::decode(bytes.as_slice()).unwrap().message {
            Some(v1::envelope::Message::Event(back)) => assert_eq!(back, event),
            other => panic!("unexpected message: {other:?}"),
        }

        let envelope = v1::Envelope {
            protocol_version: PROTOCOL_VERSION,
            session_id: 1,
            actor_id: 7,
            nonce: 3,
            message: Some(v1::envelope::Message::EventAck(ack.clone())),
        };
        let mut bytes = Vec::new();
        envelope.encode(&mut bytes).expect("encode");
        match v1::Envelope::decode(bytes.as_slice()).unwrap().message {
            Some(v1::envelope::Message::EventAck(back)) => assert_eq!(back, ack),
            other => panic!("unexpected message: {other:?}"),
        }
    }

    #[test]
    fn body_snapshot_round_trips() {
        let snapshot = v1::Snapshot {
            kind: v1::StreamKind::Body as i32,
            origin_actor: 3,
            seq: 11,
            payload: Some(v1::snapshot::Payload::Body(v1::BodySnapshot {
                captured_ms: 123,
                root: Some(v1::Pose {
                    px: 1.,
                    py: 2.,
                    pz: 3.,
                    qw: 1.,
                    ..Default::default()
                }),
                enabled: 1,
                bodies: vec![v1::Body {
                    pose: Some(v1::Pose {
                        qw: 1.,
                        ..Default::default()
                    }),
                    ..Default::default()
                }],
            })),
        };
        let mut bytes = Vec::new();
        snapshot.encode(&mut bytes).expect("encode");
        assert_eq!(v1::Snapshot::decode(bytes.as_slice()).unwrap(), snapshot);
    }
}
