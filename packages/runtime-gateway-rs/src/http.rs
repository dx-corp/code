use serde::Serialize;
use std::collections::HashMap;
use std::env;
use std::future::Future;
use tokio::io::AsyncReadExt;
use tokio::net::TcpStream;

tokio::task_local! {
    static RESPONSE_CORS_ORIGIN: String;
}

pub(crate) const WILDCARD_ORIGIN: &str = "*";
const MAX_HEADER_BYTES: usize = 64 * 1024;
pub(crate) const MAX_JSON_BODY_BYTES: usize = 32 * 1024 * 1024;
const CORS_ALLOW_HEADERS: &str = "authorization,content-type,a2a-version,a2a-extensions,traceparent,tracestate,x-a2a-notification-token,x-organization-id,x-evalops-agent-id,x-evalops-actor-id,x-evalops-session-id,x-evalops-workspace-id,x-composer-artifact-access,x-composer-api-key,x-composer-approval-mode,x-composer-client,x-composer-client-tools,x-composer-csrf,x-composer-agent-id,x-composer-slim-events,x-composer-workspace,x-composer-workspace-id,x-maestro-artifact-access,x-maestro-api-key,x-maestro-approval-mode,x-maestro-agent-id,x-maestro-client,x-maestro-client-tools,x-maestro-csrf,x-maestro-slim-events,x-maestro-workspace,x-maestro-workspace-id,x-csrf-token,x-xsrf-token";

#[derive(Clone, Debug)]
pub(crate) struct RequestHead {
    pub(crate) method: String,
    pub(crate) path: String,
    pub(crate) query: HashMap<String, String>,
    pub(crate) headers: HashMap<String, String>,
}

pub(crate) async fn read_request_head(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
) -> Result<RequestHead, String> {
    let mut chunk = [0_u8; 4096];
    loop {
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("connection closed before request headers".into());
        }
        initial.extend_from_slice(&chunk[..read]);
        if initial.windows(4).any(|window| window == b"\r\n\r\n") {
            break;
        }
        if initial.len() > MAX_HEADER_BYTES {
            return Err("request headers exceeded limit".into());
        }
    }
    parse_request_head(initial)
}

pub(crate) async fn read_request_body(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
) -> Result<Vec<u8>, String> {
    read_request_body_with_limit(stream, initial, head, MAX_JSON_BODY_BYTES).await
}

pub(crate) async fn read_request_body_with_limit(
    stream: &mut TcpStream,
    initial: &mut Vec<u8>,
    head: &RequestHead,
    max_body_bytes: usize,
) -> Result<Vec<u8>, String> {
    let header_end = header_end(initial)?;
    let body_start = header_end + 4;
    let content_length = head
        .headers
        .get("content-length")
        .and_then(|value| parse_content_length(value))
        .ok_or_else(|| "content-length is required".to_string())?;
    if content_length > max_body_bytes {
        return Err(format!(
            "request body exceeded limit: {content_length} > {max_body_bytes}"
        ));
    }

    while initial.len().saturating_sub(body_start) < content_length {
        let mut chunk = [0_u8; 8192];
        let read = stream
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if read == 0 {
            return Err("connection closed before request body completed".into());
        }
        initial.extend_from_slice(&chunk[..read]);
        if initial.len().saturating_sub(body_start) > max_body_bytes {
            return Err("request body exceeded limit".into());
        }
    }

    Ok(initial[body_start..body_start + content_length].to_vec())
}

pub(crate) fn parse_request_head(initial: &[u8]) -> Result<RequestHead, String> {
    let header_end = header_end(initial)?;
    let header_text = std::str::from_utf8(&initial[..header_end])
        .map_err(|error| format!("request headers are not utf-8: {error}"))?;
    let mut lines = header_text.split("\r\n");
    let request_line = lines
        .next()
        .ok_or_else(|| "request line missing".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "request method missing".to_string())?
        .to_uppercase();
    let raw_target = parts
        .next()
        .ok_or_else(|| "request target missing".to_string())?;
    let (path, query) = raw_target
        .split_once('?')
        .map(|(path, query)| (path.to_string(), parse_query(query)))
        .unwrap_or_else(|| (raw_target.to_string(), HashMap::new()));
    let mut headers = HashMap::new();
    for (name, value) in lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_string()))
    {
        headers
            .entry(name)
            .and_modify(|existing: &mut String| {
                existing.push_str(", ");
                existing.push_str(&value);
            })
            .or_insert(value);
    }
    Ok(RequestHead {
        method,
        path,
        query,
        headers,
    })
}

