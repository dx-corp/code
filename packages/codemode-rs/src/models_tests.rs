use super::*;
use serde_json::json;

fn tool(name: &str, operation: ModelOperation, model: &str) -> Tool {
    Tool {
        name: name.into(),
        description: "Host-owned specialist".into(),
        schema: json!({"type":"object","required":["input"],"additionalProperties":false,"properties":{"input":{"type":"string"}}}),
        output_schema: Some(json!({"type":"object","properties":{"label":{"type":"string"}}})),
        namespace: None,
        model_operation: Some(operation),
        model_binding: Some(ModelBinding {
            owner: "model-owner".into(),
            provider: "configured-provider".into(),
            model: model.into(),
        }),
    }
}

#[test]
fn ordinary_tool_arguments_and_owner_schemas_are_preserved_exactly() {
    let tools = vec![tool(
        "owner.classifier",
        ModelOperation::Classify,
        "classifier-v1",
    )];
    let args = json!({"input":"opaque text", "confidence":0.99});
    let call = resolve_model_call(
        &tools,
        ModelOperation::Classify,
        &ModelSelector::default(),
        args.clone(),
    )
    .unwrap();
    assert_eq!(
        call,
        ModelCall {
            name: "owner.classifier".into(),
            args
        }
    );
    let aliases = admitted_models(&tools).unwrap();
    assert_eq!(aliases[0].input_schema, tools[0].schema);
    assert_eq!(aliases[0].output_schema, tools[0].output_schema);
}

#[test]
fn model_named_tool_without_host_operation_metadata_does_not_admit_an_alias() {
    let mut named = tool("models.classify", ModelOperation::Classify, "classifier-v1");
    named.model_operation = None;
    assert!(admitted_models(&[named.clone()]).unwrap().is_empty());
    assert!(
        resolve_model_call(
            &[named],
            ModelOperation::Classify,
            &ModelSelector::default(),
            json!({})
        )
        .unwrap_err()
        .contains("unavailable")
    );
}

#[test]
fn missing_or_wrong_operation_is_unavailable_before_any_dispatch() {
    let tools = [tool(
        "owner.classifier",
        ModelOperation::Classify,
        "classifier-v1",
    )];
    for catalog in [&[][..], &tools[..]] {
        assert!(
            resolve_model_call(
                catalog,
                ModelOperation::GenerateImages,
                &ModelSelector::default(),
                json!({})
            )
            .unwrap_err()
            .contains("unavailable")
        );
    }
}

#[test]
fn candidates_require_exact_selection_and_never_prefer_catalog_order() {
    let tools = vec![
        tool("owner.first", ModelOperation::Classify, "model-a"),
        tool("owner.second", ModelOperation::Classify, "model-b"),
    ];
    assert!(
        resolve_model_call(
            &tools,
            ModelOperation::Classify,
            &ModelSelector::default(),
            json!({})
        )
        .unwrap_err()
        .contains("ambiguous")
    );
    let selector = ModelSelector {
        model: Some("model-b".into()),
        ..Default::default()
    };
    assert_eq!(
        resolve_model_call(&tools, ModelOperation::Classify, &selector, json!({}))
            .unwrap()
            .name,
        "owner.second"
    );
    let unknown = ModelSelector {
        model: Some("unadmitted".into()),
        ..Default::default()
    };
    assert!(
        resolve_model_call(&tools, ModelOperation::Classify, &unknown, json!({}))
            .unwrap_err()
            .contains("unavailable")
    );
    let wrong_owner = ModelSelector {
        owner: Some("other-owner".into()),
        ..selector
    };
    assert!(
        resolve_model_call(&tools, ModelOperation::Classify, &wrong_owner, json!({}))
            .unwrap_err()
            .contains("unavailable")
    );
}

#[test]
fn metadata_without_exact_owner_binding_and_endpoint_like_bindings_fail_closed() {
    let mut missing = tool("owner.classifier", ModelOperation::Classify, "model-a");
    missing.model_binding = None;
    assert!(admitted_models(&[missing]).is_err());
    for reference in [
        "",
        "https://provider.invalid/v1",
        "secret=value",
        "model with spaces",
    ] {
        let invalid = tool("owner.classifier", ModelOperation::Classify, reference);
        assert!(admitted_models(&[invalid]).is_err());
    }
    assert!(
        serde_json::from_value::<ModelSelector>(json!({"baseURL":"https://provider.invalid"}))
            .is_err()
    );
    assert!(serde_json::from_value::<ModelSelector>(json!({"credential":"secret"})).is_err());
}

#[test]
fn duplicate_tool_names_and_ambiguous_same_model_routes_are_rejected() {
    let classifier = tool("owner.classifier", ModelOperation::Classify, "model-a");
    assert!(admitted_models(&[classifier.clone(), classifier.clone()]).is_err());
    let mut other = classifier.clone();
    other.name = "other.classifier".into();
    other.model_binding.as_mut().unwrap().provider = "other-provider".into();
    let tools = vec![classifier, other];
    let model_only = ModelSelector {
        model: Some("model-a".into()),
        ..Default::default()
    };
    assert!(
        resolve_model_call(&tools, ModelOperation::Classify, &model_only, json!({}))
            .unwrap_err()
            .contains("ambiguous")
    );
    let exact = ModelSelector {
        provider: Some("other-provider".into()),
        ..model_only
    };
    assert_eq!(
        resolve_model_call(&tools, ModelOperation::Classify, &exact, json!({}))
            .unwrap()
            .name,
        "other.classifier"
    );
}

#[test]
fn image_operation_resolves_to_ordinary_owner_action_without_rewriting_arguments() {
    let tools = vec![tool(
        "owner.images",
        ModelOperation::GenerateImages,
        "image-v1",
    )];
    let args = json!({"prompt":"A lighthouse", "count":2});
    assert_eq!(
        resolve_model_call(
            &tools,
            ModelOperation::GenerateImages,
            &ModelSelector::default(),
            args.clone()
        )
        .unwrap(),
        ModelCall {
            name: "owner.images".into(),
            args
        }
    );
}
