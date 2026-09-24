//! Local drafts extend the existing session log. Submitted reports are owned by
//! ProductIssueReport, including staff engagement and notification delivery.
use std::{
    collections::HashMap,
    io::{BufRead, BufReader},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result, bail, ensure};
use prost::Message;
use serde::{Deserialize, Serialize};

use crate::session::CustomEntry;
use crate::session::{SessionEntry, SessionManager};

const ENTRY_TYPE: &str = "product_issue_draft_v1";
const SUBMIT_PATH: &str = "/deixicpublic.v1.DeixicPublicService/SubmitIssueReport";

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Destination {
    pub endpoint: String,
    pub organization_id: String,
    pub workspace_id: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum DraftStatus {
    Draft,
    Reviewed,
    /// Written before HTTP: a retry must reuse the same destination and body.
    Sending,
    Sent {
        reference: String,
    },
    Dismissed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct BugReport {
    pub id: String,
    pub description: String,
    pub expected_behavior: String,
    pub app_version: String,
    pub include_diagnostics: bool,
    pub destination: Option<Destination>,
    pub status: DraftStatus,
    #[serde(default)]
    pub context: ReportContext,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default = "now_seconds")]
    pub created_at: i64,
}

impl BugReport {
    pub fn new(description: &str) -> Result<Self> {
        let description = redact(&bounded_text(description, false)?);
        Ok(Self {
            id: uuid::Uuid::new_v4().to_string(),
            description,
            expected_behavior: String::new(),
            app_version: std::env::var("MAESTRO_VERSION")
                .ok()
                .and_then(|version| semver::Version::parse(version.trim_start_matches('v')).ok())
                .map(|version| format!("Deixic Code {version}"))
                .filter(|version| version.len() <= 128)
                .unwrap_or_else(|| format!("Deixic Code runtime {}", env!("CARGO_PKG_VERSION"))),
            include_diagnostics: false,
            destination: None,
            status: DraftStatus::Draft,
            context: ReportContext::default(),
            hidden: false,
            created_at: now_seconds(),
        })
    }

    pub fn edit(
        &mut self,
        description: Option<&str>,
        expected: Option<&str>,
        diagnostics: Option<bool>,
    ) -> Result<()> {
        ensure!(
            matches!(self.status, DraftStatus::Draft | DraftStatus::Reviewed),
            "This report has already been submitted or dismissed. An uncertain send can be retried with /bug send; use /bug dismiss before starting a different report."
        );
        if let Some(text) = description {
            self.description = redact(&bounded_text(text, false)?);
        }
        if let Some(text) = expected {
            self.expected_behavior = redact(&bounded_text(text, true)?);
        }
        if let Some(enabled) = diagnostics {
            self.include_diagnostics = enabled;
        }
        self.status = DraftStatus::Draft;
        self.destination = None;
        Ok(())
    }

    pub fn preview(&self) -> String {
        let destination = self.destination.as_ref().map_or_else(
            || "Sign in and select a workspace to send.".to_owned(),
            |d| {
                format!(
                    "{}\nOrganization: {} · Workspace: {}",
                    d.endpoint, d.organization_id, d.workspace_id
                )
            },
        );
        let diagnostics = if self.include_diagnostics {
            self.app_version.as_str()
        } else {
            "None"
        };
        let evidence = self
            .context
            .evidence
            .iter()
            .map(|item| format!("{} [{}]:\n{}", item.kind, item.source_id, item.text))
            .collect::<Vec<_>>()
            .join("\n\n");
        let context = format!(
            "Reproduction steps:\n{}\n\nModel: {}\n\nSelected evidence:\n{}",
            self.context.reproduction_steps,
            if self.include_diagnostics {
                self.context.model.as_str()
            } else {
                "Not included"
            },
            if evidence.is_empty() {
                "None"
            } else {
                &evidence
            }
        );
        format!(
            "Bug report draft\n\nWhat happened:\n{}\n\nExpected behavior:\n{}\n\nDiagnostics: {}\nDestination: {}\n\n{context}\n\nOnly the fields above are sent. Check the description for private information.\n/bug draft <text> · /bug expected <text> · /bug diagnostics on|off\n/bug review · /bug send · /bug dismiss",
            self.description, self.expected_behavior, diagnostics, destination
        )
    }