fn parse_content_length(value: &str) -> Option<usize> {
    let mut lengths = value
        .split(',')
        .map(|part| part.trim().parse::<usize>().ok());
    let first = lengths.next()??;
    lengths.all(|length| length == Some(first)).then_some(first)
}

fn parse_query(query: &str) -> HashMap<String, String> {
    query
        .split('&')
        .filter(|part| !part.is_empty())
        .filter_map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            if key.is_empty() {
                None
            } else {
                Some((
                    percent_decode_component(key),
                    percent_decode_component(value),
                ))
            }
        })
        .collect()
}

pub(crate) fn query_flag(head: &RequestHead, name: &str) -> bool {
    head.query
        .get(name)
        .map(|value| !matches!(value.as_str(), "0" | "false" | "off"))
        .unwrap_or(false)
}

pub(crate) fn header_end(initial: &[u8]) -> Result<usize, String> {
    initial
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| "request header terminator not found".to_string())
}

pub(crate) fn text_response(status: u16, body: &str) -> Vec<u8> {
    response(status, "text/plain; charset=utf-8", body.as_bytes())
}

pub(crate) fn json_response<T: Serialize>(status: u16, value: &T) -> Vec<u8> {
    let body = serde_json::to_vec(value)
        .unwrap_or_else(|_| br#"{"error":"failed to serialize response"}"#.to_vec());
    response(status, "application/json", &body)
}

pub(crate) fn response(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    response_with_extra_headers(status, content_type, body, "")
}

pub(crate) fn response_with_no_store(status: u16, content_type: &str, body: &[u8]) -> Vec<u8> {
    response_with_extra_headers_and_length(
        status,
        content_type,
        body,
        "Cache-Control: no-store, no-cache, must-revalidate\r\n",
        body.len(),
    )
}

pub(crate) fn response_with_extra_headers(
    status: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: &str,
) -> Vec<u8> {
    response_with_extra_headers_and_length(status, content_type, body, extra_headers, body.len())
}

pub(crate) fn response_with_extra_headers_and_length(
    status: u16,
    content_type: &str,
    body: &[u8],
    extra_headers: &str,
    content_length: usize,
) -> Vec<u8> {
    debug_assert!(
        body.is_empty() || body.len() == content_length,
        "response Content-Length {content_length} does not match body length {}",
        body.len()
    );
    let reason = match status {
        200 => "OK",
        202 => "Accepted",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        426 => "Upgrade Required",
        413 => "Payload Too Large",
        421 => "Misdirected Request",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        503 => "Service Unavailable",
        _ => "OK",
    };
    let cors_origin = response_cors_origin();
    let mut head = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nAccess-Control-Allow-Origin: {}\r\nVary: Origin\r\n{}Access-Control-Allow-Headers: {CORS_ALLOW_HEADERS}\r\nAccess-Control-Allow-Methods: GET,POST,PATCH,DELETE,OPTIONS\r\n",
        content_length,
        cors_origin,
        cors_credentials_header(&cors_origin)
    );
    if !extra_headers.is_empty() {
        head.push_str(extra_headers);
        if !extra_headers.ends_with("\r\n") {
            head.push_str("\r\n");
        }
    }
    head.push_str("\r\n");
    let mut bytes = head.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

pub(crate) async fn with_response_cors_origin<F>(origin: String, future: F) -> F::Output
where
    F: Future,
{
    RESPONSE_CORS_ORIGIN.scope(origin, future).await
}

pub(crate) fn requested_cors_origin(head: &RequestHead) -> String {
    let configured = cors_origins();
    if configured.iter().any(|origin| origin == WILDCARD_ORIGIN) {
        // A wildcard configuration means "public API". Answer with the literal
        // wildcard rather than reflecting the caller's origin, so browsers
        // refuse credentialed cross-origin requests instead of being handed an
        // `Access-Control-Allow-Origin` that names the attacker's page.
        return WILDCARD_ORIGIN.into();
    }
    head.headers
        .get("origin")
        .map(|origin| origin.trim())
        .filter(|origin| !origin.is_empty() && origin_allowed(head))
        .map(|origin| origin.to_string())
        .unwrap_or_else(cors_origin)
}

pub(crate) fn response_cors_origin() -> String {
    RESPONSE_CORS_ORIGIN
        .try_with(Clone::clone)
        .unwrap_or_else(|_| cors_origin())
}

pub(crate) fn response_cors_credentials_header() -> &'static str {
    RESPONSE_CORS_ORIGIN
        .try_with(|origin| cors_credentials_header(origin))
        .unwrap_or_else(|_| cors_credentials_header(&cors_origin()))
}

