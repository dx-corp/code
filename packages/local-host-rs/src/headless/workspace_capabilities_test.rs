use std::collections::HashMap;

use sha2::{Digest, Sha256};

use super::controller_binding::{
    CONTROLLER_BINDING_VERSION, CONTROLLER_CONTEXT_SCHEMA_VERSION, ControllerBindingReceipt,
    ControllerContext, ControllerLifetimeProfile,
};
use super::workspace_capabilities::{
    ApplyWorkspaceCapabilitySet, WorkspaceCapabilityActivation, WorkspacePromptCapability,
    accept_workspace_capability_receipt, recompute_request_digests,
};

#[test]
fn runtime_receipts_cannot_promote_partial_sets_or_roll_back_replay() {
    let mut activation = WorkspaceCapabilityActivation::new("base prompt".to_string());
    let first = request(1, "first generation");
    let second = request(2, "second generation");
    let first_receipt = activation
        .apply(
            first.clone(),
            &binding(),
            &resident_context(),
            "runner-1",
            false,
        )
        .expect("first capability set");
    let second_receipt = activation
        .apply(
            second.clone(),
            &binding(),
            &resident_context(),
            "runner-1",
            false,
        )
        .expect("second capability set");
    let mut pending = HashMap::from([
        (first_receipt.replay_cursor.clone(), first.clone()),
        (second_receipt.replay_cursor.clone(), second.clone()),
    ]);
    let mut accepted = None;

    for malformed in [
        {
            let mut receipt = second_receipt.clone();
            receipt.schema_version = "unknown".to_string();
            receipt
        },
        {
            let mut receipt = second_receipt.clone();
            receipt.accepted_entry_digests.clear();
            receipt
        },
        {
            let mut receipt = second_receipt.clone();
            receipt.rejected_entries.push("skill.review".to_string());
            receipt
        },
    ] {
        assert!(!accept_workspace_capability_receipt(
            &mut pending,
            &mut accepted,
            &malformed,
        ));
        assert!(accepted.is_none());
    }

    assert!(accept_workspace_capability_receipt(
        &mut pending,
        &mut accepted,
        &second_receipt,
    ));
    assert_eq!(accepted, Some(second.clone()));
    assert!(
        pending.is_empty(),
        "older pending sets lose replay authority"
    );
    pending.insert(first_receipt.replay_cursor.clone(), first);
    assert!(!accept_workspace_capability_receipt(
        &mut pending,
        &mut accepted,
        &first_receipt,
    ));
    assert_eq!(accepted, Some(second));
}

fn resident_context() -> ControllerContext {
    ControllerContext {
        schema_version: CONTROLLER_CONTEXT_SCHEMA_VERSION.to_string(),
        controller_id: "evalops.platform".to_string(),
        organization_id: "org-1".to_string(),
        workspace_id: "workspace-1".to_string(),
        thread_id: "thread-1".to_string(),
        channel_id: None,
        request_id: None,
        lifetime_profile: ControllerLifetimeProfile::Resident,
        runtime_generation: Some(7),
    }
}

fn capability(qualified_id: &str, name: &str, pinned: bool) -> WorkspacePromptCapability {
    let body = format!("Pinned body for {qualified_id}.");
    WorkspacePromptCapability {
        qualified_id: qualified_id.to_string(),
        name: name.to_string(),
        scope: "workspace".to_string(),
        revision_digest: "sha256:2222222222222222222222222222222222222222222222222222222222222222"
            .to_string(),
        body_digest: prompt_digest(&body),
        trigger_patterns: vec![name.to_string()],
        user_invocable: true,
        pinned_prompt_only: pinned,
        title: "Review".to_string(),
        description: "Review a workspace change.".to_string(),
        instructions: vec!["Apply the review checklist.".to_string()],
        body,
        entry_digest: String::new(),
    }
}

