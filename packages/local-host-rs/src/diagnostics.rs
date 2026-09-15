//! Opt-in projections of explicitly supplied compiler observations.
//! Files remain owned by the caller; no session cache or execution authority.

use std::collections::BTreeMap;
use std::io::Read;

use anyhow::{Context, Result, bail, ensure};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const MAX_BYTES: usize = 4 * 1024 * 1024;
const HELP: &str = "Usage: maestro diagnostics <current.jsonl> [--previous <previous.jsonl> --previous-sha256 <digest>]\n\nProjects rustc/Cargo JSON diagnostics. Output is a lossy view; retain original files.\nThe digest binds a delta to the exact prior observation. It does not prove build success.";

fn digest(raw: &str) -> String {
    format!("{:x}", Sha256::digest(raw.as_bytes()))
}

fn normalize(diagnostic: &Value) -> Result<Value> {
    ensure!(
        diagnostic["level"].is_string() && diagnostic["message"].is_string(),
        "invalid diagnostic level or message"
    );
    let spans = diagnostic["spans"]
        .as_array()
        .context("invalid diagnostic spans")?;
    let mut locations = Vec::new();
    for span in spans {
        ensure!(
            span["file_name"].is_string() && span["is_primary"].is_boolean(),
            "invalid diagnostic location"
        );
        for field in ["line_start", "line_end", "column_start", "column_end"] {
            ensure!(span[field].is_u64(), "invalid diagnostic {field}");
        }
        let mut location = serde_json::Map::new();
        for field in [
            "file_name",
            "line_start",
            "line_end",
            "column_start",
            "column_end",
            "is_primary",
            "label",
            "suggested_replacement",
            "suggestion_applicability",
        ] {
            if let Some(value) = span.get(field) {
                location.insert(field.to_owned(), value.clone());
            }
        }
        locations.push(Value::Object(location));
    }
    let children = diagnostic["children"]
        .as_array()
        .context("invalid diagnostic children")?
        .iter()
        .map(normalize)
        .collect::<Result<Vec<_>>>()?;
    let code = match &diagnostic["code"] {
        Value::Null => Value::Null,
        value if value["code"].is_string() => value["code"].clone(),
        _ => bail!("invalid diagnostic code"),
    };
    Ok(
        json!({"level": diagnostic["level"], "code":code, "message":diagnostic["message"], "locations":locations, "children":children}),
    )
}

fn parse(raw: &str) -> Result<(Vec<Value>, Option<bool>)> {
    ensure!(raw.len() <= MAX_BYTES, "observation exceeds 4 MiB");
    let mut diagnostics = Vec::new();
    let mut completion = None;
    let mut records = 0;
    for line in raw.lines().filter(|line| !line.trim().is_empty()) {
        let value: Value = serde_json::from_str(line).context("invalid compiler JSONL")?;
        records += 1;
        match value["reason"].as_str() {
            Some("compiler-message") => {
                let mut diagnostic = normalize(&value["message"])?;
                diagnostic["package_id"] = value["package_id"].clone();
                diagnostic["target"] = value["target"].clone();
                diagnostics.push(diagnostic);
            }
            Some("compiler-artifact" | "build-script-executed") => {}
            Some("build-finished") => {
                let success = value["success"]
                    .as_bool()
                    .context("invalid build completion")?;
                ensure!(
                    completion.is_none_or(|previous| previous == success),
                    "conflicting build completion"
                );
                completion = Some(success);
            }
            None if value["$message_type"] == "diagnostic" => diagnostics.push(normalize(&value)?),
            _ => bail!("unsupported compiler record"),
        }
    }
    ensure!(records > 0, "empty compiler observation");
    Ok((diagnostics, completion))
}

fn groups(diagnostics: &[Value]) -> Vec<Value> {
    let mut grouped: BTreeMap<String, Value> = BTreeMap::new();
    for diagnostic in diagnostics {
        let mut identity = diagnostic.clone();
        identity
            .as_object_mut()
            .expect("normalized object")
            .remove("locations");
        let group = grouped.entry(identity.to_string()).or_insert_with(|| {
            identity["count"] = json!(0);
            identity["locations"] = json!([]);
            identity
        });
        group["count"] = json!(group["count"].as_u64().expect("count") + 1);
        group["locations"]
            .as_array_mut()
            .expect("locations")
            .extend(
                diagnostic["locations"]
                    .as_array()
                    .expect("locations")
                    .iter()
                    .cloned(),
            );
    }
    grouped.into_values().collect()
}

