use super::*;
use crate::a2a::{
    a2a_push_notification_tenant_headers, a2a_push_select_pinned_addr, a2a_task_visible_to_auth,
};
use crate::chat::system_prompt_from_chat;
use crate::session_messaging::{
    SendError, SendOutcome, delete_session_inbox, persist_message_store_snapshot,
    send_session_message_inner,
};
use std::fs;
use std::io::{Read, Write};
use std::net::TcpListener as StdTcpListener;

pub(crate) static ENV_LOCK: Mutex<()> = Mutex::const_new(());

#[path = "interaction_mode_tests.rs"]
mod interaction_mode_tests;

const A2A_PLATFORM_ENV_NAMES: &[&str] = &[
    "MAESTRO_A2A_PLATFORM_REGISTER",
    "MAESTRO_A2A_PLATFORM_AUTO_REGISTER",
    "MAESTRO_HOSTED_RUNNER_MODE",
    "MAESTRO_HOSTED_RUNNER",
    "MAESTRO_AGENT_REGISTRY_SERVICE_URL",
    "AGENT_REGISTRY_SERVICE_URL",
    "MAESTRO_AGENT_REGISTRY_URL",
    "AGENT_REGISTRY_BASE_URL",
    "PLATFORM_AGENT_REGISTRY_URL",
    "MAESTRO_PLATFORM_BASE_URL",
    "MAESTRO_EVALOPS_BASE_URL",
    "EVALOPS_BASE_URL",
    "MAESTRO_AGENT_REGISTRY_SERVICE_TOKEN",
    "AGENT_REGISTRY_SERVICE_TOKEN",
    "MAESTRO_AGENT_REGISTRY_TOKEN",
    "AGENT_REGISTRY_TOKEN",
    "MAESTRO_EVALOPS_ACCESS_TOKEN",
    "EVALOPS_TOKEN",
    "MAESTRO_AGENT_REGISTRY_ORG_ID",
    "AGENT_REGISTRY_ORGANIZATION_ID",
    "AGENT_REGISTRY_ORG_ID",
    "MAESTRO_EVALOPS_ORG_ID",
    "EVALOPS_ORGANIZATION_ID",
    "EVALOPS_ORG_ID",
    "MAESTRO_ENTERPRISE_ORG_ID",
    "MAESTRO_AGENT_REGISTRY_WORKSPACE_ID",
    "AGENT_REGISTRY_WORKSPACE_ID",
    "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID",
    "MAESTRO_EVALOPS_WORKSPACE_ID",
    "EVALOPS_WORKSPACE_ID",
    "MAESTRO_WORKSPACE_ID",
    "MAESTRO_A2A_PUBLIC_URL",
    "MAESTRO_CONTROL_PUBLIC_URL",
    "MAESTRO_A2A_URL",
    "MAESTRO_CONTROL_URL",
    "MAESTRO_A2A_AGENT_ID",
    "MAESTRO_AGENT_ID",
    "EVALOPS_AGENT_ID",
    "MAESTRO_A2A_AGENT_NAME",
    "MAESTRO_AGENT_NAME",
    "MAESTRO_A2A_AGENT_DESCRIPTION",
    "MAESTRO_AGENT_DESCRIPTION",
    "MAESTRO_A2A_AGENT_TYPE",
    "MAESTRO_A2A_OWNER_ID",
    "EVALOPS_USER_ID",
    "MAESTRO_A2A_TRACEPARENT",
    "MAESTRO_PLATFORM_TRACEPARENT",
    "TRACEPARENT",
    "MAESTRO_A2A_TRACESTATE",
    "MAESTRO_PLATFORM_TRACESTATE",
    "TRACESTATE",
    "MAESTRO_A2A_INTERNAL_URL",
    "MAESTRO_CONTROL_INTERNAL_URL",
    "MAESTRO_A2A_AGENT_CARD_URL",
    "MAESTRO_A2A_PLATFORM_HEARTBEAT_INTERVAL_MS",
    "MAESTRO_AGENT_REGISTRY_HEARTBEAT_INTERVAL_MS",
    "MAESTRO_AGENT_REGISTRY_TIMEOUT_MS",
    "AGENT_REGISTRY_SERVICE_TIMEOUT_MS",
    "MAESTRO_A2A_CURRENT_OBJECTIVE_IDS",
    "MAESTRO_A2A_MAX_CONCURRENT_OBJECTIVES",
    "MAESTRO_SWARM_MAX_TEAMMATES",
    "MAESTRO_A2A_PLATFORM_SURFACE",
    "MAESTRO_A2A_PLATFORM_SURFACE_TYPE",
    "MAESTRO_A2A_PLATFORM_STATUS",
    "TRACEPARENT",
    "TRACE_PARENT",
    "MAESTRO_TRACEPARENT",
    "TRACESTATE",
    "TRACE_STATE",
    "MAESTRO_TRACESTATE",
    "MAESTRO_REMOTE_RUNNER_SESSION_ID",
    "MAESTRO_RUNNER_SESSION_ID",
    "REMOTE_RUNNER_SESSION_ID",
    "PORT",
    "MAESTRO_CONTROL_HOST",
];
const RUNTIME_GATEWAY_ENV_NAMES: &[&str] = &[
    "MAESTRO_CONTROL_HOST",
    "MAESTRO_WEB_API_KEY",
    "MAESTRO_WEB_API_KEY_SCOPES",
    "MAESTRO_WEB_REQUIRE_KEY",
    "MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN",
    "MAESTRO_JWT_SECRET",
    "MAESTRO_JWT_JWKS_URL",
    "MAESTRO_JWT_ALG",
    "MAESTRO_JWT_AUD",
    "MAESTRO_JWT_ISS",
    "MAESTRO_PROFILE",
    "MAESTRO_WEB_PROFILE",
    "MAESTRO_HOSTED_RUNNER_MODE",
    "MAESTRO_HOSTED_RUNNER",
    "MAESTRO_RUNNER_KIND",
    "MAESTRO_ORGANIZATION_ID",
    "MAESTRO_EVALOPS_ORG_ID",
    "MAESTRO_WORKSPACE_ID",
    "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID",
    "MAESTRO_EVALOPS_WORKSPACE_ID",
    "NODE_ENV",
    "PORT",
];
const CORS_ENV_NAMES: &[&str] = &["MAESTRO_WEB_ORIGIN", "MAESTRO_WEB_ORIGINS"];
const A2A_PUSH_AUTH_ENV_NAMES: &[&str] = &[
    "MAESTRO_PLATFORM_A2A_CALLBACK_TOKEN",
    "MAESTRO_A2A_CALLBACK_TOKEN",
    "MAESTRO_ORGANIZATION_ID",
    "MAESTRO_EVALOPS_ORG_ID",
    "MAESTRO_WORKSPACE_ID",
    "MAESTRO_REMOTE_RUNNER_WORKSPACE_ID",
    "MAESTRO_EVALOPS_WORKSPACE_ID",
];

