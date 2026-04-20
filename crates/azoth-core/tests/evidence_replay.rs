//! Sprint 4 verification: v1.5 JSONL sessions replay clean under v2
//! binary. New optional schema fields (`EvidenceItem.lane` and
//! `.rerank_score`) must deserialize via `#[serde(default)]` on old
//! logs without surprise.
//!
//! Risk ledger #1 ("Cache-prefix-stability drift") also depends on a
//! stable schema boundary — this test guards the deserialization
//! contract directly.

use azoth_core::schemas::{ContextPacket, EvidenceItem, SessionEvent};

/// A ContextPacket event recorded by a v1.5 binary had no `lane` /
/// `rerank_score` on its `evidence_lane` items. Must deserialize
/// unchanged.
#[test]
fn v1_5_evidence_item_deserialises_without_new_fields() {
    let v1_5_json = r#"{
        "label": "src/auth/tokens.rs:42",
        "artifact_ref": null,
        "inline": "fn refresh_token()",
        "decision_weight": 100
    }"#;

    let item: EvidenceItem = serde_json::from_str(v1_5_json).unwrap();
    assert_eq!(item.label, "src/auth/tokens.rs:42");
    assert_eq!(item.decision_weight, 100);
    assert!(
        item.lane.is_none(),
        "lane must default to None on v1.5 logs"
    );
    assert!(
        item.rerank_score.is_none(),
        "rerank_score must default to None on v1.5 logs"
    );
}

#[test]
fn v2_evidence_item_round_trips_with_new_fields_present() {
    let original = EvidenceItem {
        label: "src/a.rs:1".into(),
        artifact_ref: None,
        inline: Some("fn foo()".into()),
        decision_weight: 42,
        lane: Some("lexical".into()),
        rerank_score: Some(0.0163),
        observed_at: None,
        valid_at: None,
        freshness: None,
    };
    let json = serde_json::to_string(&original).unwrap();
    let roundtripped: EvidenceItem = serde_json::from_str(&json).unwrap();
    assert_eq!(original, roundtripped);
}

#[test]
fn v2_evidence_item_with_none_lane_omits_field_on_serialize() {
    // `#[serde(skip_serializing_if = "Option::is_none")]` keeps old
    // log shapes byte-identical when no lane is tagged — important
    // because the kernel's packet digest is sha256 of the serialized
    // bytes (cache-prefix stability).
    let v1_shape = EvidenceItem {
        label: "a".into(),
        artifact_ref: None,
        inline: None,
        decision_weight: 1,
        lane: None,
        rerank_score: None,
        observed_at: None,
        valid_at: None,
        freshness: None,
    };
    let json = serde_json::to_string(&v1_shape).unwrap();
    assert!(
        !json.contains("lane"),
        "lane=None must be omitted, got: {json}"
    );
    assert!(
        !json.contains("rerank_score"),
        "rerank_score=None must be omitted, got: {json}"
    );
}

#[test]
fn v1_5_context_packet_event_round_trips() {
    // A `SessionEvent::ContextPacket` recorded on v1.5 deserialises on
    // v2 with no panic and no silent data loss.
    let v1_5_line = r#"{
        "type": "context_packet",
        "turn_id": "t_001",
        "packet_id": "ctx_abc",
        "packet_digest": "sha256:deadbeef"
    }"#;

    let event: SessionEvent = serde_json::from_str(v1_5_line).unwrap();
    match event {
        SessionEvent::ContextPacket {
            turn_id,
            packet_id,
            packet_digest,
        } => {
            assert_eq!(turn_id.0, "t_001");
            assert_eq!(packet_id.0, "ctx_abc");
            assert_eq!(packet_digest, "sha256:deadbeef");
        }
        other => panic!("wrong variant: {other:?}"),
    }
}

#[test]
fn context_packet_with_v1_5_shape_evidence_lane_parses() {
    // Full ContextPacket fixture as v1.5 would have written it.
    let v1_5 = r#"{
        "id": "ctx_1",
        "contract_id": "ctr_1",
        "turn_id": "t_1",
        "digest": "sha256:abcdef",
        "constitution_lane": {
            "contract_digest": "sha256:contract",
            "tool_schemas_digest": "sha256:tools",
            "policy_version": "policy_v1",
            "system_prompt": "you are azoth"
        },
        "working_set_lane": [],
        "evidence_lane": [
            {"label": "a.rs:1", "artifact_ref": null, "inline": "x", "decision_weight": 5}
        ],
        "checkpoint_lane": null,
        "exit_criteria_lane": {"step_goal": "g", "rubric": []}
    }"#;

    let packet: ContextPacket = serde_json::from_str(v1_5).unwrap();
    assert_eq!(packet.evidence_lane.len(), 1);
    assert!(packet.evidence_lane[0].lane.is_none());
    assert!(packet.evidence_lane[0].rerank_score.is_none());
}