/// Project compiler observations, requiring the exact prior digest for deltas.
/// Output omits rendered excerpts, expansion metadata, and reference essays;
/// callers must keep the original observations available for recovery.
pub fn project(current: &str, previous: Option<(&str, &str)>) -> Result<Value> {
    let (now, completion) = parse(current)?;
    let before = if let Some((raw, expected)) = previous {
        ensure!(digest(raw) == expected, "baseline digest mismatch");
        parse(raw)?.0
    } else {
        Vec::new()
    };
    let mut remaining: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for value in before {
        remaining.entry(value.to_string()).or_default().push(value);
    }
    let mut added = Vec::new();
    let mut unchanged = 0;
    let mut code_counts: BTreeMap<String, usize> = BTreeMap::new();
    for value in &now {
        if let Some(code) = value["code"].as_str() {
            *code_counts.entry(code.to_owned()).or_default() += 1;
        }
        if remaining
            .get_mut(&value.to_string())
            .and_then(Vec::pop)
            .is_some()
        {
            unchanged += 1;
        } else {
            added.push(value.clone());
        }
    }
    let removed: Vec<Value> = remaining.into_values().flatten().collect();
    Ok(json!({"format":"maestro-diagnostics-v1", "lossy":true,
        "current_sha256":digest(current), "previous_sha256":previous.map(|(_, hash)| hash),
        "build_succeeded":completion, "observed_error_count":now.iter().filter(|d| d["level"] == "error").count(),
        "current_code_counts":code_counts, "unchanged_diagnostics":unchanged,
        "added":groups(&added), "removed":groups(&removed)}))
}

fn read_observation(path: &str) -> Result<String> {
    let file = std::fs::File::open(path).with_context(|| format!("read {path}"))?;
    ensure!(
        file.metadata()?.is_file(),
        "observation must be a regular file"
    );
    let mut raw = String::new();
    file.take((MAX_BYTES + 1) as u64).read_to_string(&mut raw)?;
    ensure!(raw.len() <= MAX_BYTES, "observation exceeds 4 MiB");
    Ok(raw)
}

