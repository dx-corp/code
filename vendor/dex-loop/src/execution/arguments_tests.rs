use serde_json::{Map, Value, json};

use super::*;

const STATES: [&str; 7] = [
    "FRAMEWORK_COVERAGE_STATE_UNSPECIFIED",
    "FRAMEWORK_COVERAGE_STATE_VERIFIED",
    "FRAMEWORK_COVERAGE_STATE_FAILING",
    "FRAMEWORK_COVERAGE_STATE_UNKNOWN",
    "FRAMEWORK_COVERAGE_STATE_ATTESTED",
    "FRAMEWORK_COVERAGE_STATE_NOT_APPLICABLE",
    "FRAMEWORK_COVERAGE_STATE_EXCEPTED",
];

/// The shape `workspace_mcp::read_schema` gives a page read: proto field
/// names, closed objects, inlined enums.
fn schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "states": {"type": "array", "items": {
                "type": "string",
                "enum": STATES,
                "description": "Requirement roll-up state for one framework edition.\nMore prose."
            }},
            "control_ids": {"type": "array", "items": {"type": "string"},
                "description": "Controls whose tests to list."},
            "policies": {"type": "array", "items": {"type": "string"}},
            "limit": {"type": "integer"},
            "ratio": {"type": "number"},
            "since": {"type": "string", "format": "date-time"},
            "sla_state": {"type": "string", "enum": ["overdue", "due_soon", "within_sla", "no_sla"]},
            "query": {"type": "object", "additionalProperties": false,
                "properties": {"search": {"type": "string"}, "kinds": {"type": "array", "items": {"type": "string"}}}}
        }
    })
}

fn args(value: Value) -> Map<String, Value> {
    value.as_object().cloned().expect("object")
}

fn accepted(value: Value) -> Normalized {
    normalize(&schema(), &args(value.clone()))
        .unwrap_or_else(|refused| panic!("{value} refused: {refused}"))
}

fn refused(value: Value) -> ArgumentsInvalid {
    match normalize(&schema(), &args(value.clone())) {
        Ok(normalized) => panic!("{value} accepted as {:?}", normalized.args),
        Err(refused) => refused,
    }
}

/// The example a refusal carries, and any corrected call, pass the same
/// layer with nothing left to interpret.
fn round_trips(refusal: &ArgumentsInvalid) {
    let again = normalize(&schema(), &refusal.example)
        .unwrap_or_else(|error| panic!("example {:?} refused: {error}", refusal.example));
    assert_eq!(again.args, refusal.example);
    assert!(again.interpreted.is_empty(), "{:?}", again.interpreted);
    if let Some(corrected) = refusal.corrected() {
        let again = normalize(&schema(), &corrected.args).expect("corrected call accepted");
        assert!(again.interpreted.is_empty(), "{:?}", again.interpreted);
    }
}

#[test]
fn an_enum_value_in_another_case_or_without_its_prefix_is_that_value() {
    for sent in [
        "failing",
        "FAILING",
        "Failing",
        "framework_coverage_state_failing",
        "framework-coverage-state-failing",
        " failing ",
    ] {
        let normalized = accepted(json!({"states": [sent]}));
        assert_eq!(
            normalized.args["states"],
            json!(["FRAMEWORK_COVERAGE_STATE_FAILING"]),
            "{sent}"
        );
        assert_eq!(normalized.interpreted.len(), 1, "{sent}");
        let note = &normalized.interpreted[0];
        assert_eq!(note.rule, Rule::EnumName);
        assert_eq!(note.path, "states[0]");
        assert_eq!(
            note.to_string(),
            format!(
                "interpreted states[0] {} as \"FRAMEWORK_COVERAGE_STATE_FAILING\"",
                json!(sent)
            )
        );
    }
    let normalized =
        accepted(json!({"states": ["not applicable", "unknown"], "sla_state": "DUE-SOON"}));
    assert_eq!(
        normalized.args["states"],
        json!([
            "FRAMEWORK_COVERAGE_STATE_NOT_APPLICABLE",
            "FRAMEWORK_COVERAGE_STATE_UNKNOWN"
        ])
    );
    assert_eq!(normalized.args["sla_state"], json!("due_soon"));
}

#[test]
fn exact_arguments_pass_unchanged_with_nothing_interpreted() {
    let sent = json!({
        "states": ["FRAMEWORK_COVERAGE_STATE_FAILING"],
        "control_ids": ["ctl-1"],
        "limit": 25,
        "since": "2026-01-31T10:00:00Z",
        "sla_state": "overdue",
        "query": {"search": "snowflake"}
    });
    let normalized = accepted(sent.clone());
    assert_eq!(Value::Object(normalized.args), sent);
    assert!(normalized.interpreted.is_empty());
}