fn request(generation: u64, instruction: &str) -> ApplyWorkspaceCapabilitySet {
    let mut request = ApplyWorkspaceCapabilitySet {
        organization_id: "org-1".to_string(),
        workspace_id: "workspace-1".to_string(),
        runner_session_id: "runner-1".to_string(),
        runtime_generation: 7,
        activation_generation: generation,
        workspace_snapshot_digest:
            "sha256:0000000000000000000000000000000000000000000000000000000000000000".to_string(),
        workspace_skill_set_digest:
            "sha256:1111111111111111111111111111111111111111111111111111111111111111".to_string(),
        capability_set_digest: String::new(),
        workspace_instructions: vec![instruction.to_string()],
        admitted_catalog: vec![capability("skill.review", "review", true)],
        admission_receipt_id: "admission-1".to_string(),
    };
    recompute_request_digests(&mut request).expect("test request digests");
    request
}

fn binding() -> ControllerBindingReceipt {
    ControllerBindingReceipt {
        binding_version: CONTROLLER_BINDING_VERSION.to_string(),
        binding_sha256: "sha256:binding".to_string(),
        controller_context: resident_context(),
    }
}

fn prompt_digest(prompt: &str) -> String {
    format!("sha256:{:x}", Sha256::digest(prompt.as_bytes()))
}

#[test]
fn resident_admission_replaces_complete_catalog_stages_and_proves_provider_prompt() {
    let mut activation = WorkspaceCapabilityActivation::new("base prompt".to_string());
    let first = activation
        .apply(
            request(1, "Follow workspace rules."),
            &binding(),
            &resident_context(),
            "runner-1",
            false,
        )
        .expect("resident admission is accepted");
    assert_eq!(first.organization_id, "org-1");
    assert_eq!(first.workspace_id, "workspace-1");
    assert_eq!(first.runner_session_id, "runner-1");
    assert_eq!(first.runtime_generation, 7);
    assert_eq!(first.activation_generation, 1);
    assert_eq!(first.accepted_entry_digests.len(), 1);
    assert!(first.rejected_entries.is_empty());
    assert!(first.applied_at > 0);
    assert_eq!(first.controller_binding_sha256, "sha256:binding");
    assert_eq!(
        first.provider_prompt_sha256,
        "sha256:16d7b26dd0e0c41d24714d4451af7205774dbeeba2521ccacaa5eae83f20e7b4"
    );
    assert!(
        activation
            .current_prompt()
            .contains("Pinned body for skill.review.")
    );
    assert!(activation.current_prompt().contains("executable authority"));

    let mut replacement = request(2, "Use only the new bounded catalog.");
    replacement.admitted_catalog = vec![capability("skill.plan", "plan", true)];
    recompute_request_digests(&mut replacement).expect("replacement digests");
    let staged = activation
        .apply(
            replacement,
            &binding(),
            &resident_context(),
            "runner-1",
            true,
        )
        .expect("active-turn update stages");
    assert!(staged.staged_for_next_turn);
    assert!(activation.current_prompt().contains("skill.review"));
    activation.activate_staged_for_next_turn();
    assert!(activation.current_prompt().contains("skill.plan"));
    assert!(!activation.current_prompt().contains("skill.review"));
    assert!(
        activation
            .current_prompt()
            .contains("Pinned body for skill.plan.")
    );
}

