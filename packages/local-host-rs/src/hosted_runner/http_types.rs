//! HTTP request and response types shared by hosted route owners.
use super::*;

pub(super) struct HttpRequest {
    pub(super) method: String,
    pub(super) path: String,
    pub(super) query: HashMap<String, String>,
    pub(super) raw_query: Option<String>,
    pub(super) headers: HashMap<String, String>,
    pub(super) body: Vec<u8>,
}

pub(super) enum ResponseBody {
    Bytes {
        status: u16,
        content_type: String,
        body: Vec<u8>,
    },
    Json {
        status: u16,
        body: serde_json::Value,
    },
    Sse {
        replay: Vec<StreamEnvelope>,
        rx: broadcast::Receiver<StreamEnvelope>,
        // Keep the common JSON response representation small. `SharedRunner`
        // owns the runtime state and event-pump handles, so storing it inline
        // makes this enum unnecessarily large even though only SSE responses
        // need it.
        shared: Box<SharedRunner>,
        filter: Box<TranscriptStreamFilter>,
        controller_authorization: Option<ControllerStreamAuthorization>,
    },
}

pub(super) async fn read_request<S>(socket: &mut S) -> io::Result<Option<HttpRequest>>
where
    S: AsyncRead + Unpin,
{
    let mut buffer = Vec::new();
    let mut header_end = None;
    loop {
        let mut chunk = [0_u8; 1024];
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            if buffer.is_empty() {
                return Ok(None);
            }
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = find_header_end(&buffer) {
            header_end = Some(position);
            break;
        }
        if buffer.len() > 64 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "request headers too large",
            ));
        }
    }
    let Some(header_end) = header_end else {
        return Ok(None);
    };
    let headers_text = String::from_utf8_lossy(&buffer[..header_end]);
    let mut lines = headers_text.split("\r\n");
    let Some(request_line) = lines.next() else {
        return Ok(None);
    };
    let mut request_parts = request_line.split_whitespace();
    let method = request_parts.next().unwrap_or("").to_string();
    let target = request_parts.next().unwrap_or("/");
    let (path, query) = parse_target(target);
    let raw_query = target.split_once('?').map(|(_, query)| query.to_owned());
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let content_length = headers
        .get("content-length")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(0);
    if (path.starts_with("/api/native-code/") || path == "/credential")
        && (content_length
            > if path == "/credential" {
                0
            } else {
                native_code::REQUEST_LIMIT
            }
            || headers.contains_key("transfer-encoding"))
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "native Code request body exceeds its bounded contract",
        ));
    }
    let body_start = header_end + 4;
    let mut body = buffer[body_start..].to_vec();
    while body.len() < content_length {
        let mut chunk = vec![0_u8; content_length - body.len()];
        let read = socket.read(&mut chunk).await?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Ok(Some(HttpRequest {
        method,
        path,
        query,
        raw_query,
        headers,
        body,
    }))
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer.windows(4).position(|window| window == b"\r\n\r\n")
}

pub(super) fn parse_target(target: &str) -> (String, HashMap<String, String>) {
    let (path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let mut query = HashMap::new();
    for pair in raw_query.split('&').filter(|pair| !pair.is_empty()) {
        let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
        query.insert(key.to_string(), value.to_string());
    }
    (path.to_string(), query)
}
