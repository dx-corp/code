//! Customer-cell transport for the existing Identity Code authority.
//! Signed machine policy chooses the endpoint and tenant; Identity still
//! verifies tokens, device proofs, scope, revocation, and tool decisions.
use anyhow::{Context as _, Result, bail};
use std::{
    collections::HashMap,
    io::Read as _,
    net::{IpAddr, SocketAddr, ToSocketAddrs as _},
    time::Duration,
};

use crate::{
    credential_mode::{
        IdentityIntrospection, PlatformSession, verified_platform_session_for_scope,
    },
    safety::{DisconnectedAuthority, ManagedPolicyMetadata},
};

const TIMEOUT: Duration = Duration::from_secs(10);
const MAX_RESPONSE_BYTES: u64 = 64 * 1024;

pub(crate) struct CustomerAuthority {
    pub http: reqwest::Client,
    pub base: String,
    pub session: PlatformSession,
}

pub(crate) fn load() -> Result<Option<CustomerAuthority>> {
    let Some((config, metadata)) =
        crate::safety::disconnected_authority().map_err(anyhow::Error::msg)?
    else {
        return Ok(None);
    };
    // No snapshot, vendor token, browser login, refresh, or tenant hint is used.
    let env = std::env::vars().collect();
    let (session, pinned) = verify(&config, &metadata, &env)?;
    let mut builder = reqwest::Client::builder()
        .resolve_to_addrs(&pinned.host, &pinned.addresses)
        .no_proxy()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT);
    if let Some(certificates) = certificates(&config)? {
        builder = builder.use_rustls_tls().tls_built_in_root_certs(false);
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
    }
    Ok(Some(CustomerAuthority {
        http: builder.build()?,
        base: config.endpoint.trim_end_matches('/').to_owned(),
        session,
    }))
}

/// Recheck signed policy immediately before every authority exchange, including
/// the exchange after a potentially long hardware-attestation prompt.
pub(crate) fn validate_request_target(
    base: &str,
    organization: &str,
    workspace: &str,
) -> Result<()> {
    match crate::safety::disconnected_authority().map_err(anyhow::Error::msg)? {
        Some((config, metadata)) => {
            if config.endpoint.trim_end_matches('/') != base
                || metadata.org_id != organization
                || metadata.workspace_id.as_deref() != Some(workspace)
            {
                bail!("Code authority no longer matches signed customer policy");
            }
            Ok(())
        }
        None => crate::safety::require_vendor_network(),
    }
}

#[derive(Debug, Clone)]
struct PinnedAuthority {
    host: String,
    addresses: Vec<SocketAddr>,
}

fn allowed_authority_ip(ip: IpAddr) -> bool {
    // AWS reserves this ULA range for instance-local infrastructure, including
    // IMDS at fd00:ec2::254. A general ULA exception must not authorize it.
    if matches!(ip, IpAddr::V6(v6) if v6.segments()[..2] == [0xfd00, 0x0ec2]) {
        return false;
    }
    !crate::tools::net_guard::is_blocked_ip(ip)
        || match ip {
            IpAddr::V4(v4) => v4.is_private() || v4.is_loopback(),
            IpAddr::V6(v6) => v6.is_unique_local() || v6.is_loopback(),
        }
}

fn checked_addresses(addresses: Vec<SocketAddr>, port: u16) -> Result<Vec<SocketAddr>> {
    if addresses.is_empty()
        || addresses
            .iter()
            .any(|addr| addr.port() != port || !allowed_authority_ip(addr.ip()))
    {
        bail!("Customer Identity resolved to a forbidden address");
    }
    Ok(addresses)
}

fn resolve_authority(config: &DisconnectedAuthority) -> Result<PinnedAuthority> {
    let url = reqwest::Url::parse(&config.endpoint)?;
    let host = url
        .host_str()
        .context("Customer Identity host is missing")?
        .trim_matches(['[', ']'])
        .to_owned();
    let port = url
        .port_or_known_default()
        .context("Customer Identity port is missing")?;
    let addresses = if let Ok(ip) = host.parse::<IpAddr>() {
        vec![SocketAddr::new(ip, port)]
    } else {
        let (sender, receiver) = std::sync::mpsc::sync_channel(1);
        let resolver_host = host.clone();
        std::thread::spawn(move || {
            let result = (resolver_host.as_str(), port)
                .to_socket_addrs()
                .map(Iterator::collect);
            let _ = sender.send(result);
        });
        receiver
            .recv_timeout(TIMEOUT)
            .context("Customer Identity DNS resolution timed out")??
    };
    Ok(PinnedAuthority {
        host,
        addresses: checked_addresses(addresses, port)?,
    })
}

