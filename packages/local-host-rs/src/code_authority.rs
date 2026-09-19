//! Code-only live tool authority. Enrollment is global user state; repository
//! configuration cannot enable it or replace Identity authority. A protected
//! signed machine policy can select a customer-operated Identity endpoint.
use anyhow::{Context as _, Result, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    path::PathBuf,
    process::Stdio,
    sync::{Arc, OnceLock},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    sync::Mutex,
};

pub use maestro_runtime::CodeAuthorityDecision;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Enrollment {
    key_id: String,
}

#[cfg(test)]
type TestDecisions =
    Arc<std::sync::Mutex<std::collections::VecDeque<Result<CodeAuthorityDecision, String>>>>;

#[derive(Clone)]
pub(crate) struct CodeToolAuthority {
    record: PathBuf,
    expected: Option<(String, String, String)>,
    session_id: String,
    lock: Arc<Mutex<()>>,
    #[cfg(test)]
    test_decisions: Option<TestDecisions>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Challenge {
    challenge_id: String,
    client_data: String,
}

fn enrollment_path() -> Option<PathBuf> {
    dirs::home_dir().map(|p| p.join(".composer/code-device.json"))
}
fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|v| v.as_secs() as i64)
        .unwrap_or(i64::MAX)
}

// All executors in this process share the device counter ordering.
fn device_lock() -> Arc<Mutex<()>> {
    static LOCK: OnceLock<Arc<Mutex<()>>> = OnceLock::new();
    LOCK.get_or_init(|| Arc::new(Mutex::new(()))).clone()
}

impl CodeToolAuthority {
    #[cfg(test)]
    pub(crate) fn for_test(decisions: Vec<Result<CodeAuthorityDecision, String>>) -> Self {
        Self {
            record: PathBuf::new(),
            expected: None,
            session_id: "code-test-session".into(),
            lock: device_lock(),
            test_decisions: Some(Arc::new(std::sync::Mutex::new(decisions.into()))),
        }
    }

    pub(crate) fn configured() -> Option<Self> {
        Self::configured_at(enrollment_path()?)
    }

    pub(crate) fn configured_at(record: PathBuf) -> Option<Self> {
        // Once the file exists, malformed state is an error at execution, not
        // a downgrade to the old approval policy.
        record.exists().then(|| {
            let expected = std::fs::read(&record)
                .ok()
                .and_then(|b| serde_json::from_slice::<Enrollment>(&b).ok())
                .and_then(|enrollment| {
                    Context::load()
                        .ok()
                        .map(|c| (enrollment.key_id, c.organization, c.workspace))
                });
            Self {
                record,
                expected,
                session_id: uuid::Uuid::new_v4().to_string(),
                lock: device_lock(),
                #[cfg(test)]
                test_decisions: None,
            }
        })
    }
    pub(crate) fn session_id(&self) -> &str {
        &self.session_id
    }
    pub(crate) async fn authorize(&self, invocation: Value) -> Result<CodeAuthorityDecision> {
        let _guard = self.lock.lock().await;
        #[cfg(test)]
        if let Some(decisions) = &self.test_decisions {
            return decisions
                .lock()
                .unwrap()
                .pop_front()
                .expect("unexpected authorization call")
                .map_err(anyhow::Error::msg);
        }
        let enrollment: Enrollment = serde_json::from_slice(&tokio::fs::read(&self.record).await?)?;
        let context = Context::load()?;
        if self.expected.as_ref()
            != Some(&(
                enrollment.key_id.clone(),
                context.organization.clone(),
                context.workspace.clone(),
            ))
        {
            bail!(
                "Code device or workspace changed; start a new session with the intended authority"
            );
        }
        let challenge = context
            .challenge(&enrollment.key_id, Some(invocation))
            .await?;
        let proof=helper(json!({"command":"assert","keyId":enrollment.key_id,"clientData":challenge.client_data})).await?;
        context
            .finish(
                "authorize",
                &enrollment.key_id,
                &challenge,
                required(&proof, "proof")?,
            )
            .await
    }
}