#[test]
fn a_field_in_another_case_or_number_is_that_field() {
    let normalized =
        accepted(json!({"controlIds": ["ctl-1"], "state": ["failing"], "policy": ["p-1"]}));
    assert_eq!(normalized.args["control_ids"], json!(["ctl-1"]));
    assert_eq!(
        normalized.args["states"],
        json!(["FRAMEWORK_COVERAGE_STATE_FAILING"])
    );
    assert_eq!(normalized.args["policies"], json!(["p-1"]));
    assert!(!normalized.args.contains_key("controlIds"));
    let notes: Vec<String> = normalized
        .interpreted
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        notes.contains(&"interpreted field \"controlIds\" as \"control_ids\"".to_owned()),
        "{notes:?}"
    );
    assert!(
        notes.contains(&"interpreted field \"state\" as \"states\"".to_owned()),
        "{notes:?}"
    );
    // Nested objects are checked the same way.
    let nested = accepted(json!({"query": {"kind": "bucket"}}));
    assert_eq!(nested.args["query"], json!({"kinds": ["bucket"]}));
}

#[test]
fn one_value_where_a_list_is_declared_is_a_list_of_one() {
    let normalized = accepted(json!({"states": "failing", "control_ids": "ctl-1"}));
    assert_eq!(
        normalized.args["states"],
        json!(["FRAMEWORK_COVERAGE_STATE_FAILING"])
    );
    assert_eq!(normalized.args["control_ids"], json!(["ctl-1"]));
    let rules: Vec<(String, Rule)> = normalized
        .interpreted
        .iter()
        .map(|note| (note.path.clone(), note.rule))
        .collect();
    assert!(
        rules.contains(&("states".into(), Rule::OneAsList)),
        "{rules:?}"
    );
    assert!(
        rules.contains(&("states[0]".into(), Rule::EnumName)),
        "{rules:?}"
    );
    assert!(
        rules.contains(&("control_ids".into(), Rule::OneAsList)),
        "{rules:?}"
    );
}

#[test]
fn a_number_sent_as_text_is_that_number() {
    let normalized = accepted(json!({"limit": " 25 ", "ratio": "0.5"}));
    assert_eq!(normalized.args["limit"], json!(25));
    assert_eq!(normalized.args["ratio"], json!(0.5));
    assert!(
        normalized
            .interpreted
            .iter()
            .all(|note| note.rule == Rule::NumberText)
    );
    let refusal = refused(json!({"limit": "twenty"}));
    assert!(matches!(&refusal.problems[..], [Problem::WrongType { path, .. }] if path == "limit"));
    assert_eq!(refusal.to_string(), "limit takes an integer, not a string.");
    assert!(refusal.corrected().is_none());
    round_trips(&refusal);
}

#[test]
fn an_iso_date_is_rewritten_in_rfc_3339_form() {
    for (sent, used) in [
        ("2026-01-31", "2026-01-31T00:00:00Z"),
        ("2026-01-31 10:30", "2026-01-31T10:30:00Z"),
        ("2026-01-31t10:30:15z", "2026-01-31T10:30:15Z"),
        ("2026-01-31T10:30:15.250", "2026-01-31T10:30:15.250Z"),
        ("2026-01-31T10:30:15+0200", "2026-01-31T10:30:15+02:00"),
        ("2026-01-31T10:30:15-05", "2026-01-31T10:30:15-05:00"),
        ("2024-02-29", "2024-02-29T00:00:00Z"),
    ] {
        let normalized = accepted(json!({"since": sent}));
        assert_eq!(normalized.args["since"], json!(used), "{sent}");
        assert_eq!(normalized.interpreted[0].rule, Rule::DateTime, "{sent}");
    }
    for invalid in [
        "2026-02-30",
        "2025-02-29",
        "2026-13-01",
        "31/01/2026",
        "2026-01-31T24:00",
        "2026-01-31T10:30+2:00",
        "yesterday",
    ] {
        let refusal = refused(json!({"since": invalid}));
        assert!(
            matches!(&refusal.problems[..], [Problem::WrongType { path, .. }] if path == "since"),
            "{invalid}: {refusal}"
        );
        round_trips(&refusal);
    }
}

/// Replays the call production refused (`ListControlTests` `states` named
/// after the control-test enum): the one value it can mean is a correction
/// to retry once, never an obvious variant applied silently.
#[test]
fn a_value_of_another_enum_of_the_same_kind_is_one_correction_to_retry() {
    let refusal = refused(json!({"states": ["CONTROL_TEST_STATE_FAILING"], "limit": "50"}));
    assert_eq!(
        refusal.to_string(),
        format!(
            "states[0] does not accept \"CONTROL_TEST_STATE_FAILING\". Did you mean FRAMEWORK_COVERAGE_STATE_FAILING? \
             It takes only these exact values: {}. (Requirement roll-up state for one framework edition.)",
            STATES[1..].join(", ")
        )
    );
    let corrected = refusal.corrected().expect("one candidate is a correction");
    assert_eq!(
        corrected.args["states"],
        json!(["FRAMEWORK_COVERAGE_STATE_FAILING"])
    );
    assert_eq!(corrected.args["limit"], json!(50));
    let notes: Vec<String> = corrected
        .interpreted
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        notes,
        [
            "interpreted limit \"50\" as 50",
            "corrected states[0] \"CONTROL_TEST_STATE_FAILING\" to \"FRAMEWORK_COVERAGE_STATE_FAILING\"",
        ]
    );
    round_trips(&refusal);
    // A near miss by one letter is a correction too.
    let typo = refused(json!({"states": ["FAILNG"]}));
    assert_eq!(
        typo.corrected().map(|call| call.args["states"].clone()),
        Some(json!(["FRAMEWORK_COVERAGE_STATE_FAILING"]))
    );
}

