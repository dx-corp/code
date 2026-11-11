//! Runtime-local admission, drain and termination. Platform effect authority is unchanged.
use super::*;
use std::future::Future;
use std::sync::Mutex as SyncMutex;
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

pub(crate) const DRAIN_PATH: &str = "/.well-known/evalops/remote-runner/drain";

#[derive(Default, PartialEq, Eq)]
enum AdmissionPhase {
    #[default]
    Accepting,
    Draining,
    Stopping,
}

#[derive(Default)]
struct AdmissionState {
    phase: AdmissionPhase,
    active: usize,
}

#[derive(Default)]
pub(crate) struct Lifecycle {
    state: SyncMutex<AdmissionState>,
    changed: Notify,
    pub(crate) cancel: CancellationToken,
}

#[derive(Clone)]
pub(crate) struct Admission(Arc<Lease>);
struct Lease(Arc<Lifecycle>);

tokio::task_local! { static ADMITTED: Admission; }

impl Drop for Lease {
    fn drop(&mut self) {
        self.0.state.lock().expect("lifecycle lock").active -= 1;
        self.0.changed.notify_waiters();
    }
}

impl Lifecycle {
    pub(crate) fn admit(self: &Arc<Self>) -> Result<Admission, ()> {
        if let Ok(existing) = ADMITTED.try_with(Clone::clone) {
            if Arc::ptr_eq(&existing.0.0, self) {
                return Ok(existing);
            }
        }
        let mut state = self.state.lock().expect("lifecycle lock");
        if state.phase != AdmissionPhase::Accepting {
            return Err(());
        }
        state.active += 1;
        Ok(Admission(Arc::new(Lease(self.clone()))))
    }

    /// Count a control operation only after its handler has authorized an existing
    /// accepted resource. This does not admit a new turn during drain.
    pub(crate) fn continue_accepted(self: &Arc<Self>) -> Result<Admission, ()> {
        if let Ok(existing) = ADMITTED.try_with(Clone::clone) {
            if Arc::ptr_eq(&existing.0.0, self) {
                return Ok(existing);
            }
        }
        let mut state = self.state.lock().expect("lifecycle lock");
        if state.phase == AdmissionPhase::Stopping {
            return Err(());
        }
        state.active += 1;
        Ok(Admission(Arc::new(Lease(self.clone()))))
    }

    pub(crate) fn begin_drain(&self) {
        let mut state = self.state.lock().expect("lifecycle lock");
        if state.phase == AdmissionPhase::Accepting {
            state.phase = AdmissionPhase::Draining;
        }
    }

    fn begin_shutdown(&self) {
        // Stop creating continuation owners before the server waits for idle.
        // Existing leases can still finish or cancel their accepted work.
        self.state.lock().expect("lifecycle lock").phase = AdmissionPhase::Stopping;
    }

    pub(crate) fn is_draining(&self) -> bool {
        self.state.lock().expect("lifecycle lock").phase != AdmissionPhase::Accepting
    }

    pub(crate) fn active(&self) -> usize {
        self.state.lock().expect("lifecycle lock").active
    }

    pub(crate) async fn idle(&self) {
        loop {
            let changed = self.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if self.active() == 0 {
                return;
            }
            changed.await;
        }
    }
}

pub(crate) async fn scope<F: Future>(admission: Option<Admission>, future: F) -> F::Output {
    match admission {
        Some(admission) => ADMITTED.scope(admission, future).await,
        None => future.await,
    }
}

/// Carry the accepted operation into its asynchronous owner after HTTP returns.
pub(crate) fn spawn<F>(admission: Admission, future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: Future + Send + 'static,
    F::Output: Send + 'static,
{
    tokio::spawn(scope(Some(admission), future))
}

pub(crate) fn admit_request(state: &AppState, head: &RequestHead) -> Result<Option<Admission>, ()> {
    // These controls finish or stop an already accepted turn; their existing
    // resource ownership/auth checks still run in the route handler.
    let continuation = head.path.starts_with("/api/pending-requests/")
        || head.path == "/api/approvals"
        || (head.path.starts_with("/api/native/turns/") && head.path.ends_with("/control"))
        || crate::a2a_task_id_from_cancel_path(&head.path).is_some();
    if head.path == DRAIN_PATH || continuation {
        return Ok(None);
    }
    if is_chat_websocket_endpoint(head) {
        return if state.lifecycle().is_draining() {
            Err(())
        } else {
            Ok(None)
        };
    }
    if matches!(head.method.as_str(), "GET" | "HEAD" | "OPTIONS") {
        return Ok(None);
    }
    state.lifecycle().admit().map(Some)
}

pub(crate) fn unavailable() -> Vec<u8> {
    json_response(
        503,
        &serde_json::json!({"error":"Runtime is draining","retryable":true}),
    )
}

pub(crate) fn drain_response(head: &RequestHead, stream: &TcpStream, state: &AppState) -> Vec<u8> {
    // Process administration uses the container's existing key over its actual
    // loopback socket. A tenant write token does not administer other tenants.
    if !stream.peer_addr().is_ok_and(|peer| peer.ip().is_loopback()) {
        return json_response(
            403,
            &serde_json::json!({"error":"Drain requires local process access"}),
        );
    }
    let valid = state
        .config
        .api_key
        .as_ref()
        .zip(head.headers.get("x-maestro-api-key"))
        .is_some_and(|(expected, provided)| {
            !expected.is_empty() && constant_time_eq(expected.as_bytes(), provided.as_bytes())
        });
    if !valid {
        return json_response(
            401,
            &serde_json::json!({"error":"Drain requires the configured process API key"}),
        );
    }
    if head.method == "POST" {
        state.lifecycle().begin_drain();
    }
    json_response(
        200,
        &serde_json::json!({"draining":state.lifecycle().is_draining(),"active":state.lifecycle().active()}),
    )
}

async fn termination_signal() -> anyhow::Result<&'static str> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            _ = terminate.recv() => Ok("sigterm"),
            result = tokio::signal::ctrl_c() => { result?; Ok("sigint") },
        }
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok("interrupt")
    }
}

