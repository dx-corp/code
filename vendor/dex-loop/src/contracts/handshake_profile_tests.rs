use super::{ThreadContextProfile, ThreadId};

#[test]
fn handshake_profile_survives_serialization_without_a_caller_selected_override() {
    for (namespace, expected) in [
        (
            ThreadId::HANDSHAKE_COORDINATOR_NAMESPACE,
            ThreadContextProfile::HandshakeCoordinator,
        ),
        (
            ThreadId::HANDSHAKE_CONSULTATION_NAMESPACE,
            ThreadContextProfile::HandshakeConsultation,
        ),
    ] {
        let thread = ThreadId {
            org: "org".into(),
            workspace: "ws".into(),
            thread: format!("{namespace}0123456789abcdef0123456789abcdef"),
        };
        let replay: ThreadId =
            serde_json::from_str(&serde_json::to_string(&thread).unwrap()).unwrap();
        assert_eq!(replay.context_profile(), expected);
    }
}

#[test]
fn malformed_reserved_ids_stay_restricted_instead_of_becoming_workspace_contexts() {
    for (id, expected) in [
        (
            "dex_handshake_coordinator_",
            ThreadContextProfile::HandshakeCoordinator,
        ),
        (
            "dex_handshake_consultation_invalid",
            ThreadContextProfile::HandshakeConsultation,
        ),
        ("ordinary", ThreadContextProfile::Workspace),
        (
            "thread:handshake:canonical",
            ThreadContextProfile::HandshakeCoordinator,
        ),
        (
            "thread:handshake-private:0123456789abcdef0123456789abcdef",
            ThreadContextProfile::HandshakeConsultation,
        ),
        (
            "thread:handshake-private:malformed",
            ThreadContextProfile::HandshakeConsultation,
        ),
    ] {
        let thread = ThreadId {
            org: "org".into(),
            workspace: "ws".into(),
            thread: id.into(),
        };
        assert_eq!(thread.context_profile(), expected);
    }
}
