#![cfg(unix)]
use std::{
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    os::fd::OwnedFd,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

fn maestro_binary() -> std::ffi::OsString {
    option_env!("CARGO_BIN_EXE_maestro")
        .map(std::ffi::OsString::from)
        .or_else(|| std::env::var_os("CARGO_BIN_EXE_maestro"))
        .expect("Cargo must provide the maestro integration-test binary")
}

struct ReapedChild(Child);
impl Drop for ReapedChild {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn native_child_serves_the_inherited_listener_with_tenant_authentication() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let fd: OwnedFd = listener.into();
    let temp = std::env::temp_dir().join(format!("maestro-companion-{}", std::process::id()));
    std::fs::create_dir_all(&temp).unwrap();
    let temp = dunce::canonicalize(temp).unwrap();
    let binary = maestro_binary();
    let mut child = ReapedChild(
        Command::new(binary)
            .args([
                "serve",
                "--port",
                &address.port().to_string(),
                "--parent-pid",
                &std::process::id().to_string(),
            ])
            .current_dir(&temp)
            .env("MAESTRO_NATIVE_CODE_COMPANION", "1")
            .env("MAESTRO_NATIVE_CODE_LISTENER_STDIN", "1")
            .env("MAESTRO_CONTROL_HOST", "127.0.0.1")
            .env(
                "MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN",
                "integration-private-token",
            )
            .env(
                "MAESTRO_DEFAULT_MODEL",
                "evalops/accounts/fireworks/models/glm-5p3",
            )
            .env("MAESTRO_HOME", &temp)
            .env("MAESTRO_LLM_GATEWAY_TIMEOUT_MS", "50")
            .env_remove("MAESTRO_LIVENESS_FD")
            .stdin(Stdio::from(fd))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        assert!(
            child.0.try_wait().unwrap().is_none(),
            "companion exited before readiness"
        );
        if let Ok(reply) = request(address, true) {
            if reply.starts_with("HTTP/1.1 200 ") {
                let body = reply.split_once("\r\n\r\n").unwrap().1;
                let value: serde_json::Value = serde_json::from_str(body).unwrap();
                assert_eq!(value["workspacePath"], temp.to_str().unwrap());
                assert_eq!(
                    value["modelId"],
                    "evalops/accounts/fireworks/models/glm-5p3"
                );
                assert_eq!(value["interactionModes"], serde_json::json!(["discuss"]));
                assert_eq!(value["principal"]["subject"], "native-integration-user");
                assert!(
                    value["gatewayEpoch"]
                        .as_str()
                        .is_some_and(|value| !value.is_empty())
                );
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "native child did not become ready"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        request(address, false)
            .unwrap()
            .starts_with("HTTP/1.1 401 ")
    );
    assert!(
        TcpListener::bind(address).is_err(),
        "child retains its original listener"
    );
    drop(child);
    std::fs::remove_dir_all(temp).unwrap();
}

fn request(address: std::net::SocketAddr, authorized: bool) -> std::io::Result<String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    let headers = if authorized {
        "x-maestro-proxy-auth: integration-private-token\r\nx-auth-request-user: native-integration-user\r\nx-organization-id: native-org\r\nx-workspace-id: native-workspace\r\nx-auth-request-scope: maestro:read\r\n"
    } else {
        ""
    };
    stream.write_all(format!("GET /api/native/capabilities HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{headers}\r\n").as_bytes())?;
    let mut reply = String::new();
    stream.take(65_536).read_to_string(&mut reply)?;
    Ok(reply)
}