fn certificates(config: &DisconnectedAuthority) -> Result<Option<Vec<reqwest::Certificate>>> {
    let Some(pem) = &config.ca_pem else {
        return Ok(None);
    };
    if pem.len() > MAX_RESPONSE_BYTES as usize {
        bail!("Customer Identity CA exceeds size limit");
    }
    let certificates = reqwest::Certificate::from_pem_bundle(pem.as_bytes())
        .context("parse signed customer Identity CA")?;
    if certificates.is_empty() {
        bail!("Customer Identity CA contains no certificates");
    }
    Ok(Some(certificates))
}

fn verify(
    config: &DisconnectedAuthority,
    metadata: &ManagedPolicyMetadata,
    env: &HashMap<String, String>,
) -> Result<(PlatformSession, PinnedAuthority)> {
    let workspace = metadata
        .workspace_id
        .as_deref()
        .filter(|v| !v.trim().is_empty())
        .context("Customer Code authority requires signed workspace scope")?;
    if metadata.org_id.trim().is_empty() {
        bail!("Customer Code authority requires signed organization scope");
    }
    let token = env
        .get(&config.credential_env)
        .filter(|v| !v.trim().is_empty())
        .context("Customer Code authority credential is unavailable")?
        .clone();
    let pinned = resolve_authority(config)?;
    let config = config.clone();
    let expected_org = metadata.org_id.clone();
    let expected_workspace = workspace.to_owned();
    std::thread::spawn(move || {
        let mut builder = reqwest::blocking::Client::builder()
            .resolve_to_addrs(&pinned.host, &pinned.addresses)
            .no_proxy()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT);
        if let Some(certificates) = certificates(&config)? {
            builder = builder.use_rustls_tls().tls_built_in_root_certs(false);
            for certificate in certificates {
                builder = builder.add_root_certificate(certificate);
            }
        }
        let response = builder
            .build()?
            .post(format!(
                "{}/v1/tokens/introspect",
                config.endpoint.trim_end_matches('/')
            ))
            .bearer_auth(&token)
            .send()
            .context("verify customer Identity credential")?;
        if !response.status().is_success() {
            bail!("Customer Identity rejected credential verification");
        }
        let mut body = Vec::new();
        response
            .take(MAX_RESPONSE_BYTES + 1)
            .read_to_end(&mut body)?;
        if body.len() > MAX_RESPONSE_BYTES as usize {
            bail!("Customer Identity response exceeds size limit");
        }
        let introspection: IdentityIntrospection = serde_json::from_slice(&body)?;
        let session = PlatformSession {
            access_token: token,
            organization_id: expected_org.clone(),
            workspace_id: Some(expected_workspace.clone()),
            provider_ref: serde_json::Value::Null,
            email: None,
            user_id: None,
        };
        let verified =
            verified_platform_session_for_scope(session, introspection, "code:tools:execute")?;
        if verified.organization_id != expected_org
            || verified.workspace_id.as_deref() != Some(expected_workspace.as_str())
        {
            bail!("Customer Identity token does not match signed policy tenant scope");
        }
        Ok((verified, pinned))
    })
    .join()
    .map_err(|_| anyhow::anyhow!("Customer Identity verification worker failed"))?
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::{io::Write as _, sync::Arc};

    fn metadata() -> ManagedPolicyMetadata {
        ManagedPolicyMetadata {
            org_id: "customer-org".into(),
            workspace_id: Some("customer-workspace".into()),
            policy_version: 1,
            issued_at: 1,
            expires_at: u64::MAX,
            key_id: "operator-key".into(),
            policy_hash: "verified-by-policy-owner".into(),
            kill_switch: false,
        }
    }

    fn active() -> serde_json::Value {
        json!({"active":true,"subject":"customer-user","token_type":"access",
            "organization_id":"customer-org","workspace_id":"customer-workspace","scope":"code:tools:execute"})
    }

    fn fixture(
        body: String,
        status: &str,
        trusted: bool,
    ) -> (DisconnectedAuthority, std::thread::JoinHandle<()>) {
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let other = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let authority = DisconnectedAuthority {
            endpoint: format!("https://localhost:{port}"),
            credential_env: "CUSTOMER_CODE_TOKEN".into(),
            ca_pem: Some(if trusted {
                cert.cert.pem()
            } else {
                other.cert.pem()
            }),
        };
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nLocation: https://identity.evalops.dev/v1/tokens/introspect\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let server = std::thread::spawn(move || {
            listener.set_nonblocking(true).unwrap();
            let deadline = std::time::Instant::now() + TIMEOUT;
            let stream = loop {
                match listener.accept() {
                    Ok((stream, _)) => break stream,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(std::time::Instant::now() < deadline);
                        std::thread::sleep(Duration::from_millis(5));
                    }
                    Err(error) => panic!("fixture accept: {error}"),
                }
            };
            // Accepted sockets inherit O_NONBLOCK from this listener on macOS.
            // Rustls expects a blocking stream in this fixture.
            stream.set_nonblocking(false).unwrap();
            stream.set_read_timeout(Some(TIMEOUT)).unwrap();
            let connection = rustls::ServerConnection::new(Arc::new(tls)).unwrap();
            let mut stream = rustls::StreamOwned::new(connection, stream);
            let mut request = Vec::new();
            let mut buffer = [0; 4096];
            loop {
                match stream.read(&mut buffer) {
                    Ok(0) => {
                        assert!(!trusted);
                        return;
                    }
                    Err(error) => {
                        assert!(!trusted, "trusted fixture TLS read failed: {error:?}");
                        return;
                    }
                    Ok(count) => request.extend_from_slice(&buffer[..count]),
                }
                if request.windows(4).any(|part| part == b"\r\n\r\n") {
                    break;
                }
            }
            let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
            assert!(request.starts_with("post /v1/tokens/introspect "));
            assert!(request.contains("authorization: bearer customer-token"));
            assert!(!request.contains("vendor-token"));
            stream.write_all(response.as_bytes()).unwrap();
            stream.flush().unwrap();
        });
        (authority, server)
    }

    #[test]
    fn private_code_authority_verifies_tls_token_scope_and_signed_tenant() {
        let env = HashMap::from([
            ("CUSTOMER_CODE_TOKEN".into(), "customer-token".into()),
            ("MAESTRO_EVALOPS_ACCESS_TOKEN".into(), "vendor-token".into()),
        ]);
        let mut cases = vec![(active(), true)];
        for (field, value) in [
            ("active", json!(false)),
            ("token_type", json!("service")),
            ("scope", json!("llm_gateway:invoke")),
            ("organization_id", json!("other-org")),
            ("workspace_id", json!("other-workspace")),
            ("workspace_id", serde_json::Value::Null),
        ] {
            let mut response = active();
            response[field] = value;
            cases.push((response, false));
        }
        for (response, expected) in cases {
            let (config, server) = fixture(response.to_string(), "200 OK", true);
            let result = verify(&config, &metadata(), &env);
            assert_eq!(result.is_ok(), expected, "{result:?}");
            server.join().unwrap();
        }
        for (status, trusted, body) in [
            ("302 Found", true, active().to_string()),
            ("200 OK", false, active().to_string()),
            ("200 OK", true, "malformed".into()),
        ] {
            let (config, server) = fixture(body, status, trusted);
            assert!(verify(&config, &metadata(), &env).is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn private_code_authority_rejects_every_forbidden_dns_answer() {
        for ip in [
            "169.254.169.254",
            "100.100.100.200",
            "0.0.0.0",
            "224.0.0.1",
            "192.0.2.1",
            "::",
            "fe80::1",
            "ff02::1",
            "fd00:ec2::254",
            "::ffff:169.254.169.254",
            "64:ff9b::a9fe:a9fe",
        ] {
            let good = "10.1.2.3:443".parse().unwrap();
            let forbidden = SocketAddr::new(ip.parse().unwrap(), 443);
            assert!(
                checked_addresses(vec![good, forbidden], 443).is_err(),
                "{ip}"
            );
        }
        for ip in ["10.1.2.3", "127.0.0.1", "::1", "fd12::1", "8.8.8.8"] {
            assert!(
                checked_addresses(vec![SocketAddr::new(ip.parse().unwrap(), 443)], 443).is_ok()
            );
        }
        assert!(checked_addresses(vec![], 443).is_err());
        assert!(checked_addresses(vec!["10.1.2.3:80".parse().unwrap()], 443).is_err());
    }

    #[test]
    fn private_code_authority_never_falls_back_to_vendor_credentials_or_tenant_hints() {
        let config = DisconnectedAuthority {
            endpoint: "https://customer.invalid".into(),
            credential_env: "CUSTOMER_CODE_TOKEN".into(),
            ca_pem: None,
        };
        let env = HashMap::from([
            ("MAESTRO_EVALOPS_ACCESS_TOKEN".into(), "vendor-token".into()),
            ("MAESTRO_EVALOPS_ORG_ID".into(), "customer-org".into()),
        ]);
        assert!(
            verify(&config, &metadata(), &env)
                .unwrap_err()
                .to_string()
                .contains("credential is unavailable")
        );
        let mut scope = metadata();
        scope.workspace_id = None;
        assert!(
            verify(&config, &scope, &env)
                .unwrap_err()
                .to_string()
                .contains("signed workspace")
        );
    }
}