    pub fn prepare_send(&mut self, destination: &Destination) -> Result<()> {
        ensure!(
            matches!(self.status, DraftStatus::Reviewed | DraftStatus::Sending),
            "Review this draft with /bug review before sending."
        );
        ensure!(
            self.destination.as_ref() == Some(destination),
            "The report destination or workspace changed. Restore the reviewed workspace to retry, or dismiss this draft and create a new report."
        );
        self.status = DraftStatus::Sending;
        Ok(())
    }
}

fn bounded_text(text: &str, allow_empty: bool) -> Result<String> {
    let text = text.trim();
    ensure!(
        allow_empty || !text.is_empty(),
        "Describe what happened with /bug draft <description>."
    );
    ensure!(
        text.len() <= 4000,
        "Report text must be at most 4000 bytes."
    );
    ensure!(
        !text
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t'),
        "Report text contains terminal control characters."
    );
    Ok(text.to_owned())
}

/// No new files or cleanup lifecycle: session deletion also deletes its drafts.
pub fn save(manager: &mut SessionManager, report: &BugReport) -> Result<()> {
    let writer = manager.writer().context(
        "Bug report drafts require session persistence; they are unavailable with --no-session.",
    )?;
    writer.write_entry(SessionEntry::Custom(CustomEntry {
        id: Some(uuid::Uuid::new_v4().to_string()),
        parent_id: None,
        timestamp: chrono::Utc::now().to_rfc3339(),
        custom_type: ENTRY_TYPE.to_owned(),
        data: Some(serde_json::to_value(report)?),
    }))?;
    writer.flush()?;
    Ok(())
}

pub fn load(path: Option<&Path>) -> Result<Option<BugReport>> {
    let Some(path) = path else { return Ok(None) };
    Ok(load_all(path)?.into_iter().last())
}

/// Fold append-only updates by draft ID; hiding a card does not discard its draft.
pub fn load_all(path: &Path) -> Result<Vec<BugReport>> {
    let mut reports: Vec<BugReport> = Vec::new();
    for line in BufReader::new(std::fs::File::open(path)?).lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let entry: serde_json::Value = serde_json::from_str(&line)
            .context("Could not read saved session; report submission stopped.")?;
        if entry.get("type").and_then(|v| v.as_str()) == Some("custom")
            && entry.get("customType").and_then(|v| v.as_str()) == Some(ENTRY_TYPE)
        {
            let draft: BugReport = serde_json::from_value(entry["data"].clone())
                .context("Could not read saved bug report.")?;
            reports.retain(|item| item.id != draft.id);
            reports.push(draft);
        }
    }
    Ok(reports)
}

pub struct FeedbackClient {
    pub destination: Destination,
    token: String,
    http: reqwest::Client,
}

impl FeedbackClient {
    pub fn resolve() -> Result<Self> {
        let snapshot = crate::init_cli::load_evalops_snapshot()?;
        let env: HashMap<String, String> = std::env::vars().collect();
        let session = crate::credential_mode::platform_session_from(snapshot.as_ref(), &env)
            .context("Sign in to Deixic Code before sending a report.")?;
        let base = env
            .get("MAESTRO_EVALOPS_BASE_URL")
            .map(String::as_str)
            .unwrap_or(crate::init_cli::DEFAULT_AGENT_MCP_BASE_URL);
        let destination = Destination {
            endpoint: endpoint(base)?,
            organization_id: session.organization_id,
            workspace_id: session
                .workspace_id
                .filter(|s| !s.trim().is_empty())
                .context("Select a workspace before sending a report.")?,
        };
        Self::new(destination, session.access_token)
    }