#[derive(Debug)]
pub(crate) struct CapturedHttpRequest {
    request_line: String,
    headers: HashMap<String, String>,
    body: Value,
}

pub(crate) struct TestDir {
    path: PathBuf,
}

const JWKS_TEST_ENV_NAMES: &[&str] = &[
    "MAESTRO_JWT_JWKS",
    "MAESTRO_JWT_JWKS_URL",
    "MAESTRO_JWT_ALG",
    "MAESTRO_JWT_AUD",
    "MAESTRO_JWT_ISS",
    "MAESTRO_JWT_SECRET",
    "MAESTRO_PROFILE",
    "MAESTRO_WEB_PROFILE",
    "MAESTRO_HOSTED_RUNNER_MODE",
    "MAESTRO_HOSTED_RUNNER",
    "MAESTRO_RUNNER_KIND",
];

// Throwaway 2048-bit RSA key for these tests only, checked in so tokens can be
// signed without a network or key-generation dependency.
// nosemgrep: generic.secrets.security.detected-private-key.detected-private-key
const TEST_RSA_PRIVATE_KEY_PEM: &str = r"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDSPe792Sabb2ly
LMi7j5tRTebUUK5SL8AbgCu7osi5dl/zlJNtffBlJrMmBR6kIjEhvFsX5flMp0Nm
+SiLfxc80zkorOd1JTpYlEUPNjKnVkBfAvVQCnsLVZlXz3Wwpp1vCJwWD97kviMj
zmijsX21vgANEA7vzlsP8kBP/wBLI2V6xFP8w/NbZDhPj3l4JIfGvo+0DIivO9fm
H06en1orNjShu5T1zqAe+6gciqeHGfhe/3IWhrbctMlJR2kxHidmovDd4GpnXQfU
HIJNlepc6W/LMuZWUj7H8O3oryx/JSGCCWlxUNpfHQWc9I62rpn3zhb1ggzQLbYy
swgmVbfbAgMBAAECggEAZQizXdVpur+/PkmsS4p3OwrDV5vQMhnVacHeAmV3rbzn
3pAziyY/DPUcmbRTJdByqQIyCpmPhRlKiGVLaUIxoh7ltJjnAEJcOC5Ew8spa4ZF
GAO9bPIkcG157Bt8NODU/oN2Mxn8ZRPEolPysFu/DERbFOv3KaIS2+ZwpqDmfLSO
BEQbfldIPG/CvgD67Y00s3rXDusxzLnroK+hiIQz/WoiLj2yCA2QTIY6Rr1X4AHp
6Xx9jMq2hQ4bv4jq2WA1uCWwxPF27tYAAsNOCBL58zmvQu75XowsP5SENTbOfK7o
a9a5lAcAvjCp/XpeDjYfvSmzDv2LQm+qWgnYxAvPeQKBgQD3TWX3yT7NH9dM6BvO
8aEm0uXtg6C35QzRuAyIYkIib9r5ogJNjpNcOWRkuAzaEtyA+dQIhHD1q1ImBxTQ
Vkc/dWOpkcjqjhAkH7QXXv76pHJwGQxUsAIU6ZeuUExrzLKNyGH0a0ohKMFH4AXV
9zRejD1whOS6tLcWg1h+6TBckwKBgQDZotwOQRzVNgLP/Sm7D/rLjAFq4aJx3vlv
TLgpRFo46usxHJrfVlw4Dp5n2k6+AuUyV1X47+eqASRmb8fHU5xeWZimpoOqQqlI
PzKjRmaSfHOlXS67QTUomTTOczfwVF/VBMebCsbS68Coa41VwnaULABdeUdQ7cc+
NR8FgbqMmQKBgQDz/yxtFuTck97kJVpSivq6CHkNJ8KpzdchEBtlcLTZr0z44cyt
4s8nvgR8j0821kczBcsbADlHWlo55OC3UXkIdnT3eDwomDP6wED6kiK2/wtd6IjP
Ab18DqE2Pkm4ToWY+C0Vb8n6/2/7z19SpY3I/0sbOjNGt0ixcLQeu0qY+wKBgGTc
8ol0qc0yg+kq1j1IsZ3GHB4Rxjxp70Yi0zLk5797OFcBf9FD7+dW9xkAdv/ezaQg
D8sYPFBwyRLkeT0qxcyAT5vkjh7JWDUQfQJorT70iJA5+F92YBGZt3x6r5ElOWi7
F1sGipDUC+zCM7VsM5KGNgEcJO4f1PhCnEbsEa35AoGAf7zOMUB0nwrOwL2kqXXv
TnOiAt4lnHRsmkMA5j1HOYHErNNHbiPchx1PPSCHn7+FuSdB776cIqKPU65NCcbE
vrlKRUStRoNCN/cKeqCJyIYMrwZVsydX9UJrMfs871NlK40G/8Ec1gMNBr1B2CjN
Tv5TrT/AZ3XKorUF90AEe/0=
-----END PRIVATE KEY-----
";