#[test]
fn an_ambiguous_or_unknown_value_is_refused_with_the_values_not_guessed() {
    let pets = json!({"type": "object", "additionalProperties": false, "properties": {
        "pet": {"type": "string", "enum": ["PET_CAT", "PET_CAR", "PET_DOG"]}}});
    let refusal = normalize(&pets, &args(json!({"pet": "cab"}))).expect_err("ambiguous");
    assert!(
        matches!(&refusal.problems[..], [Problem::UnknownValue { candidates, .. }]
            if candidates == &["PET_CAT".to_owned(), "PET_CAR".to_owned()]),
        "{refusal:?}"
    );
    assert!(refusal.corrected().is_none(), "ambiguity is never retried");
    assert!(
        refusal
            .to_string()
            .contains("It could mean PET_CAT or PET_CAR; name the one you mean."),
        "{refusal}"
    );
    let again = normalize(&pets, &refusal.example).expect("example accepted");
    assert!(again.interpreted.is_empty());

    // "passing" is not one edit from FAILING: it is listed, never guessed.
    for sent in ["passing", "NOT_FAILING", "green"] {
        let refusal = refused(json!({"states": [sent]}));
        assert!(
            matches!(&refusal.problems[..], [Problem::UnknownValue { candidates, accepted, .. }]
                if candidates.is_empty() && accepted.len() == STATES.len() - 1),
            "{sent}: {refusal:?}"
        );
        assert!(refusal.corrected().is_none(), "{sent}");
        assert!(
            refusal
                .to_string()
                .contains("FRAMEWORK_COVERAGE_STATE_VERIFIED"),
            "{sent}"
        );
        assert!(!refusal.to_string().contains("UNSPECIFIED"), "{sent}");
        round_trips(&refusal);
    }
}

#[test]
fn an_unknown_field_is_refused_with_the_nearest_field_never_dropped() {
    let refusal = refused(json!({"contol_ids": ["ctl-1"], "limit": 10}));
    assert_eq!(
        refusal.to_string(),
        "contol_ids is not a field here. Did you mean control_ids (Controls whose tests to list.)?"
    );
    let corrected = refusal.corrected().expect("one nearest field");
    assert_eq!(
        corrected.args,
        args(json!({"control_ids": ["ctl-1"], "limit": 10}))
    );
    assert_eq!(
        corrected
            .interpreted
            .last()
            .map(ToString::to_string)
            .as_deref(),
        Some("corrected field \"contol_ids\" to \"control_ids\"")
    );
    round_trips(&refusal);

    // No near field: refused naming every field; the example leaves it out
    // and says so through the refusal, and nothing is retried.
    let refusal = refused(json!({"owner": "me", "limit": 10}));
    assert!(
        matches!(&refusal.problems[..], [Problem::UnknownField { sent, candidates, fields, .. }]
            if sent == "owner" && candidates.is_empty() && fields.contains(&"control_ids".to_owned())),
        "{refusal:?}"
    );
    assert!(
        refusal
            .to_string()
            .starts_with("owner is not a field here. Its fields are: ")
    );
    assert!(refusal.corrected().is_none());
    assert!(!refusal.example.contains_key("owner"));
    round_trips(&refusal);

    // Two names for one field: which is meant is the caller's choice.
    let refusal = refused(json!({"state": ["failing"], "states": ["verified"]}));
    assert!(refusal.corrected().is_none(), "{refusal}");
}

#[test]
fn an_open_object_keeps_fields_it_does_not_declare() {
    let open = json!({"type": "object", "properties": {"limit": {"type": "integer"}}});
    let normalized =
        normalize(&open, &args(json!({"limit": "5", "extra": true}))).expect("accepted");
    assert_eq!(normalized.args, args(json!({"limit": 5, "extra": true})));
}

/// Replays the call production refused as "tenant fields cannot be set":
/// `ListAssets` `query` sent as a string.
#[test]
fn a_scalar_where_an_object_is_declared_names_the_shape() {
    let refusal = refused(json!({"query": "snowflake"}));
    assert_eq!(
        refusal.to_string(),
        "query takes an object with these fields: kinds, search, not a string."
    );
    assert!(
        refusal.corrected().is_none(),
        "which field the text belongs in is not guessed"
    );
    round_trips(&refusal);
}

#[test]
fn edit_distance_counts_character_edits() {
    assert_eq!(levenshtein("", ""), 0);
    assert_eq!(levenshtein("abc", ""), 3);
    assert_eq!(levenshtein("kitten", "sitting"), 3);
    assert_eq!(levenshtein("FAILNG", "FAILING"), 1);
    assert_eq!(levenshtein("PASSING", "FAILING"), 3);
}
