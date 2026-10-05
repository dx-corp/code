use super::*;
use crate::{Event, Session, Store};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

fn fixture(name: &str, description: &str, schema: Value) -> Tool {
    Tool {
        name: name.into(),
        description: description.into(),
        schema,
        namespace: Some("documents".into()),
        ..Default::default()
    }
}

#[tokio::test]
async fn natural_language_and_parameter_queries_match_host_and_script_discovery() {
    let tools = vec![
        fixture(
            "docs.export_pdf",
            "Export a document as a PDF file",
            json!({"properties":{"documentId":{"type":"string"}}}),
        ),
        fixture(
            "docs.review",
            "Review document comments",
            json!({"properties":{"continuationToken":{"description":"Page through archived remarks"}}}),
        ),
        fixture("mail.send", "Send an email message", json!({})),
        fixture("docs.archive", "Archive an old document", json!({})),
        fixture(
            "docs.export_text",
            "Export a document as plain text",
            json!({}),
        ),
    ];
    let catalog = Catalog::new(tools);
    for (query, wanted) in [
        ("export PDF", vec!["docs.export_pdf", "docs.export_text"]),
        ("continuationToken", vec!["docs.review"]),
        ("archived remarks", vec!["docs.review"]),
        ("send email", vec!["mail.send"]),
        ("export-text", vec!["docs.export_text", "docs.export_pdf"]),
    ] {
        let names: Vec<_> = catalog
            .search(query, &[], 2, Some("documents"))
            .into_iter()
            .map(|t| t.name.as_str())
            .collect();
        assert_eq!(names, wanted, "{query}");
        let code = format!(
            "text(searchTools({},{{limit:2,namespace:'documents'}}).map(t=>t.name));",
            json!(query)
        );
        let mut session = Session::start_with_catalog(
            code,
            catalog.clone(),
            &CancellationToken::new(),
            Duration::from_secs(5),
            Store::new(),
        );
        let Some(Event::Done(report)) = session.next().await else {
            panic!("unexpected dispatch")
        };
        assert!(report.error.is_none(), "{:?}", report.error);
        assert_eq!(
            serde_json::from_str::<Vec<String>>(&report.output[0]).unwrap(),
            wanted
                .iter()
                .map(|s| s.replace('.', "_"))
                .collect::<Vec<_>>()
        );
    }
    assert!(catalog.search("export", &[], 8, Some("unknown")).is_empty());
    assert_eq!(
        catalog.search("export", &["docs.archive".into()], 1, None)[0].name,
        "docs.archive"
    );
}

#[test]
fn schema_eviction_and_new_snapshots_preserve_exact_pages_and_revisions() {
    let catalog = Catalog::new(
        (0..70)
            .map(|i| {
                fixture(
                    &format!("tool_{i}"),
                    "",
                    json!({"description":"界".repeat(40_000),"const":i}),
                )
            })
            .collect(),
    );
    let first = catalog
        .schema_page("tool_0", &json!({"maxBytes":4096}))
        .unwrap();
    let shared = catalog.clone();
    assert!(std::ptr::eq(
        catalog.lookup("tool_0").unwrap(),
        shared.lookup("tool_0").unwrap()
    ));
    for i in 1..70 {
        catalog
            .schema_page(&format!("tool_{i}"), &json!({}))
            .unwrap();
        assert!(catalog.cached_schema_bytes() <= 4 * 1024 * 1024);
    }
    assert_eq!(
        shared
            .schema_page("tool_0", &json!({"maxBytes":4096}))
            .unwrap(),
        first
    );
    let mut encoded = String::new();
    let mut offset = 0;
    loop {
        let page = catalog
            .schema_page("tool_0", &json!({"offsetBytes":offset,"maxBytes":4096}))
            .unwrap();
        assert_eq!(page["revision"], first["revision"]);
        encoded.push_str(page["json"].as_str().unwrap());
        if page["complete"] == true {
            break;
        }
        offset = page["nextOffsetBytes"].as_u64().unwrap();
    }
    let reconstructed: Value = serde_json::from_str(&encoded).unwrap();
    assert_eq!(
        reconstructed["inputSchema"]["description"]
            .as_str()
            .unwrap(),
        "界".repeat(40_000)
    );
    assert_eq!(reconstructed["inputSchema"]["const"], 0);
    let replacement = Catalog::new(vec![fixture("tool_0", "", json!({"const":"replacement"}))]);
    assert_ne!(
        replacement.schema_page("tool_0", &json!({})).unwrap()["revision"],
        first["revision"]
    );
    assert_eq!(
        catalog
            .schema_page("tool_0", &json!({"maxBytes":4096}))
            .unwrap(),
        first
    );
    let hidden = catalog.filtered(|tool| tool.name != "tool_0");
    assert!(hidden.lookup("tool_0").is_none());
    assert_eq!(
        hidden.schema_page("tool_0", &json!({})).unwrap(),
        Value::Null
    );
    assert!(hidden.search("", &["tool_0".into()], 8, None).is_empty());
}

