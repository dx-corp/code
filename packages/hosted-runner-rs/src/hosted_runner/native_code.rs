//! Native coding facade. Only the workload-mTLS runner-host peer may set the
//! principal; its account token and service credential never reach the child.
use super::*;

pub(super) const REQUEST_LIMIT: usize = 2 * 1024 * 1024;
const RESPONSE_LIMIT: usize = 16 * 1024 * 1024;

fn unavailable() -> HostedError {
    HostedError::new(
        HostedRunnerErrorCode::RuntimeNotReady,
        "native Code owner is starting or unavailable",
    )
}
fn denied() -> HostedError {
    HostedError::new(
        HostedRunnerErrorCode::AccessDenied,
        "native Code requires the authenticated runner owner",
    )
}

fn route_allowed(method: &str, path: &str) -> bool {
    if path.contains('%')
        || path.contains('\\')
        || path.contains('?')
        || path.contains('#')
        || path.split('/').any(|v| matches!(v, "" | "." | ".."))
    {
        return false;
    }
    let parts: Vec<_> = path.split('/').collect();
    match parts.as_slice() {
        ["api", "native", "capabilities"] => method == "GET",
        ["api", "native", "turns"] | ["api", "sessions"] => matches!(method, "GET" | "POST"),
        ["api", "native", "turns", _]
        | ["api", "sessions", _]
        | ["api", "sessions", _, "page" | "turn-diff"] => method == "GET",
        ["api", "native", "turns", _, "control"] | ["api", "pending-requests", _, "resume"] => {
            method == "POST"
        }
        _ => false,
    }
}

pub(super) async fn proxy(
    request: HttpRequest,
    shared: &SharedRunner,
    verified_mtls: bool,
) -> HostedResult<ResponseBody> {
    let url = std::env::var("MAESTRO_NATIVE_CODE_URL").ok();
    let secret = std::env::var("MAESTRO_WEB_TRUST_PROXY_AUTH_TOKEN").ok();
    proxy_inner(
        request,
        shared,
        verified_mtls,
        url.as_deref(),
        secret.as_deref(),
    )
    .await
}

async fn proxy_inner(
    request: HttpRequest,
    shared: &SharedRunner,
    verified_mtls: bool,
    base_url: Option<&str>,
    proxy_secret: Option<&str>,
) -> HostedResult<ResponseBody> {
    if !verified_mtls {
        return Err(denied());
    }
    let identity = shared
        .config
        .workload_identity
        .as_ref()
        .ok_or_else(denied)?;
    let path = request
        .path
        .strip_prefix("/api/native-code/")
        .ok_or_else(denied)?;
    if !route_allowed(&request.method, path) || request.body.len() > REQUEST_LIMIT {
        return Err(denied());
    }
    let field = |name: &str| {
        request
            .headers
            .get(name)
            .filter(|v| !v.is_empty() && v.len() <= 512 && v.trim() == v.as_str())
            .ok_or_else(denied)
    };
    let subject = field("x-auth-request-user")?;
    let required_scope = if request.method == "GET" {
        "console:read"
    } else {
        "console:write"
    };
    if field("x-auth-request-scope")? != required_scope
        || field("x-evalops-organization-id")? != &identity.organization_id
        || field("x-evalops-workspace-id")? != &identity.workspace_id
    {
        return Err(denied());
    }
    let _mutation = if request.method == "POST" {
        let mutation = shared.mutation_lifecycle.lock().await;
        shared.ensure_mutation_allowed()?;
        Some(mutation)
    } else {
        None
    };
    let proxy_secret = proxy_secret
        .filter(|v| !v.is_empty() && v.len() <= 512)
        .ok_or_else(unavailable)?;
    let mut url = url::Url::parse(base_url.ok_or_else(unavailable)?).map_err(|_| unavailable())?;
    if url.scheme() != "http"
        || url.host_str() != Some("127.0.0.1")
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(unavailable());
    }
    url.set_path(&format!("/{path}"));
    url.set_query(request.raw_query.as_deref());
    let client = reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|_| unavailable())?;
    // New request and explicit headers prevent injection or token forwarding.
    let mut response = client
        .request(
            reqwest::Method::from_bytes(request.method.as_bytes()).map_err(|_| denied())?,
            url,
        )
        .header("x-maestro-proxy-auth", proxy_secret)
        .header("x-auth-request-user", subject)
        .header("x-auth-request-scope", required_scope)
        .header("x-evalops-organization-id", &identity.organization_id)
        .header("x-evalops-workspace-id", &identity.workspace_id)
        .header("content-type", "application/json")
        .body(request.body)
        .send()
        .await
        .map_err(|_| unavailable())?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/json")
        .to_owned();
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(|_| unavailable())? {
        if body.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(unavailable());
        }
        body.extend_from_slice(&chunk);
    }
    Ok(ResponseBody::Bytes {
        status,
        content_type,
        body,
    })
}

#[cfg(test)]
mod tests;
