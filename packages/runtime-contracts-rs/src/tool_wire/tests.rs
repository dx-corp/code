use super::*;
use serde_json::{Value, json};

fn request() -> Value {
    json!({
        "type": "governed_client_tool_request", "call_id": "call-1",
        "tool_execution_id": "execution-1", "tool": "test.read", "args": {},
        "provider_tool_name": "provider_read", "tool_id": "read",
        "client_instance_id": "client-1", "grant_id": "grant-1", "grant_version": 7,
        "grant_hash": "grant-hash", "turn_digest": "turn-digest",
        "definition_digest": "definition-digest", "args_digest": "args-digest",
        "owner_lease_epoch": 9, "idempotency_key": "result-1"
    })
}

fn result() -> Value {
    let mut result = request();
    let object = result.as_object_mut().unwrap();
    for field in ["tool", "args", "provider_tool_name", "tool_id"] {
        object.remove(field);
    }
    object.insert("type".into(), json!("governed_client_tool_result"));
    object.insert(
        "content".into(),
        json!([{"type":"text", "text":"done"},
        {"type":"image", "data":"base64", "mimeType":"image/png"}]),
    );
    object.insert("is_error".into(), json!(false));
    result
}

#[test]
fn governed_payloads_preserve_existing_flat_wire_format() {
    let request = request();
    let decoded: ToolServerMessage = serde_json::from_value(request.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), request);
    let result = result();
    let decoded: ToolClientMessage = serde_json::from_value(result.clone()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), result);
}

#[test]
fn governed_identity_fields_are_required_and_typed_in_both_directions() {
    for (wire, client) in [(request(), false), (result(), true)] {
        for field in [
            "call_id",
            "tool_execution_id",
            "client_instance_id",
            "grant_id",
            "grant_version",
            "grant_hash",
            "turn_digest",
            "definition_digest",
            "args_digest",
            "owner_lease_epoch",
            "idempotency_key",
        ] {
            for replacement in [None, Some(Value::Null), Some(json!({}))] {
                let mut malformed = wire.clone();
                match replacement {
                    Some(value) => {
                        malformed[field] = value;
                    }
                    None => {
                        malformed.as_object_mut().unwrap().remove(field);
                    }
                }
                let rejected = if client {
                    serde_json::from_value::<ToolClientMessage>(malformed).is_err()
                } else {
                    serde_json::from_value::<ToolServerMessage>(malformed).is_err()
                };
                assert!(rejected, "accepted malformed {field} (client={client})");
            }
        }
    }
}

#[test]
fn completion_rejects_malformed_success_or_execution_id() {
    let wire = json!({"type":"tool_end", "call_id":"call-1", "success":true});
    assert!(serde_json::from_value::<ToolServerMessage>(wire.clone()).is_ok());
    for value in [Value::Null, json!("true"), json!(1)] {
        let mut bad = wire.clone();
        bad["success"] = value;
        assert!(serde_json::from_value::<ToolServerMessage>(bad).is_err());
    }
    let mut missing = wire.clone();
    missing.as_object_mut().unwrap().remove("success");
    assert!(serde_json::from_value::<ToolServerMessage>(missing).is_err());
    let mut bad = wire;
    bad["tool_execution_id"] = json!(42);
    assert!(serde_json::from_value::<ToolServerMessage>(bad).is_err());
}

#[test]
fn result_rejects_unknown_content_and_wrong_cost_types() {
    for content in [
        json!([{"type":"future_content"}]),
        json!([{"type":"text"}]),
        json!("done"),
    ] {
        let mut wire = result();
        wire["content"] = content;
        assert!(serde_json::from_value::<ToolClientMessage>(wire).is_err());
    }
    for cost in [json!(-1), json!(1.5), json!("12")] {
        let mut wire = result();
        wire["process_tool_cost_micros"] = cost;
        assert!(serde_json::from_value::<ToolClientMessage>(wire).is_err());
    }
}

#[test]
fn unrelated_tags_never_decode_as_tool_acknowledgements() {
    for tag in ["ready", "turn_completed", "future_tool_end"] {
        let wire = json!({"type":tag,"call_id":"call-1","success":true});
        assert!(serde_json::from_value::<ToolServerMessage>(wire).is_err());
    }
}

#[test]
fn acceptance_requires_a_correlated_request_and_is_distinct_from_completion() {
    let wire = json!({"type":"response_accepted", "request_id":"execution-1"});
    assert!(
        matches!(serde_json::from_value::<ToolServerMessage>(wire.clone()).unwrap(),
        ToolServerMessage::ResponseAccepted(ResponseAccepted { request_id }) if request_id == "execution-1")
    );
    for malformed in [
        json!({"type":"response_accepted"}),
        json!({"type":"response_accepted", "request_id":null}),
        json!({"type":"response_accepted", "request_id":5}),
        json!({"type":"response_accepted", "call_id":"execution-1", "success":true}),
    ] {
        assert!(serde_json::from_value::<ToolServerMessage>(malformed).is_err());
    }
    assert!(matches!(
        serde_json::from_value::<ToolServerMessage>(json!({
            "type":"tool_end", "call_id":"execution-1", "success":true,
        }))
        .unwrap(),
        ToolServerMessage::ToolEnd(_)
    ));
}
