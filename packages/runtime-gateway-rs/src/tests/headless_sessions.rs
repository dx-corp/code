#[allow(unused_imports)]
use super::*;

#[test]
pub(crate) fn hosted_runner_switches_agree_and_reject_unknown_values() {
    let _guard = ENV_LOCK.blocking_lock();
    // auth::hosted_runner_profile also reads MAESTRO_RUNNER_KIND / MAESTRO_PROFILE,
    // which are not in A2A_PLATFORM_ENV_NAMES; clear them too so a leaked ambient
    // value cannot decide the profile.
    const EXTRA: &[&str] = &["MAESTRO_RUNNER_KIND", "MAESTRO_PROFILE"];
    let snapshot = snapshot_env(A2A_PLATFORM_ENV_NAMES);
    let extra_snapshot = snapshot_env(EXTRA);
    clear_env(A2A_PLATFORM_ENV_NAMES);
    clear_env(EXTRA);

    // Affirmative values engage both the platform auto-registration switch
    // (a2a_platform_registration.rs, via env_bool) and the JWT strict profile
    // (auth::hosted_runner_profile, now routed through the same env_bool). The
    // two agree.
    for value in ["1", "true", "yes", "on"] {
        clear_env(A2A_PLATFORM_ENV_NAMES);
        env::set_var("MAESTRO_HOSTED_RUNNER_MODE", value);
        assert!(
            a2a_platform_registration_enabled(),
            "registration should be on for {value:?}"
        );
        assert!(
            crate::auth::hosted_runner_profile(),
            "jwt strict profile should be on for {value:?}"
        );
    }

    // Unknown / negative values leave both off. On the old truthy_env,
    // "disabled"/"none"/"flase"/"2"/"enabled" turned platform registration ON
    // while auth.rs's strict matches! left the JWT profile OFF. This assertion
    // fails on the old code (registration was true for "disabled") and pins that
    // the two switches now agree.
    for value in ["", "disabled", "none", "flase", "2", "enabled"] {
        clear_env(A2A_PLATFORM_ENV_NAMES);
        env::set_var("MAESTRO_HOSTED_RUNNER_MODE", value);
        assert!(
            !a2a_platform_registration_enabled(),
            "registration must stay off for {value:?}"
        );
        assert_eq!(
            a2a_platform_registration_enabled(),
            crate::auth::hosted_runner_profile(),
            "registration and jwt strict profile must agree for {value:?}"
        );
    }

    restore_env(extra_snapshot);
    restore_env(snapshot);
}

#[test]
pub(crate) fn assistant_output_from_jsonl_records_maestro_headless_tool_events() {
    let jsonl = r#"
{"type":"item","subtype":"tool_call","data":{"toolCallId":"collab-call-2","toolName":"codex.subagent.wait","args":{"codexTool":"wait","receiverThreadIds":["thread-child"]}}}
{"type":"item","subtype":"tool_result","data":{"toolCallId":"collab-call-2","toolName":"codex.subagent.wait","result":{"role":"toolResult","toolCallId":"collab-call-2","toolName":"codex.subagent.wait","content":[{"type":"text","text":"wait completed"}],"isError":false,"timestamp":1},"isError":false}}
{"type":"turn","phase":"start","role":"assistant","turnId":"turn-1"}
{"type":"item","subtype":"message_complete","turnId":"turn-1","data":{"text":"assistant text","stopReason":"stop"}}
{"type":"turn","phase":"end","role":"assistant","turnId":"turn-1"}
"#;
    let output = assistant_output_from_jsonl(jsonl).expect("assistant output");

    assert_eq!(output.tool_events.len(), 2);
    assert_eq!(output.tool_events[0].tool_name, "codex.subagent.wait");
    assert_eq!(
        output.tool_events[0].args["codexWorkGraph"]["childRuns"][0]["childRunId"],
        "codex-thread:thread-child"
    );
    assert_eq!(
        output.tool_events[0].args["codexWorkGraph"]["toolCallId"],
        "collab-call-2"
    );
    assert_eq!(
        output.tool_events[0].args["codexWorkGraph"]["childRuns"][0]["edgeId"],
        "collab-call-2:0:wait:codex-thread:thread-child"
    );
    assert_eq!(output.tool_events[1].is_error, Some(false));
    assert_eq!(
        output.tool_events[1].result["content"][0]["text"],
        "wait completed"
    );
}

