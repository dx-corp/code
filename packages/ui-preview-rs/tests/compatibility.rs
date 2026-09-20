use maestro_ui_preview::{
    Scene,
    adapters::{AdapterManifest, StoryTemplate},
    schema::{
        CONTRACT_SCHEMA, CONTRACT_SCHEMA_VERSION, CoverageProfile, RECIPE_SCHEMA,
        RECIPE_SCHEMA_VERSION, REPLAY_SCHEMA, REPLAY_SCHEMA_VERSION, StoryAlias, StoryAliases,
        TerminalProfile, WireEnvelope, builtin_profiles, encode_contract, encode_recipe,
        encode_replay, migrate_contract, migrate_recipe, migrate_replay, migrate_theme_replay,
    },
};
use serde_json::{Value, json};
use std::collections::BTreeMap;

fn fixture(name: &str) -> Value {
    let source = match name {
        "recipe-v1.json" => include_str!("fixtures/recipe-v1.json"),
        "replay-v1.json" => include_str!("fixtures/replay-v1.json"),
        "contract-v1.json" => include_str!("fixtures/contract-v1.json"),
        _ => panic!("unknown fixture"),
    };
    serde_json::from_str(source).unwrap()
}

#[test]
fn compatibility_corpus_migrates_to_current_envelopes() {
    let recipe = migrate_recipe(fixture("recipe-v1.json")).unwrap();
    assert_eq!(recipe.schema, RECIPE_SCHEMA);
    assert_eq!(recipe.version, RECIPE_SCHEMA_VERSION);
    assert_eq!(recipe.value.id, "workspace-picker");
    let round_trip: WireEnvelope<Value> =
        serde_json::from_value(serde_json::to_value(&recipe).unwrap()).unwrap();
    assert_eq!(round_trip.schema, RECIPE_SCHEMA);

    let replay = migrate_replay(fixture("replay-v1.json")).unwrap();
    assert_eq!(
        (replay.schema.as_str(), replay.version),
        (REPLAY_SCHEMA, REPLAY_SCHEMA_VERSION)
    );
    assert_eq!(replay.value.inputs.len(), 1);

    let contract = migrate_contract(fixture("contract-v1.json")).unwrap();
    assert_eq!(
        (contract.schema.as_str(), contract.version),
        (CONTRACT_SCHEMA, CONTRACT_SCHEMA_VERSION)
    );
    contract.value.validate().unwrap();

    assert_eq!(
        serde_json::from_str::<Value>(&encode_recipe(recipe.value).unwrap()).unwrap()["schema"],
        RECIPE_SCHEMA
    );
    assert_eq!(
        serde_json::from_str::<Value>(&encode_replay(replay.value).unwrap()).unwrap()["schema"],
        REPLAY_SCHEMA
    );
    assert_eq!(
        serde_json::from_str::<Value>(&encode_contract(contract.value).unwrap()).unwrap()["schema"],
        CONTRACT_SCHEMA
    );
}

#[test]
fn unknown_versions_schemas_and_extra_envelope_fields_fail_closed() {
    assert_eq!(
        migrate_recipe(json!({"version": 99})).unwrap_err().code(),
        "unsupported-recipe-version"
    );
    assert_eq!(
        migrate_replay(json!({"schema": REPLAY_SCHEMA, "version": 99, "value": {}}))
            .unwrap_err()
            .code(),
        "unsupported-replay-version"
    );
    assert_eq!(
        migrate_contract(json!({"schema": "other", "version": 1, "value": {}}))
            .unwrap_err()
            .code(),
        "unexpected-schema"
    );
    assert_eq!(
        migrate_recipe(json!({"schema": RECIPE_SCHEMA, "version": 1, "value": {}, "extra": true}))
            .unwrap_err()
            .code(),
        "invalid-wire-envelope"
    );
}

#[test]
fn theme_replay_has_a_distinct_envelope_and_legacy_reader() {
    let raw = json!({"version": 1, "fixture": "ready", "width": 72, "height": 22, "inputs": []});
    let legacy = migrate_theme_replay::<Value>(raw.clone()).unwrap();
    assert_eq!(legacy.schema, "maestro.ui.theme-replay");
    assert_eq!(legacy.value, raw);
    let wrapped = migrate_theme_replay::<Value>(json!({
        "schema": "maestro.ui.theme-replay", "version": 1, "value": raw
    }))
    .unwrap();
    assert_eq!(wrapped.version, 1);
    assert_eq!(
        migrate_theme_replay::<Value>(json!({
            "schema": "maestro.ui.theme-replay", "version": 9, "value": {}
        }))
        .unwrap_err()
        .code(),
        "unsupported-theme-replay-version"
    );
}

