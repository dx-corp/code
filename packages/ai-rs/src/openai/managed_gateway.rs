use super::*;

fn required_managed_receipt_header(
    headers: &HeaderMap,
    name: &'static str,
    max_len: usize,
) -> Result<String> {
    let value = headers
        .get(name)
        .with_context(|| format!("managed Gateway response is missing required {name} header"))?
        .to_str()
        .with_context(|| format!("managed Gateway response has invalid {name} header"))?;
    let value = value.trim();
    if value.is_empty() {
        anyhow::bail!("managed Gateway response has empty {name} header");
    }
    if value.len() > max_len {
        anyhow::bail!("managed Gateway response {name} header exceeds the {max_len}-byte limit");
    }
    Ok(value.to_string())
}

pub(super) fn managed_provider_tools_evidence(
    headers: &reqwest::header::HeaderMap,
) -> Option<(String, u32)> {
    let digest = headers
        .get("x-evalops-provider-tools-sha256")?
        .to_str()
        .ok()?;
    let hex = digest.strip_prefix("sha256:")?;
    if hex.len() != 64
        || !hex
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return None;
    }
    let count = headers
        .get("x-evalops-provider-tool-count")?
        .to_str()
        .ok()?;
    if count.is_empty() || !count.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((digest.to_owned(), count.parse().ok()?))
}

pub(super) fn managed_gateway_receipt(
    headers: &HeaderMap,
    expected_lineage: Option<&str>,
) -> Result<ManagedGatewayReceipt> {
    let lineage_id = required_managed_receipt_header(
        headers,
        "x-evalops-lineage-id",
        MANAGED_GATEWAY_RECEIPT_LINEAGE_MAX_LEN,
    )?;
    if let Some(expected_lineage) = expected_lineage {
        if lineage_id != expected_lineage {
            anyhow::bail!("managed Gateway response receipt lineage mismatch");
        }
    }
    Ok(ManagedGatewayReceipt {
        provider_prompt_sha256: None,
        provider_tools_sha256: None,
        provider_tool_count: None,
        request_id: required_managed_receipt_header(
            headers,
            "x-request-id",
            MANAGED_GATEWAY_RECEIPT_ID_MAX_LEN,
        )?,
        record_id: required_managed_receipt_header(
            headers,
            "x-evalops-record-id",
            MANAGED_GATEWAY_RECEIPT_ID_MAX_LEN,
        )?,
        lineage_id,
        record_status: required_managed_receipt_header(
            headers,
            "x-evalops-record-status",
            MANAGED_GATEWAY_RECEIPT_STATUS_MAX_LEN,
        )?,
    })
}

pub(super) fn managed_gateway_cooldown_retry_after(
    managed_gateway: bool,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
) -> Option<std::time::Duration> {
    if !managed_gateway {
        return None;
    }
    let code = headers.get("x-evalops-error-code")?;
    let cooling = (status == reqwest::StatusCode::SERVICE_UNAVAILABLE
        && code == "provider_cooldown_open")
        || (status == reqwest::StatusCode::BAD_GATEWAY && code == "provider_execution_failed");
    if !cooling {
        return None;
    }
    let seconds = headers
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .parse::<u64>()
        .ok()?;
    Some(std::time::Duration::from_secs(seconds.clamp(1, 30)))
}

pub(super) fn managed_gateway_error_retry_after(
    managed_gateway: bool,
    status: reqwest::StatusCode,
    headers: &reqwest::header::HeaderMap,
    lineage_id: Option<&str>,
) -> Option<std::time::Duration> {
    let retry_after = managed_gateway_cooldown_retry_after(managed_gateway, status, headers);
    if managed_gateway {
        let bounded_header = |name: &str, max_len: usize| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .filter(|value| !value.is_empty() && value.len() <= max_len)
                .unwrap_or("")
        };
        tracing::warn!(
            target: "maestro.llm",
            event = "managed_gateway_attempt_failed",
            gateway_request_id = bounded_header("x-request-id", 128),
            gateway_record_id = bounded_header("x-evalops-record-id", 128),
            gateway_error_code = bounded_header("x-evalops-error-code", 64),
            lineage_id = lineage_id.filter(|value| value.len() <= 128).unwrap_or(""),
            status = status.as_u16(),
            retry_after_ms = retry_after.map_or(0, |delay| delay.as_millis() as u64),
            "managed gateway attempt failed"
        );
    }
    retry_after
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn only_typed_managed_cooldown_controls_retry_delay() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-evalops-error-code",
            "provider_cooldown_open".parse().unwrap(),
        );
        headers.insert(reqwest::header::RETRY_AFTER, "30".parse().unwrap());
        assert_eq!(
            managed_gateway_cooldown_retry_after(
                true,
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                &headers
            ),
            Some(std::time::Duration::from_secs(30))
        );
        assert_eq!(
            managed_gateway_cooldown_retry_after(
                false,
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                &headers
            ),
            None
        );
        headers.insert("x-evalops-error-code", "other_error".parse().unwrap());
        assert_eq!(
            managed_gateway_cooldown_retry_after(
                true,
                reqwest::StatusCode::SERVICE_UNAVAILABLE,
                &headers
            ),
            None
        );
    }

    #[test]
    fn attempted_failure_that_opens_cooldown_respects_retry_after() {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            "x-evalops-error-code",
            "provider_execution_failed".parse().unwrap(),
        );
        headers.insert(reqwest::header::RETRY_AFTER, "29".parse().unwrap());
        assert_eq!(
            managed_gateway_cooldown_retry_after(true, reqwest::StatusCode::BAD_GATEWAY, &headers),
            Some(std::time::Duration::from_secs(29))
        );
        headers.remove(reqwest::header::RETRY_AFTER);
        assert_eq!(
            managed_gateway_cooldown_retry_after(true, reqwest::StatusCode::BAD_GATEWAY, &headers),
            None
        );
    }
}
