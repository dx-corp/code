//! Hosted Code delegates its sole executable tool to the Platform owner.
//! The admission coordinate and private credential are never command inputs.
use super::*;
use maestro_dex_host::dex_loop::{ExecutorKind, GovernanceClass, ToolName, ToolSpec};
use maestro_runtime_contracts::native_code::tool_idempotency_key;
use prost::Message;

#[path = "governed_native_wire.rs"]
mod wire;

const TOOL_SERVICE: &str = "toolexecution.v1.ToolExecutionService";
const ADMISSION_SERVICE: &str = "deixicpublic.v1.NativeCodeService";
const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;

#[derive(Clone)]
pub(crate) struct Client {
    http: reqwest::Client,
    credentials: crate::native_credentials::Credentials,
}

impl Client {
    pub(crate) fn from_env() -> Result<Self, String> {
        let credentials = crate::native_credentials::Credentials::from_env()?;
        let http = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(35))
            .build()
            .map_err(|_| "Hosted tool transport is unavailable")?;
        Ok(Self { http, credentials })
    }

    async fn rpc(&self, service: &str, method: &str, body: &Value) -> Result<Value, String> {
        let descriptor = self.credentials.current(&self.http).await?;
        let base = if service == ADMISSION_SERVICE {
            &descriptor.tool_execution.platform_base_url
        } else {
            &descriptor.tool_execution.base_url
        };
        let mut url = reqwest::Url::parse(base).map_err(|_| "Hosted tool owner URL is invalid")?;
        url.set_path(&format!("/{service}/{method}"));
        let mut response = self
            .http
            .post(url)
            .bearer_auth(&descriptor.tool_execution.token)
            .header("connect-protocol-version", "1")
            .header("x-organization-id", &descriptor.organization_id)
            .header("x-workspace-id", &descriptor.workspace_id)
            .json(body)
            .send()
            .await
            .map_err(|_| "Hosted tool owner request failed")?;
        if !response.status().is_success() {
            return Err(format!(
                "Hosted tool owner rejected the request ({})",
                response.status().as_u16()
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| "Hosted tool owner response failed")?
        {
            if bytes.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
                return Err("Hosted tool response exceeds its bound".into());
            }
            bytes.extend(chunk);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| "Hosted tool owner returned an invalid response".into())
    }

    pub(crate) async fn admission(
        &self,
        id: &str,
        auth: &AuthContext,
    ) -> Result<Admission, String> {
        if !self.credentials.matches_principal(auth) {
            return Err("Hosted credential tenant does not match the caller".into());
        }
        if id.is_empty() || id.len() > 256 {
            return Err("Invalid Platform admission coordinate".into());
        }
        let descriptor = self.credentials.current(&self.http).await?;
        let mut url = reqwest::Url::parse(&descriptor.tool_execution.platform_base_url)
            .map_err(|_| "Hosted admission owner URL is invalid")?;
        url.set_path(&format!("/{ADMISSION_SERVICE}/GetNativeCodeTurnAdmission"));
        let admission = wire::read(
            &self.http,
            url,
            &descriptor.tool_execution.token,
            wire::GetAdmissionRequest {
                organization_id: descriptor.organization_id,
                workspace_id: descriptor.workspace_id,
                admission_id: id.into(),
            },
        )
        .await?;
        if !self
            .credentials
            .matches_runner(&admission.runner_session_id)
        {
            return Err("Hosted admission runtime does not match the credential owner".into());
        }
        Ok(admission)
    }

    pub(crate) async fn execute(
        &self,
        admission: &Admission,
        step: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let linkage = admission.linkage(step);
        let key = tool_idempotency_key(
            &admission.organization_id,
            &admission.workspace_id,
            &admission.admission_id,
            step,
        );
        let body = serde_json::json!({"linkage":linkage,"tool":{"namespace":"computer","name":"computer.shell","capability":"computer.shell","operation":"shell","mutatesResource":true,"idempotent":false},"arguments":args,"idempotencyKey":key});
        let response = self.rpc(TOOL_SERVICE, "ExecuteTool", &body).await?;
        let execution = response["execution"].clone();
        validate_execution(&execution, admission, step, args)?;
        Ok(execution)
    }

    pub(crate) async fn observe(
        &self,
        id: &str,
        admission: &Admission,
        step: &str,
        args: &Value,
    ) -> Result<Value, String> {
        let body = self.rpc(TOOL_SERVICE, "GetToolExecution", &serde_json::json!({"id":id,"organizationId":admission.organization_id,"workspaceId":admission.workspace_id,"waitTimeoutMs":30000})).await?;
        let execution = body["execution"].clone();
        validate_execution(&execution, admission, step, args)?;
        if execution["id"].as_str() != Some(id) {
            return Err("Hosted execution identity changed".into());
        }
        Ok(execution)
    }
}