pub(crate) async fn serve_connections(
    listener: TcpListener,
    state: AppState,
    owner_loss: impl Future<Output = String>,
) -> anyhow::Result<()> {
    tokio::pin!(owner_loss);
    let signal = termination_signal();
    tokio::pin!(signal);
    let mut requests = tokio::task::JoinSet::new();
    let reason = loop {
        tokio::select! {
            reason = &mut owner_loss => break reason,
            reason = &mut signal => break reason?.to_owned(),
            _ = requests.join_next(), if !requests.is_empty() => {},
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let state = state.clone();
                        requests.spawn(async move {
                            if let Err(error) = handle_connection(stream, state).await {
                                eprintln!("runtime-gateway request failed: {error:#}");
                            }
                        });
                    }
                    Err(error) => {
                        eprintln!("runtime-gateway accept failed: {error}");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    };
    state.lifecycle().begin_shutdown();
    drop(listener);
    if tokio::time::timeout(Duration::from_secs(10), state.lifecycle().idle())
        .await
        .is_err()
    {
        state.lifecycle().cancel.cancel();
        state.native_turns.shutdown();
        for sender in state.a2a_cancel_senders.lock().await.values() {
            let _ = sender.send(true);
        }
        if tokio::time::timeout(Duration::from_secs(5), state.lifecycle().idle())
            .await
            .is_err()
        {
            let incomplete = format!(
                "shutdown_incomplete:{reason}:{}",
                state.lifecycle().active()
            );
            write_daemon_exit_receipt(&incomplete, "incomplete").await?;
            anyhow::bail!(
                "Shutdown incomplete: {} accepted operation(s) still running after {reason}",
                state.lifecycle().active()
            );
        }
    }
    requests.abort_all();
    while requests.join_next().await.is_some() {}
    write_daemon_exit_receipt(&reason, "stopped").await?;
    Ok(())
}

pub(super) fn drain_address(host: &str, port: u16) -> std::net::SocketAddr {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};

    let host = host.trim();
    let literal = host
        .strip_prefix('[')
        .and_then(|rest| rest.strip_suffix(']'))
        .unwrap_or(host);
    let address = literal
        .parse::<IpAddr>()
        .ok()
        .filter(IpAddr::is_loopback)
        .unwrap_or_else(|| {
            if host.contains(':') {
                Ipv6Addr::LOCALHOST.into()
            } else {
                Ipv4Addr::LOCALHOST.into()
            }
        });
    SocketAddr::new(address, port)
}

/// Native preStop client. The API key stays in the process environment and is
/// sent only to a loopback socket, without proxying or redirects.
pub async fn drain() -> anyhow::Result<()> {
    let key =
        trimmed_env("MAESTRO_WEB_API_KEY").context("MAESTRO_WEB_API_KEY is required to drain")?;
    let port = env::var("PORT")
        .unwrap_or_else(|_| "8080".into())
        .parse::<u16>()?;
    let host = env::var("MAESTRO_CONTROL_HOST").unwrap_or_default();
    let url = format!("http://{}{DRAIN_PATH}", drain_address(&host, port));
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?;
    tokio::time::timeout(Duration::from_secs(25), async {
        let mut begin = true;
        loop {
            let request = if begin {
                client.post(&url).json(&serde_json::json!({}))
            } else {
                client.get(&url)
            };
            begin = false;
            let response = request.header("x-maestro-api-key", &key).send().await?;
            anyhow::ensure!(
                response.status().is_success(),
                "Drain rejected with HTTP {}",
                response.status()
            );
            let status: Value = response.json().await?;
            anyhow::ensure!(
                status["draining"] == true,
                "Runtime did not enter drain state"
            );
            if status["active"].as_u64() == Some(0) {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .context("Drain timed out with accepted work still running; readiness remains disabled")??;
    Ok(())
}
