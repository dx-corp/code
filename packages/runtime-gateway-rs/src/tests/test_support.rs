#[allow(unused_imports)]
use super::*;

pub(crate) fn snapshot_env(
    names: &'static [&'static str],
) -> Vec<(&'static str, Option<std::ffi::OsString>)> {
    names
        .iter()
        .map(|name| (*name, env::var_os(name)))
        .collect()
}

pub(crate) fn clear_env(names: &[&str]) {
    for name in names {
        env::remove_var(name);
    }
}

pub(crate) fn hs256_signed_claims(secret: &[u8], claims: Value) -> String {
    let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"HS256","typ":"JWT"}"#);
    let payload = URL_SAFE_NO_PAD
        .encode(serde_json::to_vec(&claims).expect("test JWT claims should serialize"));
    let signing_input = format!("{header}.{payload}");
    let signature = hmac_sha256_base64url(secret, signing_input.as_bytes());
    format!("{signing_input}.{signature}")
}

pub(crate) fn unique_test_dir(prefix: &str) -> PathBuf {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    env::temp_dir().join(format!("{prefix}-{}-{now}", process::id()))
}

pub(crate) async fn tcp_stream_pair() -> (TcpStream, TcpStream) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("listener should bind");
    let addr = listener
        .local_addr()
        .expect("listener should have local addr");
    let connect = TcpStream::connect(addr);
    let accept = listener.accept();
    let (client, accepted) = tokio::join!(connect, accept);
    let client = client.expect("client should connect");
    let (server, _) = accepted.expect("listener should accept");
    (client, server)
}

impl TestDir {
    pub(crate) fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "maestro-runtime-gateway-{label}-{}-{}",
            process::id(),
            ATTACHMENT_TEMP_COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&path).expect("test dir should be created");
        Self { path }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}

pub(crate) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

pub(crate) async fn inbox_of(state: &AppState, session_id: &str) -> Vec<SessionMessage> {
    state
        .session_messages
        .lock()
        .await
        .messages_by_to_session
        .get(session_id)
        .cloned()
        .unwrap_or_default()
}