/// Credentials are decided from the *configured* origin, never from the value
/// reflected back to the caller. A wildcard configuration can therefore never
/// combine `Access-Control-Allow-Credentials: true` with a reflected attacker
/// origin, which is what let any visited page drive the local agent runtime.
fn cors_credentials_header(reflected_origin: &str) -> &'static str {
    if reflected_origin == WILDCARD_ORIGIN
        || cors_origins()
            .iter()
            .any(|origin| origin == WILDCARD_ORIGIN)
    {
        ""
    } else {
        "Access-Control-Allow-Credentials: true\r\n"
    }
}

pub(crate) fn cors_origin() -> String {
    cors_origins()
        .into_iter()
        .next()
        .unwrap_or_else(|| "http://localhost:4173".into())
}

fn cors_origins() -> Vec<String> {
    let configured = env::var("MAESTRO_WEB_ORIGINS")
        .ok()
        .or_else(|| env::var("MAESTRO_WEB_ORIGIN").ok())
        .unwrap_or_else(|| "http://localhost:4173".into());
    configured
        .split(',')
        .map(str::trim)
        .filter(|origin| !origin.is_empty())
        .map(str::to_string)
        .collect()
}

pub(crate) fn origin_allowed(head: &RequestHead) -> bool {
    let Some(origin) = head.headers.get("origin").map(|origin| origin.trim()) else {
        return true;
    };
    origin.is_empty() || origin_allowed_value(origin)
}

fn origin_allowed_value(origin: &str) -> bool {
    let configured_origins = cors_origins();
    if configured_origins
        .iter()
        .any(|configured| configured == WILDCARD_ORIGIN || origin == configured)
    {
        return true;
    }
    matches!(
        origin,
        "http://localhost:4173"
            | "http://localhost:8080"
            | "http://localhost:3000"
            | "http://localhost:5173"
            | "http://127.0.0.1:4173"
            | "http://127.0.0.1:8080"
            | "http://127.0.0.1:3000"
            | "http://127.0.0.1:5173"
            | "http://[::1]:4173"
            | "http://[::1]:8080"
            | "http://[::1]:3000"
            | "http://[::1]:5173"
    )
}

pub(crate) fn percent_decode_component(value: &str) -> String {
    let normalized = value.replace('+', " ");
    let bytes = normalized.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' && index + 2 < bytes.len() {
            if let Ok(hex) = std::str::from_utf8(&bytes[index + 1..index + 3]) {
                if let Ok(byte) = u8::from_str_radix(hex, 16) {
                    decoded.push(byte);
                    index += 3;
                    continue;
                }
            }
        }
        decoded.push(bytes[index]);
        index += 1;
    }
    String::from_utf8_lossy(&decoded).to_string()
}

pub(crate) fn mime_for_path(path: &std::path::Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "application/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        Some("ico") => "image/x-icon",
        Some("wasm") => "application/wasm",
        _ => "application/octet-stream",
    }
}