#[test]
fn aliases_reject_ambiguity_cycles_missing_targets_and_adapter_crossings() {
    let stories = BTreeMap::from([
        ("workspace-picker".into(), "shared-menu".into()),
        ("theme-selector".into(), "theme-selector".into()),
    ]);
    let alias = |from: &str, to: &str, adapter: &str| StoryAlias {
        from: from.into(),
        to: to.into(),
        adapter: adapter.into(),
        remove_in_version: 2,
    };
    let graph = StoryAliases::new(
        [alias("old-picker", "workspace-picker", "shared-menu")],
        &stories,
    )
    .unwrap();
    assert_eq!(graph.resolve("old-picker").original, "old-picker");
    assert_eq!(graph.resolve("old-picker").canonical, "workspace-picker");
    assert!(graph.validate_for_version(1).is_ok());
    assert!(
        graph
            .validate_for_version(2)
            .unwrap_err()
            .contains("expired")
    );
    assert!(
        StoryAliases::new(
            [
                alias("old-picker", "workspace-picker", "shared-menu"),
                alias("old-picker", "workspace-picker", "shared-menu"),
            ],
            &stories
        )
        .unwrap_err()
        .contains("duplicate")
    );
    assert!(
        StoryAliases::new(
            [
                alias("old-a", "old-b", "shared-menu"),
                alias("old-b", "old-a", "shared-menu"),
            ],
            &stories
        )
        .unwrap_err()
        .contains("cycle")
    );
    assert!(
        StoryAliases::new([alias("old-picker", "missing", "shared-menu")], &stories)
            .unwrap_err()
            .contains("does not exist")
    );
    assert!(
        StoryAliases::new(
            [alias("old-picker", "theme-selector", "shared-menu")],
            &stories
        )
        .unwrap_err()
        .contains("crosses adapters")
    );
    let mut no_expiry = alias("old-picker", "workspace-picker", "shared-menu");
    no_expiry.remove_in_version = 0;
    assert!(
        StoryAliases::new([no_expiry], &stories)
            .unwrap_err()
            .contains("removal version")
    );
    assert!(
        StoryAliases::new(
            [alias("workspace-picker", "theme-selector", "shared-menu")],
            &stories
        )
        .unwrap_err()
        .contains("shadows canonical")
    );

    let mut registry = maestro_ui_preview::registry::Registry::default();
    registry
        .add(
            maestro_ui_preview::registry::Story::new(
                "workspace-picker",
                "Workspace picker",
                "fixture.rs",
                |_, _| {},
            )
            .adapter("shared-menu"),
        )
        .unwrap();
    registry
        .add_aliases([alias("old-picker", "workspace-picker", "shared-menu")])
        .unwrap();
    let capture = registry
        .filtered(maestro_ui_preview::StoryFilter::Story("old-picker".into()))
        .unwrap()
        .captures()
        .unwrap()
        .pop()
        .unwrap();
    let resolved = capture.resolved_story.unwrap();
    assert_eq!(resolved.original, "old-picker");
    assert_eq!(resolved.canonical, "workspace-picker");
}

#[test]
fn named_profiles_are_bounded_and_select_existing_cases_only() {
    for profile in builtin_profiles() {
        profile.validate().unwrap();
    }
    let profile = &builtin_profiles()[0];
    let manifest = AdapterManifest::active(
        "shared-menu",
        "maestro-ui",
        "fixtures",
        "registration.rs",
        StoryTemplate::MenuRecipe,
    );
    let scenes = vec![
        Scene {
            id: "menu".into(),
            label: "Menu".into(),
            width: 40,
            height: 20,
            time_ms: 0,
        },
        Scene {
            id: "menu".into(),
            label: "Menu".into(),
            width: 80,
            height: 30,
            time_ms: 0,
        },
    ];
    let selected = profile.select(&manifest, &scenes).unwrap();
    assert_eq!(selected.len(), 1);
    assert_eq!((selected[0].width, selected[0].height), (40, 20));

    let invalid = CoverageProfile {
        id: "bad".into(),
        version: 1,
        terminal: TerminalProfile {
            color: "truecolor".into(),
            unicode: true,
            motion: false,
        },
        geometries: vec![(40, 20), (40, 20)],
        purpose: "duplicate geometry".into(),
        owner: "maestro-ui".into(),
        runtime_budget_ms: 1,
    };
    assert!(invalid.validate().unwrap_err().contains("duplicate"));
}

#[test]
fn captures_carry_all_schema_and_renderer_versions_without_changing_cells() {
    let capture = maestro_ui_preview::review::capture(
        Scene {
            id: "fixture".into(),
            label: "Fixture".into(),
            width: 8,
            height: 3,
            time_ms: 0,
        },
        |_| {},
    )
    .unwrap();
    let json = serde_json::to_value(capture).unwrap();
    let metadata = &json["metadata"];
    for key in [
        "capture_version",
        "story_contract_version",
        "replay_version",
        "observation_version",
        "profile_version",
    ] {
        assert_eq!(metadata[key], 1, "missing {key}");
    }
    assert_eq!(metadata["renderer"]["id"], "maestro-ui-preview");
    assert_eq!(metadata["profile"]["id"], "unprofiled");
    assert_eq!(metadata["profile"]["version"], 1);
    assert!(
        json["cells"]
            .as_array()
            .is_some_and(|cells| !cells.is_empty())
    );
}
