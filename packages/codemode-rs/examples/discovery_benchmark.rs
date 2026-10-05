//! Run with `cargo run -p agent-codemode --example discovery_benchmark`.
//! Measures discovery separately from fixture generation. No timing CI gates.
use agent_codemode::{Catalog, Event, Session, Store, Tool};
use serde_json::{Value, json};
use std::hint::black_box;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

fn fixture(count: usize) -> Vec<Tool> {
    let mut tools: Vec<_> = (0..count - 3).map(|i| Tool {
        name: format!("inventory.item_{i:05}"),
        description: "Look up stock quantity and warehouse location".into(),
        namespace: Some("inventory".into()),
        schema: json!({"type":"object","properties":{"itemId":{"type":"string"},"notes":{"type":"string","description":"x".repeat(8192)}}}),
        ..Default::default()
    }).collect();
    for (name, description, properties) in [
        (
            "documents.export_pdf",
            "Export a document as PDF",
            json!({"documentId":{"type":"string"}}),
        ),
        (
            "documents.comments",
            "Read document comments and archived remarks",
            json!({"continuationToken":{"type":"string"}}),
        ),
        (
            "documents.export_text",
            "Export a document as plain text",
            json!({"documentId":{"type":"string"}}),
        ),
    ] {
        tools.push(Tool {
            name: name.into(),
            description: description.into(),
            namespace: Some("documents".into()),
            schema: json!({"type":"object","properties":properties}),
            ..Default::default()
        });
    }
    tools
}

async fn script(catalog: Catalog) {
    let mut session = Session::start_with_catalog(
        "text(searchTools('export PDF',{namespace:'documents',limit:1})[0].name);".into(),
        catalog,
        &CancellationToken::new(),
        Duration::from_secs(10),
        Store::new(),
    );
    let Some(Event::Done(report)) = session.next().await else {
        panic!("unexpected dispatch")
    };
    assert!(report.error.is_none(), "{:?}", report.error);
    assert_eq!(report.output, vec!["documents_export_pdf"]);
}

async fn measure(count: usize) -> Value {
    let tools = fixture(count);
    let schema_source_bytes = tools
        .iter()
        .map(|t| serde_json::to_vec(&t.schema).unwrap().len())
        .sum::<usize>();
    let start = Instant::now();
    let catalog = Catalog::new(tools);
    let construct_us = start.elapsed().as_micros();
    let queries = [
        ("export PDF", "documents.export_pdf"),
        ("continuationToken", "documents.comments"),
        ("archived remarks", "documents.comments"),
    ];
    for (query, want) in queries {
        assert_eq!(catalog.search(query, &[], 1, None)[0].name, want);
    }
    let start = Instant::now();
    for _ in 0..100 {
        for (query, _) in queries {
            black_box(catalog.search(query, &[], 8, None));
        }
    }
    let warm_search_us = start.elapsed().as_micros() as f64 / 300.0;
    let start = Instant::now();
    let first = catalog
        .schema_page("inventory.item_00000", &json!({"maxBytes":4096}))
        .unwrap();
    let schema_cold_us = start.elapsed().as_micros();
    let start = Instant::now();
    for _ in 0..300 {
        black_box(
            catalog
                .schema_page(
                    "inventory.item_00000",
                    &json!({"offsetBytes":first["nextOffsetBytes"],"maxBytes":4096}),
                )
                .unwrap(),
        );
    }
    let schema_warm_us = start.elapsed().as_micros() as f64 / 300.0;
    // The prior ownership pattern copied schemas and rebuilt search text on
    // every script. This baseline isolates that cost under the current ranking.
    let start = Instant::now();
    for _ in 0..5 {
        black_box(Catalog::new(catalog.iter().cloned().collect()).search(
            "export PDF",
            &[],
            8,
            None,
        ));
    }
    let rebuilt_search_us = start.elapsed().as_micros() as f64 / 5.0;
    let start = Instant::now();
    for _ in 0..5 {
        script(catalog.clone()).await;
    }
    let shared_script_us = start.elapsed().as_micros() as f64 / 5.0;
    json!({"tools":count,"schema_source_bytes":schema_source_bytes,"catalog_construct_us":construct_us,
        "warm_search_us":warm_search_us,"rebuild_and_search_us":rebuilt_search_us,
        "schema_cold_page_us":schema_cold_us,"schema_warm_page_us":schema_warm_us,
        "cached_schema_allocation_bytes":catalog.cached_schema_bytes(),"shared_script_us":shared_script_us,
        "discovery_queries_correct":3,"script_heap_limit_bytes":32*1024*1024,
        "discovery_description_bytes":agent_codemode::DESCRIPTION.len()})
}

fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let results = runtime.block_on(async {
        let mut rows = Vec::new();
        for count in [100, 1_000, 10_000] {
            rows.push(measure(count).await);
        }
        rows
    });
    println!("{}",serde_json::to_string_pretty(&json!({"profile":"debug","timing_thresholds":false,
        "memory_note":"schema_source_bytes is serialized source size; retained cache bytes count string capacities. Whole-process peak RSS is measured separately.","results":results})).unwrap());
}
