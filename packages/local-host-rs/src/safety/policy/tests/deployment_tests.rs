use super::*;
use crate::safety::disconnected_deployment_contract;

#[test]
fn disconnected_deployment_contract_reports_signed_destinations_without_credentials() {
    let _lock = test_env_guard();
    let fixture = MachineFixture::new();
    let mut profile = private_profile();
    profile.routes[0].credential_env = Some("CUSTOMER_MODEL_API_KEY".into());
    profile.authority = Some(DisconnectedAuthority {
        endpoint: "https://identity.customer.example".into(),
        credential_env: "CUSTOMER_CODE_ACCESS_TOKEN".into(),
        ca_pem: None,
    });
    fixture.install(profile);
    // Deliberately no credential provisioning or customer server. Inspection
    // must work before an operator grants credentials or network access.
    let contract = disconnected_deployment_contract().unwrap();
    let json = serde_json::to_value(contract).unwrap();
    assert_eq!(json["schemaVersion"], 1);
    assert_eq!(json["policy"]["policyVersion"], 1);
    assert_eq!(
        json["modelRoutes"][0]["endpoint"],
        "http://127.0.0.1:11434/v1"
    );
    assert_eq!(json["modelRoutes"][0]["credentialRequired"], true);
    assert_eq!(
        json["customerAuthority"]["endpoint"],
        "https://identity.customer.example"
    );
    assert_eq!(json["enrolledTools"], "customer_authority_required");
    assert_eq!(json["externalProcessNetwork"], "operator_enforced");
    assert_eq!(json["deploymentQualification"], "not_assessed");
    let serialized = json.to_string();
    assert!(!serialized.contains("CUSTOMER_MODEL_API_KEY"));
    assert!(!serialized.contains("CUSTOMER_CODE_ACCESS_TOKEN"));
    assert!(!serialized.contains("caPem"));
    assert!(vendor_network_disabled());
}

#[test]
fn disconnected_deployment_contract_requires_valid_installed_policy() {
    let _lock = test_env_guard();
    let fixture = MachineFixture::new();
    assert!(disconnected_deployment_contract().is_err());
    fixture.install(private_profile());
    assert!(
        disconnected_deployment_contract()
            .unwrap()
            .customer_authority
            .is_none()
    );
    let path = fixture.directory.path().join("managed-policy.json");
    let mut envelope: ManagedPolicyEnvelope =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    envelope.policy.disconnected.as_mut().unwrap().routes[0].endpoint =
        "https://unapproved.customer.example/v1".into();
    std::fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    assert!(disconnected_deployment_contract().is_err());
    assert!(vendor_network_disabled());
}

#[test]
fn disconnected_policy_uses_signed_machine_routes_and_rejects_tampering() {
    let _lock = test_env_guard();
    let fixture = MachineFixture::new();
    fixture.install(private_profile());
    assert!(vendor_network_disabled());
    assert!(
        disconnected_route("ollama/customer-model")
            .unwrap()
            .is_some()
    );
    assert!(disconnected_route("openai/gpt-5.5").is_err());
    assert!(require_vendor_network().is_err());
    let path = fixture.directory.path().join("managed-policy.json");
    let mut envelope: ManagedPolicyEnvelope =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    envelope.policy.disconnected.as_mut().unwrap().routes[0].endpoint =
        "https://api.openai.com/v1".into();
    std::fs::write(&path, serde_json::to_vec(&envelope).unwrap()).unwrap();
    assert!(disconnected_policy().is_err());
    assert!(vendor_network_disabled());
    assert!(check_tool_allowed("read").is_some());
}