#[test]
pub(crate) fn hosted_jwt_requires_algorithm_expiration_audience_and_issuer() {
    let _guard = ENV_LOCK.blocking_lock();
    let names: &[&str] = &[
        "MAESTRO_JWT_SECRET",
        "MAESTRO_JWT_ALG",
        "MAESTRO_JWT_AUD",
        "MAESTRO_JWT_ISS",
        "MAESTRO_PROFILE",
        "MAESTRO_WEB_PROFILE",
        "MAESTRO_HOSTED_RUNNER_MODE",
        "MAESTRO_HOSTED_RUNNER",
        "MAESTRO_RUNNER_KIND",
    ];
    let snapshot = snapshot_env(names);
    env::set_var("MAESTRO_JWT_SECRET", "shared-secret");
    env::set_var("MAESTRO_JWT_ALG", "HS256");
    env::set_var("MAESTRO_JWT_AUD", "evalops.maestro");
    env::set_var("MAESTRO_JWT_ISS", "evalops.platform");
    env::set_var("MAESTRO_PROFILE", "production");
    env::remove_var("MAESTRO_WEB_PROFILE");
    env::remove_var("MAESTRO_HOSTED_RUNNER_MODE");
    env::remove_var("MAESTRO_HOSTED_RUNNER");
    env::remove_var("MAESTRO_RUNNER_KIND");

    let base = serde_json::json!({
        "sub": "hosted-user",
        "scope": "maestro:write",
        "exp": now_secs() + 60,
        "aud": "evalops.maestro",
        "iss": "evalops.platform",
    });
    assert!(jwt_principal(&hs256_signed_claims(b"shared-secret", base.clone())).is_some());

    let mut missing_exp = base.clone();
    missing_exp
        .as_object_mut()
        .expect("claims object")
        .remove("exp");
    let mut wrong_audience = base.clone();
    wrong_audience["aud"] = serde_json::json!("other-service");
    let mut wrong_issuer = base.clone();
    wrong_issuer["iss"] = serde_json::json!("other-issuer");
    for claims in [missing_exp, wrong_audience, wrong_issuer] {
        assert!(
            jwt_principal(&hs256_signed_claims(b"shared-secret", claims)).is_none(),
            "hosted JWT invariant violation must reject"
        );
    }

    env::remove_var("MAESTRO_JWT_ALG");
    assert!(
        jwt_principal(&hs256_signed_claims(b"shared-secret", base)).is_none(),
        "hosted JWT must not default its algorithm"
    );

    restore_env(snapshot);
}

#[test]
pub(crate) fn hosted_runner_signals_enable_strict_jwt_validation() {
    let _guard = ENV_LOCK.blocking_lock();
    let names: &[&str] = &[
        "MAESTRO_JWT_SECRET",
        "MAESTRO_JWT_ALG",
        "MAESTRO_JWT_AUD",
        "MAESTRO_JWT_ISS",
        "MAESTRO_PROFILE",
        "MAESTRO_WEB_PROFILE",
        "MAESTRO_HOSTED_RUNNER_MODE",
        "MAESTRO_HOSTED_RUNNER",
        "MAESTRO_RUNNER_KIND",
    ];
    let snapshot = snapshot_env(names);
    env::set_var("MAESTRO_JWT_SECRET", "shared-secret");
    env::set_var("MAESTRO_JWT_ALG", "HS256");
    env::set_var("MAESTRO_JWT_AUD", "evalops.maestro");
    env::set_var("MAESTRO_JWT_ISS", "evalops.platform");
    let base = serde_json::json!({
        "sub": "hosted-user",
        "exp": now_secs() + 60,
        "aud": "evalops.maestro",
        "iss": "evalops.platform",
    });
    let mut missing_exp = base.clone();
    missing_exp
        .as_object_mut()
        .expect("claims object")
        .remove("exp");
    for (name, value) in [
        ("MAESTRO_HOSTED_RUNNER_MODE", "1"),
        ("MAESTRO_HOSTED_RUNNER", "true"),
        ("MAESTRO_RUNNER_KIND", "hosted"),
        ("MAESTRO_PROFILE", "hosted-runner"),
    ] {
        for signal in [
            "MAESTRO_PROFILE",
            "MAESTRO_WEB_PROFILE",
            "MAESTRO_HOSTED_RUNNER_MODE",
            "MAESTRO_HOSTED_RUNNER",
            "MAESTRO_RUNNER_KIND",
        ] {
            env::remove_var(signal);
        }
        env::set_var(name, value);
        assert!(
            jwt_principal(&hs256_signed_claims(b"shared-secret", missing_exp.clone())).is_none(),
            "missing exp must fail for {name}"
        );
        assert!(jwt_principal(&hs256_signed_claims(b"shared-secret", base.clone())).is_some());
    }
    restore_env(snapshot);
}

#[test]
pub(crate) fn desktop_headless_handshake_uses_the_native_owner_protocol() {
    let hello = codex_bridge::codex_headless_hello();
    let _: maestro_local_host::headless::ToAgentMessage =
        serde_json::from_value(hello.clone()).unwrap();
    assert!(
        maestro_local_host::headless::messages::SUPPORTED_CLIENT_PROTOCOL_VERSIONS
            .contains(&hello["protocol_version"].as_str().unwrap())
    );
    assert_eq!(hello["role"], "controller");
    assert_eq!(hello["capabilities"]["server_requests"][0], "approval");
}