/// Run the explicit-file diagnostics utility without starting an agent.
pub fn run_cli(args: &[String]) -> Result<i32> {
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        println!("{HELP}");
        return Ok(0);
    }
    let (current_path, prior) = match args {
        [current] if !current.starts_with('-') => (current, None),
        [current, previous_flag, previous_path, hash_flag, hash]
            if previous_flag == "--previous" && hash_flag == "--previous-sha256" =>
        {
            (current, Some((previous_path, hash)))
        }
        _ => bail!("{HELP}"),
    };
    let current = read_observation(current_path)?;
    let previous = prior
        .map(|(path, hash)| read_observation(path).map(|raw| (raw, hash)))
        .transpose()?;
    let view = project(
        &current,
        previous
            .as_ref()
            .map(|(raw, hash)| (raw.as_str(), hash.as_str())),
    )?;
    println!("{}", serde_json::to_string(&view)?);
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use sha2::{Digest, Sha256};

    fn diagnostic(line: u64, byte: u64) -> Value {
        json!({"$message_type":"diagnostic","level":"error", "code":{"code":"E0308","explanation":"long reference essay"},
            "message":"mismatched types", "spans":[{"file_name":"src/lib.rs","line_start":line,"line_end":line,
                "column_start":3,"column_end":9,"is_primary":true,"label":"expected u32","byte_start":byte}],
            "children":[],"rendered":"full source excerpt"})
    }

    fn stream(records: &[Value]) -> String {
        records
            .iter()
            .map(Value::to_string)
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn hash(text: &str) -> String {
        format!("{:x}", Sha256::digest(text.as_bytes()))
    }

    #[test]
    fn groups_keep_multiplicity_locations_and_omit_reference_essays() {
        let raw = stream(&[diagnostic(7, 1), diagnostic(9, 8)]);
        let view = project(&raw, None).unwrap();
        assert_eq!(view["observed_error_count"], 2);
        assert_eq!(view["added"][0]["count"], 2);
        assert_eq!(view["added"][0]["locations"][1]["line_start"], 9);
        assert!(!view.to_string().contains("long reference essay"));
        assert!(view["build_succeeded"].is_null());
        assert_eq!(view["current_sha256"], hash(&raw));
    }

    #[test]
    fn delta_ignores_byte_offset_movement_but_retains_changed_labels() {
        let before = stream(&[diagnostic(7, 1), diagnostic(9, 8)]);
        let mut changed = diagnostic(12, 22);
        changed["spans"][0]["label"] = json!("expected bool");
        let after = stream(&[diagnostic(7, 90), changed]);
        let view = project(&after, Some((&before, &hash(&before)))).unwrap();
        assert_eq!(view["unchanged_diagnostics"], 1);
        assert_eq!(view["removed"][0]["locations"][0]["line_start"], 9);
        assert_eq!(view["added"][0]["locations"][0]["label"], "expected bool");
    }

    #[test]
    fn identical_locations_in_different_cargo_packages_are_not_unchanged() {
        let before = stream(&[
            json!({"reason":"compiler-message", "package_id":"first", "target":{"name":"first"}, "message":diagnostic(7, 1)}),
        ]);
        let after = stream(&[
            json!({"reason":"compiler-message", "package_id":"second", "target":{"name":"second"}, "message":diagnostic(7, 1)}),
        ]);
        let view = project(&after, Some((&before, &hash(&before)))).unwrap();
        assert_eq!(view["unchanged_diagnostics"], 0);
        assert_eq!(view["added"][0]["package_id"], "second");
        assert_eq!(view["removed"][0]["package_id"], "first");
    }

    #[test]
    fn wrong_baseline_fails_instead_of_comparing_different_history() {
        let before = stream(&[diagnostic(7, 1)]);
        let error = project(&before, Some((&before, &"0".repeat(64)))).unwrap_err();
        assert!(error.to_string().contains("baseline digest"));
    }

    #[test]
    fn cargo_completion_can_prove_zero_diagnostics_without_fabricating_success() {
        let raw = stream(&[
            json!({"reason":"compiler-artifact"}),
            json!({"reason":"build-finished","success":true}),
        ]);
        let view = project(&raw, None).unwrap();
        assert_eq!(view["observed_error_count"], 0);
        assert_eq!(view["build_succeeded"], true);
        assert!(project("", None).is_err());
        assert!(project("build passed!", None).is_err());
        assert!(project("{\"unexpected\":true}", None).is_err());
    }

    #[test]
    fn cargo_wrapped_diagnostics_and_failure_are_preserved() {
        let raw = stream(&[
            json!({"reason":"compiler-message","message":diagnostic(7, 1)}),
            json!({"reason":"build-finished","success":false}),
        ]);
        let view = project(&raw, None).unwrap();
        assert_eq!(view["observed_error_count"], 1);
        assert_eq!(view["build_succeeded"], false);
    }

    #[test]
    fn conflicting_build_results_and_oversized_input_fail_closed() {
        let raw = stream(&[
            json!({"reason":"build-finished","success":true}),
            json!({"reason":"build-finished","success":false}),
        ]);
        assert!(project(&raw, None).is_err());
        assert!(project(&" ".repeat(4 * 1024 * 1024 + 1), None).is_err());
    }

    #[test]
    fn diagnostics_without_spans_still_keep_their_count() {
        let mut d = diagnostic(7, 1);
        d["spans"] = json!([]);
        let raw = stream(&[d.clone(), d]);
        let view = project(&raw, None).unwrap();
        assert_eq!(view["added"][0]["count"], 2);
        let same = project(&raw, Some((&raw, &hash(&raw)))).unwrap();
        assert_eq!(same["unchanged_diagnostics"], 2);
        assert_eq!(same["added"], json!([]));
    }
}
