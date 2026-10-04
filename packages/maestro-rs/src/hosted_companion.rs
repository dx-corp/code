//! The opt-in coding companion shares the hosted resident's supervised lifetime.
use anyhow::{Context, Result, bail};
use maestro_local_host::hosted_runner_cli::resolve_hosted_runner_launch_config;
use std::{collections::HashMap, ffi::OsString, future::Future, net::TcpListener, path::PathBuf};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    process::{Child, Command},
    time::{Duration, Instant},
};

const READY_TIMEOUT: Duration = Duration::from_secs(10);
const PROBE_SUBJECT: &str = "native-companion-readiness";

pub(crate) fn requested(args: &[OsString], enabled: Option<&str>) -> bool {
    enabled == Some("1")
        && args.get(1).and_then(|value| value.to_str()) == Some("hosted-runner")
        && !args
            .iter()
            .skip(2)
            .any(|value| matches!(value.to_str(), Some("--help" | "-h" | "--version")))
}

pub(crate) struct Companion {
    command: Command,
    workspace: PathBuf,
    model: String,
    base_url: String,
    proxy_token: String,
}

impl Companion {
    /// Resolve the existing launch owner before starting asynchronous workers.
    pub(crate) fn prepare(args: &[OsString]) -> Result<Self> {
        let gateway_credential = maestro_runtime_gateway::prepare_native_code_gateway_credential()?;
        let env = std::env::vars().collect::<HashMap<_, _>>();
        let mut hosted_args = vec![OsString::from("deixic-code hosted-runner")];
        hosted_args.extend(args.iter().skip(2).cloned());
        let launch = resolve_hosted_runner_launch_config(hosted_args, &env)?;
        let identity = launch
            .runner
            .workload_identity
            .as_ref()
            .context("native companion requires the workload owner")?;
        maestro_runtime_gateway::validate_native_code_credential_owner(
            &identity.organization_id,
            &identity.workspace_id,
            &launch.runner.runner_session_id,
        )?;
        let workspace = launch.runner.workspace_root;
        let model = launch
            .supervisor
            .transport
            .env
            .iter()
            .find(|(key, _)| key == "MAESTRO_MODEL")
            .map(|(_, value)| value.clone())
            .context("hosted launch model is missing")?;
        let reservation = TcpListener::bind("127.0.0.1:0")?;
        let address = reservation.local_addr()?;
        let base_url = format!("http://{address}");
        let proxy_token = maestro_runtime_gateway::generate_native_proxy_token()?;
        let mut command = Command::new(std::env::current_exe()?);
        command
            .args([
                "serve",
                "--port",
                &address.port().to_string(),
                "--parent-pid",
                &std::process::id().to_string(),
            ])
            .current_dir(&workspace)
            .envs(launch.supervisor.transport.env)
            .env("MAESTRO_HOME", workspace.join(".dex-home/native-code"))
            .env("MAESTRO_STATE_DIR", workspace.join(".dex-home/native-code"))
            .env(
                "MAESTRO_SESSIONS_FILE",
                workspace.join(".dex-home/native-code/sessions.json"),
            )
            .env(
                "MAESTRO_SESSION_MESSAGES_FILE",
                workspace.join(".dex-home/native-code/session-messages.json"),
            )
            .env_remove("MAESTRO_EVALOPS_ACCESS_TOKEN")
            .env("MAESTRO_EVALOPS_ACCESS_TOKEN_FILE", gateway_credential)
            .env_remove("MAESTRO_WEB_API_KEY")
            .env_remove("MAESTRO_WEB_API_KEY_SCOPES")
            .env_remove("MAESTRO_NATIVE_CODE_URL")
            .env_remove("MAESTRO_NATIVE_CODE_ENABLED")
            .env_remove("MAESTRO_LIVENESS_FD")
            .env("MAESTRO_CONTROL_HOST", "127.0.0.1")
            .env("MAESTRO_WEB_REQUIRE_KEY", "1")
            .env("MAESTRO_WEB_REQUIRE_CSRF", "0")
            .env("MAESTRO_NATIVE_CODE_COMPANION", "1")
            .env("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN", &proxy_token)
            .env("MAESTRO_DEFAULT_MODEL", &model)
            .kill_on_drop(true);
        #[cfg(unix)]
        {
            let socket: std::os::fd::OwnedFd = reservation.into();
            command
                .stdin(std::process::Stdio::from(socket))
                .env("MAESTRO_NATIVE_CODE_LISTENER_STDIN", "1");
        }
        #[cfg(not(unix))]
        bail!("native coding companion requires an inherited Unix listener");
        Ok(Self {
            command,
            workspace,
            model,
            base_url,
            proxy_token,
        })
    }

    /// These two private transport coordinates are published before Tokio starts.
    pub(crate) fn publish(&self) {
        std::env::set_var("MAESTRO_NATIVE_CODE_URL", &self.base_url);
        std::env::set_var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN", &self.proxy_token);
    }

    pub(crate) async fn run<F>(mut self, runner: F) -> Result<()>
    where
        F: Future<Output = Result<()>>,
    {
        // The child inherits the bound socket. No other process can claim its
        // port between preparation and authenticated readiness.
        let mut child = self
            .command
            .spawn()
            .context("start native coding companion")?;
        let ready = wait_ready(
            &mut child,
            &self.base_url,
            &self.proxy_token,
            &self.workspace,
            &self.model,
        )
        .await;
        if let Err(error) = ready {
            stop_child(&mut child).await?;
            return Err(error);
        }
        supervise(child, runner).await
    }
}

