//! Literal, bounded transcript search over the authorized native session owner.
use super::*;

const MAX_RESULTS: usize = 50;
const MAX_QUERY_BYTES: usize = 256;
const SNIPPET_CHARS: usize = 240;

pub(super) async fn response(
    state: &AppState,
    auth: &AuthContext,
    query: &HashMap<String, String>,
) -> Vec<u8> {
    let store = state.sessions.lock().await;
    match search(&store, auth, query) {
        Ok(value) => json_response(200, &value),
        Err((status, message)) => json_response(status, &serde_json::json!({"error":message})),
    }
}

fn search(
    store: &SessionStore,
    auth: &AuthContext,
    query: &HashMap<String, String>,
) -> Result<Value, (u16, &'static str)> {
    let needle = query
        .get("q")
        .map(String::as_str)
        .filter(|q| !q.trim().is_empty() && q.len() <= MAX_QUERY_BYTES)
        .ok_or((400, "Invalid search query"))?;
    let limit = query
        .get("limit")
        .map(|n| {
            n.parse::<usize>()
                .ok()
                .filter(|n| (1..=MAX_RESULTS).contains(n))
        })
        .unwrap_or(Some(20))
        .ok_or((400, "Invalid search limit"))?;
    let session_id = query.get("sessionId");
    let generation = query.get("sourceCreatedAt");
    if session_id.is_some() != generation.is_some() {
        return Err((400, "Search session and generation are required together"));
    }
    if let Some(id) = session_id {
        let session = store
            .sessions
            .get(id)
            .filter(|s| session_visible_to_auth(s, auth))
            .ok_or((404, "Session not found"))?;
        if Some(&session.created_at) != generation {
            return Err((409, "Session generation changed"));
        }
    }
    let mut sessions = store
        .sessions
        .values()
        .filter(|s| session_visible_to_auth(s, auth) && session_id.is_none_or(|id| id == &s.id))
        .collect::<Vec<_>>();
    sessions.sort_by(|a, b| b.updated_at.cmp(&a.updated_at).then(a.id.cmp(&b.id)));
    let mut matches = Vec::new();
    let folded = needle.to_ascii_lowercase();
    for session in sessions {
        for (index, message) in session.messages.iter().enumerate().rev() {
            let role = message
                .get("role")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if !matches!(role, "user" | "assistant")
                || message.get("streaming").and_then(Value::as_bool) == Some(true)
            {
                continue;
            }
            let text = transcript_text(message);
            let Some(position) = text.to_ascii_lowercase().find(&folded) else {
                continue;
            };
            if matches.len() == limit {
                return Ok(serde_json::json!({"matches":matches,"truncated":true}));
            }
            matches.push(serde_json::json!({"sessionId":session.id,"sourceCreatedAt":session.created_at,"title":session.title.chars().take(256).collect::<String>(),"updatedAt":session.updated_at,"messageIndex":index,"role":role,"snippet":snippet(&text,position)}));
        }
    }
    Ok(serde_json::json!({"matches":matches,"truncated":false}))
}

fn transcript_text(message: &Value) -> String {
    let content = message.get("content").or_else(|| message.get("text"));
    match content {
        Some(Value::String(text)) => text.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter_map(|part| {
                part.as_str()
                    .or_else(|| part.get("text").and_then(Value::as_str))
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn snippet(text: &str, byte_position: usize) -> String {
    let position = text[..byte_position].chars().count();
    let chars = text.chars().collect::<Vec<_>>();
    let start = position
        .saturating_sub(72)
        .min(chars.len().saturating_sub(SNIPPET_CHARS - 2));
    let end = (start + SNIPPET_CHARS - 2).min(chars.len());
    format!(
        "{}{}{}",
        if start > 0 { "…" } else { "" },
        chars[start..end].iter().collect::<String>(),
        if end < chars.len() { "…" } else { "" }
    )
}

#[cfg(test)]
mod session_search_tests {
    use super::*;
    fn fixture() -> (SessionStore, AuthContext) {
        let mut a = create_session_record(Some("Scoped".into()), Some("owner".into()));
        a.id = "a".into();
        a.organization_id = Some("org".into());
        a.workspace_id = Some("ws".into());
        a.messages = vec![
            serde_json::json!({"role":"user","content":"literal %_.* needle"}),
            serde_json::json!({"role":"assistant","content":[{"type":"text","text":"NEEDLE in blocks"}],"attachments":[{"content":"secret"}]}),
        ];
        let mut b = a.clone();
        b.id = "b".into();
        b.owner = Some("other".into());
        (
            SessionStore {
                sessions: HashMap::from([("a".into(), a), ("b".into(), b)]),
                ..SessionStore::default()
            },
            AuthContext {
                subject: Some("owner".into()),
                organization_id: Some("org".into()),
                workspace_id: Some("ws".into()),
                ..AuthContext::default()
            },
        )
    }
    #[test]
    fn session_search_literal_queries_scopes_and_public_text_only() {
        let (store, auth) = fixture();
        let run =
            |q: &str| search(&store, &auth, &HashMap::from([("q".into(), q.into())])).unwrap();
        assert_eq!(run("%_.*")["matches"].as_array().unwrap().len(), 1);
        assert_eq!(run("needle")["matches"].as_array().unwrap().len(), 2);
        assert_eq!(run("secret")["matches"], serde_json::json!([]));
        assert!(
            run("needle")["matches"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["sessionId"] == "a")
        );
        let mut wrong = auth;
        wrong.workspace_id = Some("other".into());
        assert_eq!(
            search(
                &store,
                &wrong,
                &HashMap::from([("q".into(), "needle".into())])
            )
            .unwrap()["matches"],
            serde_json::json!([])
        );
    }
    #[test]
    fn session_search_bounds_and_generation_fail_closed() {
        let (mut store, auth) = fixture();
        store.sessions.get_mut("a").unwrap().messages.push(serde_json::json!({"role":"user","content":format!("{}needle{}","😀".repeat(500),"界".repeat(500))}));
        let result = search(
            &store,
            &auth,
            &HashMap::from([("q".into(), "needle".into()), ("limit".into(), "1".into())]),
        )
        .unwrap();
        assert_eq!(result["truncated"], true);
        assert!(
            result["matches"][0]["snippet"]
                .as_str()
                .unwrap()
                .chars()
                .count()
                <= 240
        );
        assert!(
            result["matches"][0]["snippet"]
                .as_str()
                .unwrap()
                .contains("needle")
        );
        let query = HashMap::from([
            ("q".into(), "needle".into()),
            ("sessionId".into(), "a".into()),
            ("sourceCreatedAt".into(), "stale".into()),
        ]);
        assert_eq!(search(&store, &auth, &query).unwrap_err().0, 409);
        assert!(
            search(
                &store,
                &auth,
                &HashMap::from([("q".into(), "x".repeat(257))])
            )
            .is_err()
        );
    }
}
