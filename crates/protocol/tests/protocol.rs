use repo_intelligence_protocol::{Envelope, PROTOCOL_VERSION};

#[test]
fn envelope_always_contains_protocol_version() {
    let value = serde_json::to_value(Envelope::success(serde_json::json!({"ok": true}))).unwrap();
    assert_eq!(value["protocol_version"], PROTOCOL_VERSION);
    assert_eq!(value["status"], "success");
}