pub(crate) fn inherited_listener(
    config: &maestro_runtime_gateway::RuntimeGatewayConfig,
) -> Result<Option<TcpListener>> {
    if std::env::var("MAESTRO_NATIVE_CODE_COMPANION").as_deref() != Ok("1")
        || std::env::var("MAESTRO_NATIVE_CODE_LISTENER_STDIN").as_deref() != Ok("1")
    {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::fd::FromRawFd;
        // Only the opted-in companion parent installs its already-bound
        // listener as stdin. This takes ownership before worker threads start.
        let listener = unsafe { TcpListener::from_raw_fd(0) };
        if listener.local_addr()?.to_string() != config.listen_addr() {
            bail!("inherited native listener does not match the configured owner");
        }
        listener.set_nonblocking(true)?;
        Ok(Some(listener))
    }
    #[cfg(not(unix))]
    bail!("native coding companion requires an inherited Unix listener")
}

async fn stop_child(child: &mut Child) -> Result<()> {
    if child.try_wait()?.is_none() {
        child
            .kill()
            .await
            .context("kill and reap native coding companion")?;
    }
    Ok(())
}

async fn supervise<F>(mut child: Child, runner: F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    let result = tokio::select! {
        result = runner => result,
        status = child.wait() => match status {
            Ok(status) => Err(anyhow::anyhow!("native coding companion exited: {status}")),
            Err(error) => Err(error.into()),
        },
    };
    stop_child(&mut child).await?;
    result
}

async fn wait_ready(
    child: &mut Child,
    base_url: &str,
    token: &str,
    workspace: &std::path::Path,
    model: &str,
) -> Result<()> {
    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("native coding companion exited before readiness: {status}");
        }
        if tokio::time::timeout(
            Duration::from_millis(500),
            probe(base_url, token, workspace, model),
        )
        .await
        .is_ok_and(|value| value.is_ok())
        {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("native coding companion did not become authenticated and ready");
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

async fn probe(
    base_url: &str,
    token: &str,
    workspace: &std::path::Path,
    model: &str,
) -> Result<()> {
    let address = base_url
        .strip_prefix("http://127.0.0.1:")
        .context("native companion must use loopback")?;
    let mut stream = TcpStream::connect(format!("127.0.0.1:{address}")).await?;
    stream.write_all(format!("GET /api/native/capabilities HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nx-maestro-proxy-auth: {token}\r\nx-auth-request-user: {PROBE_SUBJECT}\r\nx-organization-id: {PROBE_SUBJECT}\r\nx-workspace-id: {PROBE_SUBJECT}\r\nx-auth-request-scope: maestro:read\r\n\r\n").as_bytes()).await?;
    let mut reply = Vec::new();
    stream.take(65_537).read_to_end(&mut reply).await?;
    if reply.len() > 65_536 || !reply.starts_with(b"HTTP/1.1 200 ") {
        bail!("native companion authentication was not accepted");
    }
    let offset = reply
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .context("native companion response missing body")?
        + 4;
    let value: serde_json::Value = serde_json::from_slice(&reply[offset..])?;
    let selected = value["modelId"].as_str().unwrap_or_default();
    if value["version"] != 1
        || value["workspacePath"].as_str() != workspace.to_str()
        || value["principal"]["subject"] != PROBE_SUBJECT
        || value["principal"]["organizationId"] != PROBE_SUBJECT
        || value["principal"]["workspaceId"] != PROBE_SUBJECT
        || value["gatewayEpoch"].as_str().is_none_or(str::is_empty)
        || !(selected == model
            || (!model.contains('/') && selected.ends_with(&format!("/{model}"))))
    {
        bail!("native companion identity, workspace or model did not match the hosted launch");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_is_opt_in_and_does_not_change_help_or_other_commands() {
        let args = vec!["maestro".into(), "hosted-runner".into()];
        assert!(requested(&args, Some("1")));
        assert!(!requested(&args, None));
        assert!(!requested(&args, Some("0")));
        assert!(!requested(&["maestro".into(), "exec".into()], Some("1")));
        assert!(!requested(
            &["maestro".into(), "hosted-runner".into(), "--help".into()],
            Some("1")
        ));
    }

    #[tokio::test]
    async fn authenticated_readiness_rejects_a_different_workspace() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0; 4096];
            let count = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.contains("x-maestro-proxy-auth: private-test-token"));
            assert!(request.contains("x-auth-request-user: native-companion-readiness"));
            let body = serde_json::json!({"version":1,"workspacePath":"/other","modelId":"openai/gpt-test","gatewayEpoch":"new-owner","principal":{"subject":PROBE_SUBJECT,"organizationId":PROBE_SUBJECT,"workspaceId":PROBE_SUBJECT}}).to_string();
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        assert!(
            probe(
                &base_url,
                "private-test-token",
                std::path::Path::new("/workspace"),
                "openai/gpt-test"
            )
            .await
            .is_err()
        );
        server.await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn runner_completion_kills_and_reaps_the_companion() {
        let child = Command::new("sh")
            .args(["-c", "exec sleep 30"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        supervise(child, async { Ok(()) }).await.unwrap();
        let status = Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .unwrap();
        assert!(!status.success());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn companion_exit_ends_the_hosted_lifetime() {
        let child = Command::new("sh")
            .args(["-c", "exit 7"])
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        assert!(supervise(child, std::future::pending()).await.is_err());
    }
}
