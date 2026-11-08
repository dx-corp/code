//! Declared wrappers route to catalog tools by their arguments.

use super::*;
use dex_loop::ToolSpec;

fn read_wrapper() -> dex_loop::ToolWrapper {
    dex_loop::ToolWrapper {
        spec: ToolSpec {
            description: "Read an earlier result by ref, or a page by page.".into(),
            ..read_tool("read")
        },
        routes: vec![
            dex_loop::WrapperRoute {
                field: "ref".into(),
                value: None,
                tool: ToolName::new("x.read_ref"),
            },
            dex_loop::WrapperRoute {
                field: "page".into(),
                value: None,
                tool: ToolName::new("x.read_page"),
            },
        ],
    }
}

// A wrapper call is the call it routes to: the routed tool runs with the
// arguments unchanged, the log names the routed tool, and the routed tools
// are callable without being core or found. A call no route matches is told
// which arguments the wrapper takes. The wrapper is declared after tools.call
// on every step, and the routed tools are not declared themselves.
#[tokio::test]
async fn a_wrapper_call_is_the_call_it_routes_to() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("read", json!({"ref": "r1"})),
            call("read", json!({"page": "tests", "limit": 5})),
            call("read", json!({"nothing": true})),
        ],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![
        read_tool("search"),
        hidden_read_tool("x.read_ref"),
        hidden_read_tool("x.read_page"),
    ])
    .wrappers(vec![read_wrapper()]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "read it");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    let mut ran = tools.run_ids();
    ran.sort();
    assert_eq!(ran, strings(&["t1-1-0", "t1-1-1"]));
    assert_eq!(
        tools.run_of(&CallId::new("t1-1-0")).args,
        json!({"ref": "r1"})
    );
    assert_eq!(
        tools.run_of(&CallId::new("t1-1-1")).args,
        json!({"page": "tests", "limit": 5})
    );
    let proposed = ctx
        .history()
        .iter()
        .find_map(|entry| match &entry.message {
            dex_loop::Message::Assistant { calls, .. } if calls.len() == 3 => Some(calls.clone()),
            _ => None,
        })
        .expect("step one's calls");
    assert_eq!(proposed[0].tool, ToolName::new("x.read_ref"));
    assert_eq!(proposed[1].tool, ToolName::new("x.read_page"));
    assert_eq!(proposed[2].tool, ToolName::new("read"));
    let results = view(&model.seen()[1]);
    assert!(
        results
            .iter()
            .any(|line| line.contains("read needs one of these arguments: ref, page")),
        "{results:?}"
    );
    let offered = model.offered();
    assert_eq!(offered.len(), 2);
    for step in &offered {
        assert_eq!(
            *step,
            ["tools.search", "codemode", "tools.call", "read", "search"].map(String::from)
        );
    }
    assert_eq!(log.rehydrate(), ctx);
}

// Without wrappers the declared tools are exactly what they were.
#[tokio::test]
async fn no_wrappers_declare_nothing_extra() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![vec![text("hi")]]);
    let tools = FakeTools::new(vec![read_tool("search"), hidden_read_tool("x.read_ref")]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "hello");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    assert_eq!(model.offered()[0], FIXED_TOOLS.map(String::from));
}

// A routed tool is declared through its wrapper, so a search never lists it
// under its own name; other matches still appear.
#[tokio::test]
async fn search_never_lists_a_routed_tool() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![call("tools.search", json!({"query": "read"}))],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![
        hidden_read_tool("x.read_ref"),
        hidden_read_tool("x.other"),
    ])
    .wrappers(vec![read_wrapper()])
    .search_result("read", &["x.read_ref", "x.other"]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "find a reader");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    let results = view(&model.seen()[1]).join("\n");
    assert!(results.contains("x_other"), "{results}");
    assert!(!results.contains("x_read_ref"), "{results}");
}

fn search_wrapper() -> dex_loop::ToolWrapper {
    let route = |field: &str, value: Option<&str>, tool: &str| dex_loop::WrapperRoute {
        field: field.into(),
        value: value.map(Into::into),
        tool: ToolName::new(tool),
    };
    dex_loop::ToolWrapper {
        spec: ToolSpec {
            description: "Find records, capabilities or tools.".into(),
            ..read_tool("search")
        },
        routes: vec![
            route("scope", Some("records"), "x.find"),
            route("scope", None, "x.describe"),
            route("query", None, "tools.search"),
        ],
    }
}

// A route with a value takes only that value; a later route without one takes
// the rest. A wrapper routing to tools.search is declared in its place, and
// what Dex is told to find tools with names the wrapper.
#[tokio::test]
async fn value_routes_split_one_field_and_a_wrapper_can_replace_tools_search() {
    let log = FakeLog::default();
    let model = FakeModel::new(vec![
        vec![
            call("search", json!({"scope": "records", "query": "acme"})),
            call("search", json!({"scope": "capabilities"})),
            call("search", json!({"query": "assign training"})),
            call("x.nothing", json!({})),
        ],
        vec![text("done")],
    ]);
    let tools = FakeTools::new(vec![
        hidden_read_tool("x.find"),
        hidden_read_tool("x.describe"),
        hidden_read_tool("x.assign"),
    ])
    .wrappers(vec![search_wrapper()])
    .search_result("assign training", &["x.assign"]);
    let engine = engine(&log, &model, &tools, budget());
    let mut ctx = log.start_turn("t1", "look around");
    assert_eq!(
        engine.run(&mut ctx, &CancellationToken::new()).await,
        Ok(Exit::Done)
    );
    let proposed = ctx
        .history()
        .iter()
        .find_map(|entry| match &entry.message {
            dex_loop::Message::Assistant { calls, .. } if calls.len() == 4 => Some(calls.clone()),
            _ => None,
        })
        .expect("step one's calls");
    assert_eq!(proposed[0].tool, ToolName::new("x.find"));
    assert_eq!(proposed[1].tool, ToolName::new("x.describe"));
    assert_eq!(proposed[2].tool, ToolName::new("tools.search"));
    assert_eq!(
        tools.run_of(&CallId::new("t1-1-0")).args,
        json!({"scope": "records", "query": "acme"})
    );
    let results = view(&model.seen()[1]).join("\n");
    assert!(results.contains("x_assign"), "{results}");
    assert!(results.contains("use search to find tools"), "{results}");
    assert_eq!(
        model.offered()[0],
        ["codemode", "tools.call", "search"].map(String::from)
    );
    assert_eq!(log.rehydrate(), ctx);
}