#[derive(Clone, PartialEq, Deserialize, Message)]
#[serde(rename_all = "camelCase")]
pub(crate) struct Admission {
    #[prost(string, tag = "1")]
    pub(crate) admission_id: String,
    #[prost(string, tag = "2")]
    pub(crate) organization_id: String,
    #[prost(string, tag = "3")]
    pub(crate) workspace_id: String,
    #[prost(string, tag = "4")]
    pub(crate) subject: String,
    #[prost(string, tag = "8")]
    runner_session_id: String,
    #[prost(string, tag = "5")]
    application_id: String,
    #[prost(string, tag = "6")]
    agent_id: String,
    #[prost(string, tag = "7")]
    actor_id: String,
    #[prost(string, tag = "12")]
    gateway_epoch: String,
    #[prost(string, tag = "13")]
    pub(crate) native_session_id: String,
    #[prost(string, tag = "14")]
    pub(crate) native_session_created_at: String,
    #[prost(string, tag = "15")]
    native_turn_id: String,
    #[prost(string, tag = "16")]
    pub(crate) request_sha256: String,
    #[prost(string, tag = "17")]
    state: String,
    #[prost(string, tag = "18")]
    expires_at: String,
}

pub(crate) struct ExpectedTurn<'a> {
    pub(crate) epoch: &'a str,
    pub(crate) session: &'a str,
    pub(crate) created_at: &'a str,
    pub(crate) turn: &'a str,
    pub(crate) digest: &'a str,
}

impl Admission {
    pub(crate) fn validate(
        &self,
        id: &str,
        auth: &AuthContext,
        expected: ExpectedTurn<'_>,
    ) -> Result<(), String> {
        let expiry = chrono::DateTime::parse_from_rfc3339(&self.expires_at)
            .map_err(|_| "Hosted admission expiry is invalid")?;
        if self.admission_id != id
            || Some(self.organization_id.as_str()) != auth.organization_id.as_deref()
            || Some(self.workspace_id.as_str()) != auth.workspace_id.as_deref()
            || Some(self.subject.as_str()) != auth.subject.as_deref()
            || self.application_id != "deixic"
            || self.agent_id != "maestro"
            || self.actor_id.is_empty()
            || self.gateway_epoch != expected.epoch
            || self.native_session_id != expected.session
            || self.native_session_created_at != expected.created_at
            || self.native_turn_id != expected.turn
            || self.request_sha256 != expected.digest
            || self.state != "active"
            || expiry <= chrono::Utc::now()
        {
            return Err("Hosted admission does not match this exact native turn owner".into());
        }
        Ok(())
    }
    fn linkage(&self, step: &str) -> Value {
        serde_json::json!({"organizationId":self.organization_id,"workspaceId":self.workspace_id,"applicationId":self.application_id,"agentId":self.agent_id,"actorId":self.actor_id,"runId":"","channelId":"","correlationId":self.native_turn_id,"stepId":step,"surfaceType":"SURFACE_MAESTRO","nativeCodeAdmissionId":self.admission_id})
    }
}

fn validate_execution(
    execution: &Value,
    admission: &Admission,
    step: &str,
    args: &Value,
) -> Result<(), String> {
    let linkage = &execution["linkage"];
    for (key, expected) in admission.linkage(step).as_object().expect("linkage object") {
        if expected == "" && linkage.get(key).is_none() {
            continue;
        }
        if linkage.get(key) != Some(expected) {
            return Err("Hosted execution linkage does not match the native owner".into());
        }
    }
    if execution["id"].as_str().is_none_or(str::is_empty)
        || execution["tool"]["namespace"] != "computer"
        || execution["tool"]["name"] != "computer.shell"
        || execution["tool"]["capability"] != "computer.shell"
        || execution["arguments"] != *args
        || execution["idempotencyKey"]
            != tool_idempotency_key(
                &admission.organization_id,
                &admission.workspace_id,
                &admission.admission_id,
                step,
            )
    {
        return Err("Hosted execution does not match the exact tool call".into());
    }
    Ok(())
}

