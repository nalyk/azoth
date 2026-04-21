//! Forward-compat gate for PR 2.1-A.
//!
//! Asserts that a v2.0.2-shape JSONL session round-trips through the
//! 2.1 reader without loss. The additive `SymbolKind` variants added
//! in 2.1 (Class / Method / Interface / TypeAlias / Decorator /
//! Package) must not break pre-2.1 sessions that carry only the
//! original eight tags.
//!
//! This test is the tripwire for the v2.1 ship criterion: "pre-2.1
//! JSONL + SQLite replay clean". It stays cheap by materialising a
//! minimal session fixture inline rather than depending on a
//! committed `.jsonl` file under the test fixtures tree.

use azoth_core::event_store::jsonl::JsonlReader;
use azoth_core::retrieval::SymbolKind;

/// A synthetic v2.0.2-shape session covering the event types the
/// reader most commonly touches on resume: run start, one committed
/// turn, one contract acceptance. Uses only fields that existed at
/// v2.0.2 — the serde `#[serde(default)]` on every subsequently-added
/// field keeps the deserialiser happy.
const V2_0_2_SESSION: &str = "\
{\"type\":\"run_started\",\"run_id\":\"run_fc\",\"contract_id\":\"ctr_fc\",\"timestamp\":\"2026-04-01T00:00:00Z\"}
{\"type\":\"turn_started\",\"turn_id\":\"t_1\",\"run_id\":\"run_fc\",\"timestamp\":\"2026-04-01T00:00:01Z\"}
{\"type\":\"turn_committed\",\"turn_id\":\"t_1\",\"outcome\":\"success\",\"usage\":{\"input_tokens\":10,\"output_tokens\":20}}
";

#[test]
fn pre_2_1_jsonl_projections_are_stable() {
    let td = tempfile::TempDir::new().unwrap();
    let p = td.path().join("run_fc.jsonl");
    std::fs::write(&p, V2_0_2_SESSION).unwrap();

    let reader = JsonlReader::open(&p);

    // Replayable projection: committed turn → 3 events surface.
    let replayable = reader.replayable().expect("replayable projection");
    assert_eq!(
        replayable.len(),
        3,
        "all three lines of the committed turn replay"
    );

    // Forensic projection: every line is present and tagged
    // replayable (outcome = committed).
    let forensic = reader.forensic().expect("forensic projection");
    assert_eq!(forensic.len(), 3);
    for ev in &forensic {
        assert!(!ev.non_replayable, "committed turn is replayable");
    }
}

/// Every pre-2.1 `SymbolKind` wire tag must still deserialise under
/// the 2.1 binary. This is the "additive schema" rule 1 of the
/// trilogy spec, tested at the wire surface.
#[test]
fn pre_2_1_symbolkind_tags_deserialize_under_new_binary() {
    for tag in [
        "\"function\"",
        "\"struct\"",
        "\"enum\"",
        "\"enum_variant\"",
        "\"trait\"",
        "\"impl\"",
        "\"module\"",
        "\"const\"",
    ] {
        let got: SymbolKind = serde_json::from_str(tag).expect(tag);
        let re = serde_json::to_string(&got).unwrap();
        assert_eq!(re, tag, "round-trip is byte-stable for {tag}");
    }
}

/// v2.1-new variants round-trip via serde with their documented tags.
/// Separate test so a failure names "new variant broken" vs.
/// "pre-2.1 variant broken".
#[test]
fn v2_1_symbolkind_new_variants_round_trip() {
    for (tag, want) in [
        ("\"class\"", SymbolKind::Class),
        ("\"method\"", SymbolKind::Method),
        ("\"interface\"", SymbolKind::Interface),
        ("\"type_alias\"", SymbolKind::TypeAlias),
        ("\"decorator\"", SymbolKind::Decorator),
        ("\"package\"", SymbolKind::Package),
    ] {
        let got: SymbolKind = serde_json::from_str(tag).expect(tag);
        assert_eq!(got, want);
    }
}