    fn new(destination: Destination, token: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            destination,
            token,
            http,
        })
    }

    pub async fn send(&self, report: &BugReport) -> Result<String> {
        ensure!(
            report.status == DraftStatus::Sending
                && report.destination.as_ref() == Some(&self.destination),
            "The report must be reviewed and saved before sending."
        );
        let request = SubmitRequest {
            scope: Some(ReportQuery {
                organization_id: self.destination.organization_id.clone(),
                workspace_id: self.destination.workspace_id.clone(),
            }),
            description: report.description.clone(),
            expected_behavior: report.expected_behavior.clone(),
            app_version: if report.include_diagnostics {
                report.app_version.clone()
            } else {
                String::new()
            },
            include_diagnostics: report.include_diagnostics,
            idempotency_key: report.id.clone(),
            context: report.outgoing_context().map(Into::into),
        };
        let response = self.http.post(&self.destination.endpoint).bearer_auth(&self.token)
            .header("connect-protocol-version", "1")
            .header("x-organization-id", &self.destination.organization_id)
            .header("x-workspace-id", &self.destination.workspace_id)
            .header("content-type", "application/proto")
            .header("accept", "application/proto")
            .body(request.encode_to_vec()).send().await
            .map_err(|_| anyhow::anyhow!("Submission could not be confirmed. The draft is saved; /bug send retries the same report."))?;
        if response.status() == reqwest::StatusCode::FORBIDDEN {
            bail!(
                "Feedback permission was refused. Sign in again with /login to request product_issues:write, or ask your workspace administrator. The draft is saved."
            );
        }
        if !response.status().is_success() {
            bail!(
                "Submission returned HTTP {}. The draft is saved; /bug send retries the same report.",
                response.status().as_u16()
            );
        }
        let mut response = response;
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Could not read report receipt; retry the saved report.")?
        {
            ensure!(
                body.len() + chunk.len() <= 65536,
                "Report receipt was too large; retry the saved report."
            );
            body.extend_from_slice(&chunk);
        }
        let response = SubmitResponse::decode(body.as_slice())
            .context("The service returned no valid receipt; the draft is saved for retry.")?;
        let receipt = response
            .report
            .context("The service returned no report receipt; the draft is saved for retry.")?;
        ensure!(
            !receipt.id.is_empty()
                && receipt.reference.starts_with("DX-")
                && receipt.reference.len() <= 64
                && receipt
                    .reference
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-'),
            "The service returned an invalid receipt; the draft is saved for retry."
        );
        Ok(receipt.reference)
    }
}

fn endpoint(base: &str) -> Result<String> {
    let mut url = url::Url::parse(base).context("Invalid platform URL")?;
    ensure!(
        url.scheme() == "https"
            && url.host_str().is_some()
            && url.username().is_empty()
            && url.password().is_none()
            && url.query().is_none()
            && url.fragment().is_none()
            && matches!(url.path(), "" | "/"),
        "The platform URL must be an HTTPS origin without credentials, a path, query, or fragment."
    );
    url.set_path(SUBMIT_PATH);
    Ok(url.into())
}

#[cfg(test)]
use crate::public_protocol::IssueReport as ReportReceipt;
use crate::public_protocol::{
    IssueContext, IssueEvidence, Scope as ReportQuery, SubmitIssueReportRequest as SubmitRequest,
    SubmitIssueReportResponse as SubmitResponse,
};