#[test]
fn admission_replay_identity_digests_and_catalog_collisions_fail_closed() {
    let mut activation = WorkspaceCapabilityActivation::new("base prompt".to_string());
    let accepted = activation
        .apply(
            request(2, "Use version two."),
            &binding(),
            &resident_context(),
            "runner-1",
            false,
        )
        .expect("initial set is accepted");
    let replay = activation
        .apply(
            request(2, "Use version two."),
            &binding(),
            &resident_context(),
            "runner-1",
            false,
        )
        .expect("exact replay is idempotent");
    assert!(replay.idempotent);
    assert_eq!(
        replay.effective_catalog_digest,
        accepted.effective_catalog_digest
    );
    assert_eq!(replay.replay_cursor, accepted.replay_cursor);
    assert!(!replay.staged_for_next_turn);

    for mutate in [
        |request: &mut ApplyWorkspaceCapabilitySet| request.organization_id = "org-2".to_string(),
        |request: &mut ApplyWorkspaceCapabilitySet| {
            request.workspace_id = "workspace-2".to_string();
        },
        |request: &mut ApplyWorkspaceCapabilitySet| {
            request.runner_session_id = "runner-2".to_string();
        },
        |request: &mut ApplyWorkspaceCapabilitySet| request.runtime_generation = 8,
        |request: &mut ApplyWorkspaceCapabilitySet| request.activation_generation = 1,
        |request: &mut ApplyWorkspaceCapabilitySet| {
            request
                .admitted_catalog
                .push(capability("skill.review", "review-two", true));
        },
        |request: &mut ApplyWorkspaceCapabilitySet| {
            request
                .admitted_catalog
                .push(capability("skill.plan", "review", true));
        },
        |request: &mut ApplyWorkspaceCapabilitySet| {
            request.admitted_catalog[0].qualified_id = "native.shell".to_string();
        },
        |request: &mut ApplyWorkspaceCapabilitySet| {
            request.admitted_catalog[0].pinned_prompt_only = false;
        },
        |request: &mut ApplyWorkspaceCapabilitySet| {
            request.admitted_catalog[0].entry_digest = "sha256:bad".to_string();
        },
    ] {
        let mut invalid = request(3, "invalid admission");
        mutate(&mut invalid);
        if invalid.admitted_catalog[0].entry_digest != "sha256:bad" {
            recompute_request_digests(&mut invalid).expect("recompute invalid vector");
        }
        assert!(
            activation
                .apply(invalid, &binding(), &resident_context(), "runner-1", false)
                .is_err()
        );
    }

    assert!(
        activation
            .apply(
                request(2, "changed same generation"),
                &binding(),
                &resident_context(),
                "runner-1",
                false
            )
            .is_err()
    );
}

#[test]
fn idempotent_replay_reports_whether_the_set_is_staged() {
    let mut activation = WorkspaceCapabilityActivation::new("base prompt".to_string());
    let staged_request = request(2, "Use version two next turn.");
    let staged = activation
        .apply(
            staged_request.clone(),
            &binding(),
            &resident_context(),
            "runner-1",
            true,
        )
        .expect("set is staged");
    assert!(staged.staged_for_next_turn);

    let replay = activation
        .apply(
            staged_request,
            &binding(),
            &resident_context(),
            "runner-1",
            false,
        )
        .expect("staged set replay is idempotent");
    assert!(replay.idempotent);
    assert!(replay.staged_for_next_turn);
}

#[test]
fn admission_schema_rejects_unknown_executable_fields() {
    let mut top_level = serde_json::to_value(request(1, "Follow workspace rules."))
        .expect("serialize test request");
    top_level
        .as_object_mut()
        .expect("request object")
        .insert("executable_tools".to_string(), serde_json::json!([]));
    assert!(serde_json::from_value::<ApplyWorkspaceCapabilitySet>(top_level).is_err());

    let mut catalog_entry = serde_json::to_value(request(1, "Follow workspace rules."))
        .expect("serialize test request");
    catalog_entry["admitted_catalog"][0]
        .as_object_mut()
        .expect("catalog entry object")
        .insert("provided_tools".to_string(), serde_json::json!([]));
    assert!(serde_json::from_value::<ApplyWorkspaceCapabilitySet>(catalog_entry).is_err());
}

#[test]
fn failed_provider_install_does_not_commit_generation_or_poison_retry() {
    let mut activation = WorkspaceCapabilityActivation::new("base prompt".to_string());
    let requested = request(4, "Install this exact capability set.");

    let abandoned = activation
        .prepare(
            requested.clone(),
            &binding(),
            &resident_context(),
            "runner-1",
        )
        .expect("request validates before provider installation");
    assert!(!abandoned.is_idempotent());
    drop(abandoned);

    let retry = activation
        .prepare(
            requested.clone(),
            &binding(),
            &resident_context(),
            "runner-1",
        )
        .expect("same generation remains retryable after installation failure");
    assert!(!retry.is_idempotent());
    let receipt = activation.commit(retry, &binding(), false);
    assert!(!receipt.idempotent);
    assert_eq!(receipt.activation_generation, 4);

    let replay = activation
        .prepare(requested, &binding(), &resident_context(), "runner-1")
        .expect("committed request is idempotent");
    assert!(replay.is_idempotent());
}