#[test]
fn inspection_pages_reject_stale_snapshots_and_bound_escaped_guidance() {
    let mut tool = fixture("docs.review", "", json!({}));
    tool.namespace_instructions = Some("\u{0001}".repeat(4096));
    let catalog = Catalog::new(vec![tool.clone()]);
    let page = catalog.namespace_page("documents", &json!({})).unwrap();
    assert!(serde_json::to_vec(&page).unwrap().len() <= 16_384);
    assert_eq!(page["instructionsTrust"], "untrusted");
    let changed = Catalog::new(vec![tool]);
    assert!(
        changed
            .namespace_page(
                "documents",
                &json!({"snapshot":page["snapshot"],"offset":0})
            )
            .unwrap_err()
            .contains("snapshot changed")
    );
    assert_eq!(
        catalog
            .namespace_page(
                "documents",
                &json!({"snapshot":page["snapshot"],"offset":1})
            )
            .unwrap()["tools"],
        json!([])
    );
}

#[test]
fn schema_larger_than_cache_stays_pageable_without_retaining_it() {
    let catalog = Catalog::new(vec![fixture(
        "huge",
        "",
        json!({"description":"z".repeat(4*1024*1024)}),
    )]);
    let first = catalog
        .schema_page("huge", &json!({"maxBytes":16}))
        .unwrap();
    let second = catalog
        .schema_page(
            "huge",
            &json!({"offsetBytes":first["nextOffsetBytes"],"maxBytes":16}),
        )
        .unwrap();
    assert_eq!(first["revision"], second["revision"]);
    assert_eq!(first["complete"], false);
    assert_eq!(catalog.cached_schema_bytes(), 0);
}

#[test]
fn oversized_namespace_with_guidance_fails_without_a_nonprogressing_trim_loop() {
    for guidance in [None, Some("owner guidance".into())] {
        let mut tool = fixture("tool", "", json!({}));
        tool.namespace = Some("\u{0001}".repeat(4096));
        tool.namespace_instructions = guidance;
        let namespace = tool.namespace.clone().unwrap();
        let catalog = Catalog::new(vec![tool]);
        assert!(
            catalog
                .namespace_page(&namespace, &json!({}))
                .unwrap_err()
                .contains("budget")
        );
    }
}

#[test]
fn inspection_tokens_are_unique_across_processes() {
    const CHILD: &str = "MAESTRO_CATALOG_TOKEN_TEST_CHILD";
    if let Ok(seed) = std::env::var(CHILD) {
        let catalog = Catalog::new(vec![fixture(&format!("fixture_{seed}"), "", json!({}))]);
        let page = catalog.namespace_page("documents", &json!({})).unwrap();
        println!("CATALOG_TOKEN={}", page["snapshot"].as_str().unwrap());
        return;
    }
    let token = |seed: &str| {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "catalog::tests::inspection_tokens_are_unique_across_processes",
                "--nocapture",
            ])
            .env(CHILD, seed)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .find_map(|line| {
                line.split_once("CATALOG_TOKEN=")
                    .map(|(_, token)| token.to_owned())
            })
            .unwrap()
    };
    // Both children construct their first snapshot, with different contents.
    assert_ne!(token("first"), token("replacement"));
}