impl From<ReportContext> for IssueContext {
    fn from(context: ReportContext) -> Self {
        Self {
            reproduction_steps: context.reproduction_steps,
            model: context.model,
            evidence: context
                .evidence
                .into_iter()
                .map(|e| IssueEvidence {
                    kind: e.kind,
                    source_id: e.source_id,
                    text: e.text,
                })
                .collect(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn public_issue_report_wire_matches_reviewed_fixtures() {
        let mut request = SubmitRequest {
            context: None,
            scope: Some(ReportQuery {
                organization_id: "org-1".into(),
                workspace_id: "workspace-1".into(),
            }),
            description: "The terminal stopped responding.".into(),
            expected_behavior: "The next turn should start.".into(),
            app_version: "Deixic Code test".into(),
            include_diagnostics: true,
            idempotency_key: "native-1".into(),
        };
        use std::fmt::Write;
        let mut encoded = String::new();
        for byte in request.encode_to_vec() {
            write!(&mut encoded, "{byte:02x}").unwrap();
        }
        assert_eq!(
            encoded,
            include_str!("../../../test/fixtures/product-issue-report-public-v1.hex").trim()
        );
        request.context = Some(IssueContext {
            reproduction_steps: "Repeat failing tool".into(),
            model: "test-model".into(),
            evidence: vec![IssueEvidence {
                kind: "tool_result".into(),
                source_id: "call-1".into(),
                text: "wrong action".into(),
            }],
        });
        let mut encoded = String::new();
        for byte in request.encode_to_vec() {
            write!(&mut encoded, "{byte:02x}").unwrap();
        }
        assert_eq!(
            encoded,
            include_str!("../../../test/fixtures/product-issue-report-public-v2.hex").trim()
        );
    }

    #[test]
    fn edits_require_another_review_and_uncertain_sends_cannot_change_payload() {
        let mut draft = BugReport::new("The turn stopped").unwrap();
        let destination = Destination {
            endpoint: "https://example.test/report".into(),
            organization_id: "org-1".into(),
            workspace_id: "ws-1".into(),
        };
        assert!(draft.prepare_send(&destination).is_err());
        draft.destination = Some(destination.clone());
        draft.status = DraftStatus::Reviewed;
        draft.edit(None, Some("Complete the turn"), None).unwrap();
        assert!(draft.prepare_send(&destination).is_err());
        draft.destination = Some(destination.clone());
        draft.status = DraftStatus::Reviewed;
        let mut other = destination.clone();
        other.workspace_id = "ws-2".into();
        assert!(draft.prepare_send(&other).is_err());
        draft.prepare_send(&destination).unwrap();
        assert!(draft.edit(Some("Changed"), None, None).is_err());
        draft.prepare_send(&destination).unwrap();
    }
    #[test]
    fn rejects_empty_oversized_and_terminal_escape_text() {
        for description in [String::new(), "x".repeat(4001), "\x1b[2J".to_owned()] {
            assert!(BugReport::new(&description).is_err());
        }
    }
    #[test]
    fn collection_url_cannot_redirect_credentials_via_url_components() {
        for url in [
            "http://example.test",
            "https://user:pass@example.test",
            "https://example.test?key=x",
            "https://example.test/path",
            "https://example.test#x",
        ] {
            assert!(endpoint(url).is_err());
        }
        assert_eq!(
            endpoint("https://example.test").unwrap(),
            format!("https://example.test{SUBMIT_PATH}")
        );
    }
    #[tokio::test]
    async fn service_receipt_and_retry_use_same_bounded_reviewed_payload() {
        use tokio::{
            io::{AsyncReadExt, AsyncWriteExt},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let destination = Destination {
            endpoint: format!("http://{}{SUBMIT_PATH}", listener.local_addr().unwrap()),
            organization_id: "org-1".into(),
            workspace_id: "ws-1".into(),
        };
        let server = tokio::spawn(async move {
            let mut bodies = Vec::new();
            for status in ["503 Service Unavailable", "200 OK"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut bytes = Vec::new();
                let header_end = loop {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                    if let Some(i) = bytes.windows(4).position(|b| b == b"\r\n\r\n") {
                        break i + 4;
                    }
                };
                let headers = String::from_utf8_lossy(&bytes[..header_end]);
                assert!(headers.contains("x-organization-id: org-1"));
                assert!(headers.contains("x-workspace-id: ws-1"));
                let length: usize = headers
                    .lines()
                    .find_map(|line| line.strip_prefix("content-length: "))
                    .unwrap()
                    .parse()
                    .unwrap();
                while bytes.len() < header_end + length {
                    let mut chunk = [0; 4096];
                    let n = socket.read(&mut chunk).await.unwrap();
                    assert!(n > 0);
                    bytes.extend_from_slice(&chunk[..n]);
                }
                bodies
                    .push(SubmitRequest::decode(&bytes[header_end..header_end + length]).unwrap());
                let body = SubmitResponse {
                    report: Some(ReportReceipt {
                        id: "report-1".into(),
                        reference: "DX-123".into(),
                    }),
                }
                .encode_to_vec();
                socket.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/proto\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes()).await.unwrap();
                socket.write_all(&body).await.unwrap();
            }
            bodies
        });
        let client = FeedbackClient::new(destination.clone(), "test-token".into()).unwrap();
        let mut draft = BugReport::new("Turn stopped").unwrap();
        draft.destination = Some(destination.clone());
        draft.status = DraftStatus::Reviewed;
        draft.prepare_send(&destination).unwrap();
        assert!(client.send(&draft).await.is_err());
        assert_eq!(client.send(&draft).await.unwrap(), "DX-123");
        let bodies = server.await.unwrap();
        assert_eq!(bodies[0], bodies[1]);
        assert_eq!(bodies[0].app_version, "");
        assert!(!bodies[0].include_diagnostics);
    }
}

fn now_seconds() -> i64 {
    chrono::Utc::now().timestamp()
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportContext {
    #[serde(default)]
    pub reproduction_steps: String,
    #[serde(default)]
    pub model: String,
    #[serde(default)]
    pub evidence: Vec<ReportEvidence>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ReportEvidence {
    pub kind: String,
    pub source_id: String,
    pub text: String,
}

pub fn redact(text: &str) -> String {
    let text: String = text
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    crate::agent::credential_store::redact_credentials_in_json(&serde_json::Value::String(text))
        .as_str()
        .unwrap_or("[redacted]")
        .to_owned()
}

impl BugReport {
    pub fn outgoing_context(&self) -> Option<ReportContext> {
        let mut context = self.context.clone();
        if !self.include_diagnostics {
            context.model.clear();
        }
        (context != ReportContext::default()).then_some(context)
    }

    pub fn set_reproduction(&mut self, text: &str) -> Result<()> {
        let text = redact(&bounded_text(text, true)?);
        self.edit(None, None, None)?;
        self.context.reproduction_steps = text;
        Ok(())
    }

    pub fn set_evidence(&mut self, items: Vec<ReportEvidence>) -> Result<()> {
        ensure!(items.len() <= 10, "Choose at most 10 evidence items.");
        let bytes: usize = items.iter().map(|item| item.text.len()).sum();
        ensure!(
            bytes <= 16000,
            "Selected evidence must total at most 16000 bytes."
        );
        self.edit(None, None, None)?;
        self.context.evidence = items;
        Ok(())
    }

    /// Explicit local export uses the exact reviewed projection, never the raw session.
    pub fn export(&self, directory: &Path) -> Result<std::path::PathBuf> {
        use std::io::Write;
        ensure!(uuid::Uuid::parse_str(&self.id).is_ok(), "Invalid draft ID");
        std::fs::create_dir_all(directory)?;
        let path = directory.join(format!("report-{}-{}.json", self.id, uuid::Uuid::new_v4()));
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path)?;
        file.write_all(
            serde_json::to_string_pretty(&serde_json::json!({
                "schema": "maestro.feedback.v1", "id": self.id,
                "description": self.description, "expected_behavior": self.expected_behavior,
                "app_version": if self.include_diagnostics { &self.app_version } else { "" },
                "context": self.outgoing_context(),
            }))?
            .as_bytes(),
        )?;
        file.sync_all()?;
        Ok(path)
    }
}

/// A model can prepare a report, but cannot choose evidence, a destination, or send it.
pub fn draft_tool(args: serde_json::Value) -> crate::agent::ToolResult {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Args {
        description: String,
        expected_behavior: String,
        reproduction_steps: String,
    }
    let result = (|| -> Result<BugReport> {
        ensure!(
            std::env::var("MAESTRO_FEEDBACK_DRAFTS").as_deref() != Ok("off"),
            "Model-drafted feedback is turned off."
        );
        let args: Args = serde_json::from_value(args)?;
        let mut report = BugReport::new(&args.description)?;
        report.edit(None, Some(&args.expected_behavior), None)?;
        report.set_reproduction(&args.reproduction_steps)?;
        Ok(report)
    })();
    match result {
        Ok(report) => crate::agent::ToolResult::success("Feedback draft prepared for the user's review. Nothing was sent. Continue the user's task.")
            .with_details(serde_json::json!({"feedback_draft": report})),
        Err(error) => crate::agent::ToolResult::failure(format!("Could not prepare feedback: {error}")),
    }
}

#[cfg(test)]
mod parity_tests {
    use super::*;
    #[test]
    fn model_cannot_request_send_or_select_evidence() {
        for field in ["send", "evidence", "destination"] {
            let mut args = serde_json::json!({"description":"Tool ignored a correction", "expected_behavior":"Follow the correction", "reproduction_steps":"Correct the instruction and retry"});
            args[field] = serde_json::json!(true);
            assert!(!draft_tool(args).success);
        }
        let result = draft_tool(
            serde_json::json!({"description":"Tool ignored a correction", "expected_behavior":"Follow the correction", "reproduction_steps":"Correct the instruction and retry"}),
        );
        let report: BugReport =
            serde_json::from_value(result.details.unwrap()["feedback_draft"].clone()).unwrap();
        assert_eq!(report.status, DraftStatus::Draft);
        assert!(report.context.evidence.is_empty());
        assert!(report.destination.is_none());
    }
    #[test]
    fn evidence_edit_invalidates_consent_and_freezes_on_send() {
        let mut report = BugReport::new("Failure").unwrap();
        report.status = DraftStatus::Reviewed;
        report
            .set_evidence(vec![ReportEvidence {
                kind: "message".into(),
                source_id: "turn-1".into(),
                text: "selected text".into(),
            }])
            .unwrap();
        assert_eq!(report.status, DraftStatus::Draft);
        report.status = DraftStatus::Sending;
        assert!(report.set_evidence(vec![]).is_err());
    }
    #[test]
    fn export_contains_selected_projection_only() {
        let dir = tempfile::tempdir().unwrap();
        let mut report = BugReport::new("Failure").unwrap();
        report.context.model = "private-model".into();
        let path = report.export(dir.path()).unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(!text.contains("private-model"));
        assert!(!text.contains("destination"));
    }
}

#[cfg(test)]
mod receipt_parity_tests {
    use super::*;
    #[test]
    fn draft_survives_typed_execution_receipt_without_send_authority() {
        let result = draft_tool(
            serde_json::json!({"description":"Repeated failure", "expected_behavior":"Recover", "reproduction_steps":"Retry"}),
        );
        let execution = crate::agent::ToolExecution::from_legacy(
            "call-1",
            "draft_feedback",
            crate::agent::ExecutionSource::Native,
            result,
        );
        let encoded = serde_json::to_string(&execution).unwrap();
        let restored: crate::agent::ToolExecution = serde_json::from_str(&encoded).unwrap();
        let details = restored.to_legacy().details.unwrap();
        assert_eq!(details["feedback_draft"]["description"], "Repeated failure");
        assert_eq!(
            details["feedback_draft"]["context"]["reproduction_steps"],
            "Retry"
        );
        assert!(details["feedback_draft"].get("destination").is_none());
        assert!(details["feedback_draft"].get("status").is_none());
    }
}