const TEST_JWKS_JSON: &str = r#"{"keys":[{"kty":"RSA","use":"sig","alg":"RS256","kid":"test-key-1","n":"0j3u_dkmm29pcizIu4-bUU3m1FCuUi_AG4Aru6LIuXZf85STbX3wZSazJgUepCIxIbxbF-X5TKdDZvkoi38XPNM5KKzndSU6WJRFDzYyp1ZAXwL1UAp7C1WZV891sKadbwicFg_e5L4jI85oo7F9tb4ADRAO785bD_JAT_8ASyNlesRT_MPzW2Q4T495eCSHxr6PtAyIrzvX5h9Onp9aKzY0obuU9c6gHvuoHIqnhxn4Xv9yFoa23LTJSUdpMR4nZqLw3eBqZ10H1ByCTZXqXOlvyzLmVlI-x_Dt6K8sfyUhgglpcVDaXx0FnPSOtq6Z984W9YIM0C22MrMIJlW32w","e":"AQAB"}]}"#;

#[path = "tests/a2a_rejection.rs"]
mod a2a_rejection;

#[path = "tests/turn_diffs.rs"]
mod turn_diff_tests;

#[path = "sessions_page_tests.rs"]
mod sessions_page_tests;

#[path = "pull_request_watch_api_tests.rs"]
mod pull_request_watch_api_tests;

#[path = "tests/native_headless_contract.rs"]
mod native_headless_contract;

#[path = "tests/session_persistence.rs"]
mod session_persistence_tests;

#[path = "tests/codex_bridge_shutdown.rs"]
mod codex_bridge_shutdown;

mod a2a_tasks;
mod session_messaging;
#[allow(unused_imports)]
pub(crate) use self::session_messaging::{
    messaging_session, messaging_state, response_body_text, session_message_for_target,
    session_messaging_test_state, tenant_session, test_app_state_with_sessions,
    test_session_record,
};
mod a2a_push;
mod auth_tokens;
#[allow(unused_imports)]
pub(crate) use self::auth_tokens::{
    auth_head, auth_test_config, bearer_head, identity_jwt_bearer_token, messaging_auth,
    rs256_signed_claims, rs256_signed_token, security_fix_token, set_jwks_test_env, tenant_auth,
};
mod a2a_cards;
mod capsules;
mod codex_bridge_cases;
#[allow(unused_imports)]
pub(crate) use self::capsules::{
    valid_browser_qa_capsule, valid_code_review_capsule, valid_code_writer_capsule,
};
mod platform_requests;
#[allow(unused_imports)]
pub(crate) use self::platform_requests::{
    capture_http_request, csrf_head, csrf_head_for_path, head_with_host, response_json,
    response_status, restore_env,
};
mod web_routes;
#[allow(unused_imports)]
pub(crate) use self::web_routes::security_fix_remote_env_snapshot;
mod a2a_messaging;
mod push_notifications;
mod registration;
#[allow(unused_imports)]
pub(crate) use self::push_notifications::server_websocket_json_values;
mod a2a_protocol;
mod headless_sessions;
mod test_support;
#[allow(unused_imports)]
pub(crate) use self::test_support::{
    clear_env, hs256_signed_claims, inbox_of, now_secs, snapshot_env, tcp_stream_pair,
    unique_test_dir,
};
mod cli_daemon;
