/// Normalize a slash-completion string to a single leading `/`.
///
/// `get_completions` returns values like `/help`. Older call sites (and any
/// bare name without a slash) must still resolve to exactly one leading slash
/// so the input never becomes `//help`.
#[must_use]
pub(crate) fn normalize_slash_completion(cmd: &str) -> String {
    let trimmed = cmd.trim();
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '/') {
        return "/".to_string();
    }
    let name = trimmed.trim_start_matches('/');
    format!("/{name}")
}