pub(crate) fn spec() -> ToolSpec {
    ToolSpec { name:ToolName::new("computer.shell"),label:"Run a governed workspace command".into(),description:"Run a shell command in /workspace through Platform tool policy and approvals. Read files, edit code, and run checks with shell commands; the command environment contains no resident credentials.".into(),schema:serde_json::json!({"type":"object","additionalProperties":false,"required":["command"],"properties":{"command":{"type":"string"},"timeoutMs":{"type":"integer","minimum":1000,"maximum":3600000}}}),read_only:false,core:true,governance:GovernanceClass::Plain,executor:ExecutorKind::Client }
}

pub(crate) fn state(execution: &Value) -> Result<&str, String> {
    match execution["state"].as_str() {
        Some(
            "TOOL_EXECUTION_STATE_ACCEPTED"
            | "TOOL_EXECUTION_STATE_POLICY_EVALUATING"
            | "TOOL_EXECUTION_STATE_WAITING_APPROVAL"
            | "TOOL_EXECUTION_STATE_RUNNING"
            | "TOOL_EXECUTION_STATE_SUCCEEDED"
            | "TOOL_EXECUTION_STATE_FAILED"
            | "TOOL_EXECUTION_STATE_DENIED"
            | "TOOL_EXECUTION_STATE_CANCELLED",
        ) => Ok(execution["state"].as_str().unwrap()),
        _ => Err("Hosted execution has an unknown state".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn admission() -> Admission {
        serde_json::from_value(serde_json::json!({"admissionId":"admission","organizationId":"org","workspaceId":"workspace","subject":"human","runnerSessionId":"runner","applicationId":"deixic","agentId":"maestro","actorId":"human","gatewayEpoch":"epoch","nativeSessionId":"session","nativeSessionCreatedAt":"created","nativeTurnId":"turn","requestSha256":"digest","state":"active","expiresAt":"2100-01-01T00:00:00Z"})).unwrap()
    }
    #[test]
    fn admission_requires_exact_principal_native_turn_digest_and_live_state() {
        let auth = AuthContext {
            subject: Some("human".into()),
            organization_id: Some("org".into()),
            workspace_id: Some("workspace".into()),
            ..Default::default()
        };
        let expected = || ExpectedTurn {
            epoch: "epoch",
            session: "session",
            created_at: "created",
            turn: "turn",
            digest: "digest",
        };
        let owner = admission();
        owner.validate("admission", &auth, expected()).unwrap();
        for field in [
            "subject",
            "organizationId",
            "workspaceId",
            "gatewayEpoch",
            "nativeSessionId",
            "nativeSessionCreatedAt",
            "nativeTurnId",
            "requestSha256",
            "state",
            "applicationId",
        ] {
            let mut value = serde_json::to_value(serde_json::json!({"admissionId":"admission","organizationId":"org","workspaceId":"workspace","subject":"human","runnerSessionId":"runner","applicationId":"deixic","agentId":"maestro","actorId":"human","gatewayEpoch":"epoch","nativeSessionId":"session","nativeSessionCreatedAt":"created","nativeTurnId":"turn","requestSha256":"digest","state":"active","expiresAt":"2100-01-01T00:00:00Z"})).unwrap();
            value[field] = serde_json::json!("different");
            let changed: Admission = serde_json::from_value(value).unwrap();
            assert!(
                changed.validate("admission", &auth, expected()).is_err(),
                "{field}"
            );
        }
        let mut expired = owner;
        expired.expires_at = "2000-01-01T00:00:00Z".into();
        assert!(expired.validate("admission", &auth, expected()).is_err());
    }
    #[test]
    fn tool_observation_cannot_replace_linkage_input_or_execution() {
        let owner = admission();
        let args = serde_json::json!({"command":"pwd"});
        let value = serde_json::json!({"id":"execution","linkage":owner.linkage("step"),"tool":{"namespace":"computer","name":"computer.shell","capability":"computer.shell"},"arguments":args,"idempotencyKey":tool_idempotency_key("org","workspace","admission","step"),"state":"TOOL_EXECUTION_STATE_WAITING_APPROVAL"});
        validate_execution(&value, &owner, "step", &args).unwrap();
        let mut changed = value.clone();
        changed["linkage"]["workspaceId"] = serde_json::json!("other");
        assert!(validate_execution(&changed, &owner, "step", &args).is_err());
        changed = value.clone();
        changed["linkage"]["nativeCodeAdmissionId"] = serde_json::json!("other");
        assert!(validate_execution(&changed, &owner, "step", &args).is_err());
        changed = value.clone();
        changed["arguments"]["command"] = serde_json::json!("write code");
        assert!(validate_execution(&changed, &owner, "step", &args).is_err());
        changed = value;
        changed["state"] = serde_json::json!("unknown");
        assert!(state(&changed).is_err());
    }
}