struct Context {
    http: reqwest::Client,
    base: String,
    token: String,
    organization: String,
    workspace: String,
}
impl Context {
    fn load() -> Result<Self> {
        if let Some(customer) = crate::private_code_authority::load()? {
            return Ok(Self {
                http: customer.http,
                base: customer.base,
                token: customer.session.access_token,
                organization: customer.session.organization_id,
                workspace: customer
                    .session
                    .workspace_id
                    .context("Customer authority requires a workspace")?,
            });
        }
        crate::safety::require_vendor_network()?;
        let env: HashMap<String, String> = std::env::vars().collect();
        let snapshot = crate::init_cli::load_evalops_snapshot()?;
        let session = crate::credential_mode::platform_session_from(snapshot.as_ref(), &env)
            .context("Sign in before using Code device authority")?;
        let base = crate::init_cli::evalops_identity_base_url(snapshot.as_ref(), &env)?;
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(10))
            .build()?;
        Ok(Self {
            http,
            base,
            token: session.access_token,
            organization: session.organization_id,
            workspace: session
                .workspace_id
                .filter(|s| !s.is_empty())
                .context("Code tool authority requires an explicit workspace")?,
        })
    }
    async fn post(&self, path: &str, body: Value) -> Result<reqwest::Response> {
        crate::private_code_authority::validate_request_target(
            &self.base,
            &self.organization,
            &self.workspace,
        )?;
        let method = match path {
            "challenge" => "CreateCodeChallenge",
            "enroll" => "EnrollCodeDevice",
            "authorize" => "AuthorizeCodeTool",
            "revoke" => "RevokeCodeDevice",
            _ => bail!("Unknown Code authority method"),
        };
        let response = self
            .http
            .post(format!(
                "{}/identity.v1.CodeAuthorityService/{method}",
                self.base
            ))
            .header("connect-protocol-version", "1")
            .bearer_auth(&self.token)
            .json(&body)
            .send()
            .await?;
        if !response.status().is_success() {
            bail!(
                "Identity denied Code device authority ({})",
                response.status()
            );
        }
        Ok(response)
    }
    async fn challenge(&self, key_id: &str, invocation: Option<Value>) -> Result<Challenge> {
        let requested = json!({"organizationId":self.organization,"workspaceId":self.workspace,"deviceId":key_id,"invocation":invocation});
        let challenge: Challenge = self.post("challenge", requested).await?.json().await?;
        if challenge.challenge_id.is_empty() || challenge.client_data.len() > 16 * 1024 {
            bail!("Invalid Code authority challenge");
        }
        // A compromised/misrouted response must not ask this device to sign a
        // request other than the one its executor is about to run.
        let binding: Value = serde_json::from_str(&challenge.client_data)?;
        if binding["version"] != 1
            || binding["deviceId"] != key_id
            || binding["invocation"] != json!(invocation)
            || binding["scope"]["organization_id"] != self.organization
            || binding["scope"]["workspace_id"] != self.workspace
        {
            bail!("Code authority challenge binding mismatch");
        }
        Ok(challenge)
    }
    async fn finish(
        &self,
        path: &str,
        key_id: &str,
        challenge: &Challenge,
        proof: &str,
    ) -> Result<CodeAuthorityDecision> {
        let decision:CodeAuthorityDecision=self.post(path,json!({"organizationId":self.organization,"workspaceId":self.workspace,"deviceId":key_id,"challengeId":challenge.challenge_id,"proof":proof})).await?.json().await?;
        if !decision.allowed
            || decision.device_id != key_id
            || decision.decision_id != challenge.challenge_id
            || decision.policy_id != "identity-code-hardware-authority"
            || decision.policy_version != "1"
            || decision.request_digest
                != format!("{:x}", Sha256::digest(challenge.client_data.as_bytes()))
            || decision.expires_at_unix_seconds <= now()
            || decision.expires_at_unix_seconds > now() + 15
        {
            bail!("Code authority decision is invalid or expired");
        }
        Ok(decision)
    }
}

fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .context("Device helper response is incomplete")
}
async fn helper(request: Value) -> Result<Value> {
    let path = std::env::var_os("MAESTRO_CODE_DEVICE_HELPER")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::current_exe().ok().and_then(|p| {
                p.parent()
                    .map(|p| p.join("DeixicCodeDevice.app/Contents/MacOS/deixic-code-device"))
            })
        })
        .context("Code device helper is unavailable")?;
    let mut child = tokio::process::Command::new(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()?;
    child
        .stdin
        .take()
        .context("Device helper input unavailable")?
        .write_all(&serde_json::to_vec(&request)?)
        .await?;
    let mut stdout = child
        .stdout
        .take()
        .context("Device helper output unavailable")?;
    let (status, output) = tokio::time::timeout(Duration::from_mins(2), async {
        let mut output = Vec::new();
        (&mut stdout)
            .take(128 * 1024 + 1)
            .read_to_end(&mut output)
            .await?;
        if output.len() > 128 * 1024 {
            bail!("Device helper response is too large");
        }
        let status = child.wait().await?;
        Ok::<_, anyhow::Error>((status, output))
    })
    .await??;
    if !status.success() {
        bail!(
            "Code device attestation failed; an approved signed release on supported hardware is required"
        );
    }
    Ok(serde_json::from_slice(&output)?)
}

pub async fn enroll() -> Result<i32> {
    let path = enrollment_path().context("Home directory is unavailable")?;
    if path.exists() {
        bail!(
            "This Code installation is already enrolled; revoke the existing device before enrolling a replacement"
        );
    }
    let generated = helper(json!({"command":"generate"})).await?;
    if crate::safety::disconnected_authority()
        .map_err(anyhow::Error::msg)?
        .is_none()
    {
        crate::safety::require_vendor_network()?;
        crate::init_cli::perform_code_authority_login().await?;
    }
    let context = Context::load()?;
    let key = required(&generated, "keyId")?;
    let challenge = context.challenge(key, None).await?;
    let attestation =
        helper(json!({"command":"attest","keyId":key,"clientData":challenge.client_data})).await?;
    context
        .finish("enroll", key, &challenge, required(&attestation, "proof")?)
        .await?;
    tokio::fs::create_dir_all(path.parent().context("Enrollment directory unavailable")?).await?;
    let mut options = tokio::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    options
        .open(path)
        .await?
        .write_all(&serde_json::to_vec(&Enrollment { key_id: key.into() })?)
        .await?;
    println!("Device enrolled for Code tool authority.");
    Ok(0)
}

pub async fn revoke() -> Result<i32> {
    let path = enrollment_path().context("Home directory unavailable")?;
    let enrollment: Enrollment = serde_json::from_slice(&tokio::fs::read(&path).await?)?;
    let context = Context::load()?;
    let decision:CodeAuthorityDecision=context.post("revoke",json!({"organizationId":context.organization,"workspaceId":context.workspace,"deviceId":enrollment.key_id,"invocation":null})).await?.json().await?;
    if !decision.allowed
        || decision.device_id != enrollment.key_id
        || decision.policy_id != "identity-code-device-revocation"
    {
        bail!("Device revocation was not confirmed");
    }
    tokio::fs::remove_file(path).await?;
    println!("Code device authority revoked.");
    Ok(0)
}
