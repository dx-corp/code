use super::*;

use crate::keybindings::keybindings_test_env_lock;
use tempfile::tempdir;

fn with_temp_keybindings_file<T>(body: impl FnOnce(&Path) -> T) -> T {
    let _guard = keybindings_test_env_lock().blocking_lock();
    let temp = tempdir().expect("tempdir");
    let path = temp.path().join("keybindings.json");
    let previous = std::env::var_os("MAESTRO_KEYBINDINGS_FILE");
    std::env::set_var("MAESTRO_KEYBINDINGS_FILE", &path);
    let result = body(&path);
    match previous {
        Some(value) => std::env::set_var("MAESTRO_KEYBINDINGS_FILE", value),
        None => std::env::remove_var("MAESTRO_KEYBINDINGS_FILE"),
    }
    result
}

#[test]
fn registry_register_and_get() {
    let mut registry = CommandRegistry::new();
    registry.register(Command::new(
        "test",
        "A test command",
        CommandCategory::Diagnostics,
        Box::new(|_| Ok(CommandOutput::Silent)),
    ));

    assert!(registry.get("test").is_some());
    assert!(registry.get("unknown").is_none());
}

#[test]
fn registry_alias_lookup() {
    let mut registry = CommandRegistry::new();
    registry.register(
        Command::new(
            "help",
            "Help command",
            CommandCategory::Navigation,
            Box::new(|_| Ok(CommandOutput::Silent)),
        )
        .alias("h"),
    );

    assert!(registry.get("help").is_some());
    assert!(registry.get("h").is_some());
    assert_eq!(
        registry.get("h").unwrap().name,
        registry.get("help").unwrap().name
    );
}

#[test]
fn registry_execute() {
    let registry = build_command_registry();
    let result = registry.execute("/version", "/tmp", None, None);
    assert!(result.is_ok());
    match result.unwrap() {
        CommandOutput::Message(message) => {
            assert_eq!(
                message,
                format!("Deixic Code v{}", env!("CARGO_PKG_VERSION"))
            );
        }
        other => panic!("expected version message, got {other:?}"),
    }
}

#[test]
fn registry_execute_unknown() {
    let registry = build_command_registry();
    let result = registry.execute("/unknowncommand", "/tmp", None, None);
    assert!(result.is_err());
}

#[test]
fn model_command_parses_default_subcommand() {
    let registry = build_command_registry();

    let result = registry
        .execute("/model default gpt-5.5", "/tmp", None, None)
        .expect("parse");
    match result {
        CommandOutput::Action(CommandAction::SetDefaultModel(model)) => {
            assert_eq!(model, "gpt-5.5");
        }
        other => panic!("expected SetDefaultModel, got {other:?}"),
    }

    let bare = registry.execute("/model default", "/tmp", None, None);
    assert!(bare.is_err(), "/model default requires a model name");

    let session_only = registry
        .execute("/model gpt-5.5", "/tmp", None, None)
        .expect("parse");
    assert!(matches!(
        session_only,
        CommandOutput::Action(CommandAction::SetModel(_))
    ));

    let panel = registry
        .execute("/model", "/tmp", None, None)
        .expect("parse");
    assert!(matches!(
        panel,
        CommandOutput::Action(CommandAction::OpenPanel(ControlPanel::Model))
    ));

    let modal = registry
        .execute("/model select", "/tmp", None, None)
        .expect("parse");
    assert!(matches!(
        modal,
        CommandOutput::OpenModal(ModalType::ModelSelector)
    ));
}

#[test]
fn built_in_commands_exist() {
    let registry = build_command_registry();
    assert!(registry.get("help").is_some());
    assert!(registry.get("hotkeys").is_some());
    assert!(registry.get("keys").is_some());
    assert!(registry.get("shortcuts").is_some());
    assert!(registry.get("theme").is_some());
    assert!(registry.get("model").is_some());
    assert!(registry.get("quit").is_some());
    assert!(registry.get("limits").is_some());
    assert!(registry.get("status").is_some());
    assert!(registry.get("stats").is_some());
    assert!(registry.get("about").is_some());
    assert!(registry.get("context").is_some());
    assert!(registry.get("git").is_some());
    assert!(registry.get("diff").is_some());
    assert!(registry.get("review").is_some());
    assert!(registry.get("a2a").is_some());
    assert!(registry.get("operations").is_some());
    assert!(registry.get("rubber-duck").is_some());
    assert!(registry.get("harness").is_some());
    assert!(registry.get("refine").is_some());
}

#[test]
fn harness_command_parses_scoped_records_and_rollback() {
    use crate::commands::HarnessAction;

    let registry = build_command_registry();

    match registry
        .execute(
            "/refine add workspace memory release-proof native smoke --evidence runbook.md",
            "/tmp",
            Some("session-1"),
            None,
        )
        .expect("/refine add")
    {
        CommandOutput::Action(CommandAction::Harness(HarnessAction::Add {
            scope,
            kind,
            name,
            content,
            evidence,
        })) => {
            assert_eq!(scope, "workspace");
            assert_eq!(kind, "memory");
            assert_eq!(name, "release-proof");
            assert_eq!(content, "native smoke");
            assert_eq!(evidence.as_deref(), Some("runbook.md"));
        }
        other => panic!("expected Harness::Add, got {other:?}"),
    }

    match registry
        .execute(
            "/harness update h-1234 proof --evidence test.log",
            "/tmp",
            None,
            None,
        )
        .expect("/harness update")
    {
        CommandOutput::Action(CommandAction::Harness(HarnessAction::Update {
            id,
            content,
            evidence,
        })) => {
            assert_eq!(id, "h-1234");
            assert_eq!(content, "proof");
            assert_eq!(evidence.as_deref(), Some("test.log"));
        }
        other => panic!("expected Harness::Update, got {other:?}"),
    }

    assert!(matches!(
        registry
            .execute("/harness rollback 7", "/tmp", None, None)
            .expect("/harness rollback"),
        CommandOutput::Action(CommandAction::Harness(HarnessAction::Rollback(7)))
    ));

    match registry
        .execute(
            "/refine propose user memory release-proof native smoke --evidence runbook.md",
            "/tmp",
            None,
            None,
        )
        .expect("/refine propose")
    {
        CommandOutput::Action(CommandAction::Harness(HarnessAction::Propose {
            scope,
            kind,
            name,
            content,
            evidence,
        })) => {
            assert_eq!(scope, "user");
            assert_eq!(kind, "memory");
            assert_eq!(name, "release-proof");
            assert_eq!(content, "native smoke");
            assert_eq!(evidence, "runbook.md");
        }
        other => panic!("expected Harness::Propose, got {other:?}"),
    }

    assert!(matches!(
        registry
            .execute("/refine apply p-1234", "/tmp", None, None)
            .expect("/refine apply"),
        CommandOutput::Action(CommandAction::Harness(HarnessAction::Apply(id))) if id == "p-1234"
    ));
}

#[test]
fn rlm_and_mailbox_commands_parse_context_actions() {
    use crate::commands::{MailboxAction, RlmAction};

    let registry = build_command_registry();
    assert!(matches!(
        registry
            .execute(
                "/rlm set plan Ship release --description current objective",
                "/tmp",
                None,
                None,
            )
            .expect("/rlm set"),
        CommandOutput::Action(CommandAction::Rlm(RlmAction::Set {
            name,
            value,
            description: Some(description),
        })) if name == "plan" && value == "Ship release" && description == "current objective"
    ));
    assert!(matches!(
        registry
            .execute("/rlm render Objective: {{plan}}", "/tmp", None, None)
            .expect("/rlm render"),
        CommandOutput::Action(CommandAction::Rlm(RlmAction::Render(template)))
            if template == "Objective: {{plan}}"
    ));
    assert!(matches!(
        registry
            .execute("/mailbox send child-1 report ready", "/tmp", None, None)
            .expect("/mailbox send"),
        CommandOutput::Action(CommandAction::Mailbox(MailboxAction::Send { recipient, body }))
            if recipient == "child-1" && body == "report ready"
    ));
    assert!(matches!(
        registry
            .execute("/mailbox inspect message-1", "/tmp", None, None)
            .expect("/mailbox inspect"),
        CommandOutput::Action(CommandAction::Mailbox(MailboxAction::Inspect(id)))
            if id == "message-1"
    ));
    assert!(matches!(
        registry
            .execute("/mailbox approve message-1", "/tmp", None, None)
            .expect("/mailbox approve"),
        CommandOutput::Action(CommandAction::Mailbox(MailboxAction::Approve(id)))
            if id == "message-1"
    ));
}

#[test]
fn rubber_duck_command_returns_action() {
    let registry = build_command_registry();

    let bare = registry
        .execute("/rubber-duck", "/tmp", None, Some("gpt-5.5"))
        .expect("parse");
    assert!(matches!(
        bare,
        CommandOutput::Action(CommandAction::RubberDuck { model: None })
    ));

    let with_model = registry
        .execute(
            "/rubber-duck claude-opus-4-6",
            "/tmp",
            None,
            Some("gpt-5.5"),
        )
        .expect("parse");
    match with_model {
        CommandOutput::Action(CommandAction::RubberDuck { model: Some(model) }) => {
            assert_eq!(model, "claude-opus-4-6");
        }
        other => panic!("expected RubberDuck action with model, got {other:?}"),
    }

    let alias = registry
        .execute("/duck", "/tmp", None, Some("gpt-5.5"))
        .expect("parse");
    assert!(matches!(
        alias,
        CommandOutput::Action(CommandAction::RubberDuck { model: None })
    ));
}

#[test]
fn operations_command_opens_read_only_modal() {
    let registry = build_command_registry();
    let output = registry
        .execute("/operations", "/tmp", None, None)
        .expect("operations command");
    assert!(matches!(
        output,
        CommandOutput::OpenModal(ModalType::Operations)
    ));
}

#[test]
fn monitor_command_parses_add_list_and_remove() {
    let registry = build_command_registry();
    let add = registry
        .execute("/monitor add task-1 error \\d+", "/tmp", None, None)
        .expect("monitor add");
    assert!(matches!(
        add,
        CommandOutput::Action(CommandAction::BackgroundMonitor(
            BackgroundMonitorAction::Add { task_id, pattern }
        )) if task_id == "task-1" && pattern == "error \\d+"
    ));
    assert!(matches!(
        registry
            .execute("/monitor list", "/tmp", None, None)
            .expect("monitor list"),
        CommandOutput::Action(CommandAction::BackgroundMonitor(
            BackgroundMonitorAction::List
        ))
    ));
    assert!(matches!(
        registry
            .execute("/monitor remove monitor-1", "/tmp", None, None)
            .expect("monitor remove"),
        CommandOutput::Action(CommandAction::BackgroundMonitor(
            BackgroundMonitorAction::Remove { monitor_id }
        )) if monitor_id == "monitor-1"
    ));
}

#[test]
fn a2a_command_parses_peer_actions() {
    let registry = build_command_registry();

    assert!(registry.execute("/a2a fleet", "/tmp", None, None).is_ok());
    assert!(registry.execute("/a2a tasks", "/tmp", None, None).is_ok());
    assert!(
        registry
            .execute("/a2a tasks --work-graph mac-mini", "/tmp", None, None)
            .is_ok()
    );
    assert!(
        registry
            .execute("/a2a coordinate", "/tmp", None, None)
            .is_ok()
    );
    assert!(
        registry
            .execute(
                "/a2a delegate mac-mini run workspace smoke",
                "/tmp",
                None,
                None,
            )
            .is_ok()
    );

    match registry
        .execute("/a2a peers", "/tmp", None, None)
        .expect("a2a peers should parse")
    {
        CommandOutput::Action(CommandAction::A2a(A2aAction::Peers)) => {}
        other => panic!("expected a2a peers action, got {other:?}"),
    }

    match registry
        .execute("/a2a tasks --work-graph mac-mini", "/tmp", None, None)
        .expect("a2a task work graph view should parse")
    {
        CommandOutput::Action(CommandAction::A2a(A2aAction::Tasks {
            peer,
            include_work_graph,
        })) => {
            assert_eq!(peer.as_deref(), Some("mac-mini"));
            assert!(include_work_graph);
        }
        other => panic!("expected a2a tasks action, got {other:?}"),
    }

    match registry
        .execute(
            "/a2a accept maestro-pair-v1.payload.checksum",
            "/tmp",
            None,
            None,
        )
        .expect("a2a accept should parse")
    {
        CommandOutput::Action(CommandAction::A2a(A2aAction::Accept { code })) => {
            assert_eq!(code, "maestro-pair-v1.payload.checksum");
        }
        other => panic!("expected a2a accept action, got {other:?}"),
    }

    match registry
        .execute(
            "/a2a register --agent-id maestro-peer-1 --url https://maestro.example/a2a",
            "/tmp",
            None,
            None,
        )
        .expect("a2a register should parse")
    {
        CommandOutput::Action(CommandAction::A2a(A2aAction::Register {
            agent_id,
            public_url,
            heartbeat_only,
        })) => {
            assert_eq!(agent_id.as_deref(), Some("maestro-peer-1"));
            assert_eq!(public_url.as_deref(), Some("https://maestro.example/a2a"));
            assert!(!heartbeat_only);
        }
        other => panic!("expected a2a register action, got {other:?}"),
    }

    match registry
        .execute(
            "/a2a publish --heartbeat-only --agent-id maestro-peer-1",
            "/tmp",
            None,
            None,
        )
        .expect("a2a heartbeat-only publish should parse")
    {
        CommandOutput::Action(CommandAction::A2a(A2aAction::Register {
            agent_id,
            public_url,
            heartbeat_only,
        })) => {
            assert_eq!(agent_id.as_deref(), Some("maestro-peer-1"));
            assert!(public_url.is_none());
            assert!(heartbeat_only);
        }
        other => panic!("expected a2a register heartbeat action, got {other:?}"),
    }

    match registry
        .execute(
            "/a2a reply mac-mini task-1 use the short smoke",
            "/tmp",
            None,
            None,
        )
        .expect("a2a reply should parse")
    {
        CommandOutput::Action(CommandAction::A2a(A2aAction::Reply {
            peer,
            task_id,
            text,
        })) => {
            assert_eq!(peer, "mac-mini");
            assert_eq!(task_id, "task-1");
            assert_eq!(text, "use the short smoke");
        }
        other => panic!("expected a2a reply action, got {other:?}"),
    }

    match registry
        .execute(
            "/a2a coordinate mac-mini --work-graph --reply use the short smoke",
            "/tmp",
            None,
            None,
        )
        .expect("a2a coordinate should parse")
    {
        CommandOutput::Action(CommandAction::A2a(A2aAction::Coordinate {
            peer,
            reply,
            include_work_graph,
        })) => {
            assert_eq!(peer.as_deref(), Some("mac-mini"));
            assert_eq!(reply.as_deref(), Some("use the short smoke"));
            assert!(include_work_graph);
        }
        other => panic!("expected a2a coordinate action, got {other:?}"),
    }

    match registry
        .execute(
            "/a2a coordinate --work-graph mac-mini --reply use the short smoke",
            "/tmp",
            None,
            None,
        )
        .expect("a2a coordinate should parse work-graph before peer")
    {
        CommandOutput::Action(CommandAction::A2a(A2aAction::Coordinate {
            peer,
            reply,
            include_work_graph,
        })) => {
            assert_eq!(peer.as_deref(), Some("mac-mini"));
            assert_eq!(reply.as_deref(), Some("use the short smoke"));
            assert!(include_work_graph);
        }
        other => panic!("expected a2a coordinate action, got {other:?}"),
    }

    match registry
        .execute("/a2a send mac-mini review this branch", "/tmp", None, None)
        .expect("a2a send should parse")
    {
        CommandOutput::Action(CommandAction::A2a(A2aAction::Send { peer, text })) => {
            assert_eq!(peer, "mac-mini");
            assert_eq!(text, "review this branch");
        }
        other => panic!("expected a2a send action, got {other:?}"),
    }
}

#[test]
fn setup_command_opens_setup_modal() {
    let registry = build_command_registry();
    let result = registry.execute("/setup", "/tmp", None, None);
    match result.expect("setup command should succeed") {
        CommandOutput::OpenModal(ModalType::Setup) => {}
        other => panic!("expected setup modal, got {other:?}"),
    }
}

#[test]
fn zen_command_toggles_zen_mode() {
    let registry = build_command_registry();
    match registry.execute("/zen", "/tmp", None, None).expect("/zen") {
        CommandOutput::Action(CommandAction::ToggleZenMode) => {}
        other => panic!("expected ToggleZenMode, got {other:?}"),
    }
}

#[test]
fn help_for_named_command_prints_usage() {
    let registry = build_command_registry();
    match registry
        .execute("/help setup", "/tmp", None, None)
        .expect("/help setup")
    {
        CommandOutput::Message(message) => {
            assert!(message.contains("/setup"), "{message}");
            assert!(message.contains("Usage: /setup"), "{message}");
            assert!(message.contains("EvalOps"), "{message}");
        }
        other => panic!("expected help message, got {other:?}"),
    }

    let err = registry
        .execute("/help not-a-command", "/tmp", None, None)
        .expect_err("unknown command help should fail");
    assert!(
        err.message.contains("No help available for /not-a-command"),
        "{err}"
    );
    assert!(
        !err.message.contains("Unknown command"),
        "must not trip unknown-slash fallback: {err}"
    );
}

#[test]
fn init_command_requests_scaffold_action() {
    let registry = build_command_registry();
    match registry
        .execute("/init", "/tmp", None, None)
        .expect("/init")
    {
        CommandOutput::Action(CommandAction::Init { force }) => assert!(!force),
        other => panic!("expected Init, got {other:?}"),
    }
    match registry
        .execute("/init --force", "/tmp", None, None)
        .expect("/init --force")
    {
        CommandOutput::Action(CommandAction::Init { force }) => assert!(force),
        other => panic!("expected Init force, got {other:?}"),
    }
    assert!(registry.execute("/init extra", "/tmp", None, None).is_err());
    assert!(
        registry
            .execute("/init --force somewhere/else", "/tmp", None, None)
            .is_err(),
        "/init --force must still reject a target path"
    );
}

#[test]
fn hotkeys_command_opens_shortcuts_help_modal() {
    let registry = build_command_registry();
    let result = registry.execute("/hotkeys", "/tmp", None, None);

    match result.expect("hotkeys command should succeed") {
        CommandOutput::OpenModal(ModalType::ShortcutsHelp) => {}
        other => panic!("expected shortcuts help modal, got {other:?}"),
    }
}

#[test]
fn hotkeys_command_can_init_and_validate_keybindings_config() {
    with_temp_keybindings_file(|path| {
        let registry = build_command_registry();
        let init_result = registry
            .execute("/hotkeys init", "/tmp", None, None)
            .expect("hotkeys init should succeed");
        match init_result {
            CommandOutput::Message(message) => {
                assert!(message.contains("Created keyboard shortcuts config at"));
            }
            other => panic!("expected init message, got {other:?}"),
        }
        assert!(path.exists(), "hotkeys init should create the config file");

        let validate_result = registry
            .execute("/hotkeys validate", "/tmp", None, None)
            .expect("hotkeys validate should succeed");
        match validate_result {
            CommandOutput::Message(message) => {
                assert!(message.contains("Keyboard Shortcuts Config:"));
                assert!(message.contains("Status: present"));
                assert!(message.contains("Rust TUI overrides:"));
            }
            other => panic!("expected validation message, got {other:?}"),
        }
    });
}

#[test]
fn hotkeys_command_requires_force_to_overwrite_existing_config() {
    with_temp_keybindings_file(|path| {
        std::fs::write(path, r#"{"version":1,"bindings":{}}"#).expect("write keybindings config");
        let registry = build_command_registry();
        let err = registry
            .execute("/hotkeys init", "/tmp", None, None)
            .expect_err("init without force should fail when config exists");

        assert_eq!(
            err.message,
            format!("Keybindings config already exists at {}.", path.display())
        );
        assert_eq!(
            err.hint,
            Some("Re-run with /hotkeys init --force to overwrite it.".to_string())
        );
    });
}

#[test]
fn context_command_exists() {
    let registry = build_command_registry();
    assert!(registry.get("context").is_some());
}

#[test]
fn context_command_opens_panel_and_usage_returns_show_context_action() {
    let registry = build_command_registry();
    let result = registry.execute("/context", "/tmp", None, None);
    assert!(matches!(
        result,
        Ok(CommandOutput::Action(CommandAction::OpenPanel(
            ControlPanel::Context
        )))
    ));
    let usage = registry.execute("/context usage", "/tmp", None, None);
    assert!(matches!(
        usage,
        Ok(CommandOutput::Action(CommandAction::ShowContext))
    ));
}

#[test]
fn context_audit_commands_parse() {
    let registry = build_command_registry();
    assert!(matches!(
        registry.execute("/context audit", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::ShowPromptAudit {
            json: false
        }))
    ));
    assert!(matches!(
        registry.execute("/context audit --json", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::ShowPromptAudit {
            json: true
        }))
    ));
    assert!(matches!(
        registry.execute("/prompt-audit", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::ShowPromptAudit {
            json: false
        }))
    ));
}

#[test]
fn focus_command_parses() {
    let registry = build_command_registry();
    assert!(matches!(
        registry.execute("/focus", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::SetFocus(None)))
    ));
    assert!(matches!(
        registry.execute("/focus on", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::SetFocus(Some(true))))
    ));
    assert!(matches!(
        registry.execute("/focus off", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::SetFocus(Some(false))))
    ));
}

#[test]
fn cost_command_exists() {
    let registry = build_command_registry();
    assert!(registry.get("cost").is_some());
    assert!(registry.get("usage").is_some()); // alias
    assert!(registry.get("tokens").is_some()); // alias
}

#[test]
fn cost_command_actions() {
    let registry = build_command_registry();

    // Summary (default)
    let result = registry.execute("/cost", "/tmp", None, None);
    assert!(result.is_ok());

    // Detailed
    let result = registry.execute("/cost detailed", "/tmp", None, None);
    assert!(result.is_ok());

    // Reset
    let result = registry.execute("/cost reset", "/tmp", None, None);
    assert!(result.is_ok());

    // Invalid
    let result = registry.execute("/cost invalid", "/tmp", None, None);
    assert!(result.is_err());
}

#[test]
fn export_command_exists() {
    let registry = build_command_registry();
    assert!(registry.get("export").is_some());
}

#[test]
fn workflow_dashboard_command_lists_durable_runs() {
    let registry = build_command_registry();
    let workspace = tempfile::tempdir().unwrap();
    let result = registry
        .execute(
            "/workflows",
            &workspace.path().to_string_lossy(),
            None,
            None,
        )
        .unwrap();
    assert!(matches!(result, CommandOutput::Message(message) if message == "No workflow runs."));
}

#[test]
fn export_command_formats() {
    let registry = build_command_registry();

    // No args (show options)
    let result = registry.execute("/export", "/tmp", None, None);
    assert!(result.is_ok());

    // Markdown
    let result = registry.execute("/export markdown", "/tmp", None, None);
    assert!(result.is_ok());

    // HTML with path
    let result = registry.execute("/export html output.html", "/tmp", None, None);
    assert!(result.is_ok());

    // Invalid format
    let result = registry.execute("/export invalid", "/tmp", None, None);
    assert!(result.is_err());
}

#[test]
fn history_command_exists() {
    let registry = build_command_registry();
    assert!(registry.get("history").is_some());
    assert!(registry.get("hist").is_some()); // alias
}

#[test]
fn history_command_actions() {
    let registry = build_command_registry();

    // Default (recent 20)
    let result = registry.execute("/history", "/tmp", None, None);
    assert!(result.is_ok());

    // With count
    let result = registry.execute("/history 10", "/tmp", None, None);
    assert!(result.is_ok());

    // Search
    let result = registry.execute("/history git status", "/tmp", None, None);
    assert!(result.is_ok());

    // Clear
    let result = registry.execute("/history clear", "/tmp", None, None);
    assert!(result.is_ok());
}

#[test]
fn toolhistory_command_exists() {
    let registry = build_command_registry();
    assert!(registry.get("toolhistory").is_some());
    assert!(registry.get("th").is_some()); // alias
}

#[test]
fn toolhistory_command_actions() {
    let registry = build_command_registry();

    // Default
    let result = registry.execute("/toolhistory", "/tmp", None, None);
    assert!(result.is_ok());

    // Stats
    let result = registry.execute("/toolhistory stats", "/tmp", None, None);
    assert!(result.is_ok());

    // For specific tool
    let result = registry.execute("/toolhistory read", "/tmp", None, None);
    assert!(result.is_ok());

    // Clear
    let result = registry.execute("/toolhistory clear", "/tmp", None, None);
    assert!(result.is_ok());
}

#[test]
fn mcp_prompts_command_parses_prompt_arguments() {
    let registry = build_command_registry();
    let result = registry.execute(
        r#"/mcp prompts docs summarize topic="MCP auth flow" format=brief"#,
        "/tmp",
        None,
        None,
    );

    match result.expect("mcp prompt args should parse") {
        CommandOutput::Action(CommandAction::Mcp(McpAction::Prompts {
            server,
            name,
            arguments,
        })) => {
            assert_eq!(server.as_deref(), Some("docs"));
            assert_eq!(name.as_deref(), Some("summarize"));
            assert_eq!(
                arguments.get("topic").map(std::string::String::as_str),
                Some("MCP auth flow")
            );
            assert_eq!(
                arguments.get("format").map(std::string::String::as_str),
                Some("brief")
            );
        }
        other => panic!("expected MCP prompts action, got {other:?}"),
    }
}

#[test]
fn mcp_config_commands_preserve_conversational_arguments() {
    let registry = build_command_registry();
    for command in [
        "/mcp config add-http docs https://example.test --scope project",
        "/mcp-config add-stdio local cargo run --scope local",
    ] {
        match registry
            .execute(command, "/tmp", None, None)
            .expect("MCP configuration should parse")
        {
            CommandOutput::Action(CommandAction::Mcp(McpAction::Configure { args })) => {
                assert!(args.len() >= 3);
            }
            other => panic!("unexpected output: {other:?}"),
        }
    }
}

#[test]
fn mcp_prompts_command_rejects_invalid_prompt_arguments() {
    let registry = build_command_registry();
    let result = registry.execute(
        "/mcp prompts docs summarize invalid-arg",
        "/tmp",
        None,
        None,
    );

    let err = result.expect_err("invalid MCP prompt args should fail");
    assert_eq!(
        err.message,
        "Invalid MCP prompt argument. Use KEY=value after the prompt name."
    );
}

#[test]
fn register_if_absent_skips_colliding_names() {
    let mut registry = CommandRegistry::new();
    registry.register(Command::new(
        "help",
        "builtin",
        CommandCategory::Navigation,
        Box::new(|_| Ok(CommandOutput::Silent)),
    ));
    let registered = registry.register_if_absent(Command::new(
        "help",
        "skill",
        CommandCategory::Tools,
        Box::new(|_| Ok(CommandOutput::Silent)),
    ));
    assert!(!registered);
    assert_eq!(registry.get("help").unwrap().description, "builtin");
}

#[test]
fn extensions_register_skills_and_prompts_as_slash_commands() {
    use crate::prompts::{PromptDefinition, PromptSource};
    use crate::skills::{LoadedSkill, SkillDefinition, SkillSource};
    use std::path::PathBuf;

    let skill = LoadedSkill {
        definition: SkillDefinition::new("commit", "commit")
            .with_description("Create a commit")
            .with_source(SkillSource::User)
            .with_system_prompt("Commit carefully."),
        source_path: PathBuf::from("/tmp/commit/SKILL.md"),
        skill_dir: PathBuf::from("/tmp/commit"),
        resources: Default::default(),
    };
    let prompt = PromptDefinition {
        name: "code-review".to_string(),
        description: Some("Review code".to_string()),
        argument_hint: None,
        body: "Review $ARGUMENTS".to_string(),
        source_path: PathBuf::from("/tmp/review.md"),
        source_type: PromptSource::Project,
        named_placeholders: vec![],
        has_positional_placeholders: true,
    };

    let registry = build_command_registry_with_extensions(&[skill], &[prompt]);

    match registry
        .execute("/commit fix typo", "/tmp", None, None)
        .expect("skill slash should parse")
    {
        CommandOutput::Action(CommandAction::InvokeSkill { name, args }) => {
            assert_eq!(name, "commit");
            assert_eq!(args, "fix typo");
        }
        other => panic!("expected InvokeSkill, got {other:?}"),
    }

    match registry
        .execute("/code-review auth module", "/tmp", None, None)
        .expect("prompt slash should parse")
    {
        CommandOutput::Action(CommandAction::InvokePromptTemplate { name, args }) => {
            assert_eq!(name, "code-review");
            assert_eq!(args, "auth module");
        }
        other => panic!("expected InvokePromptTemplate, got {other:?}"),
    }
}

#[test]
fn skill_named_like_builtin_is_not_registered() {
    use crate::skills::{LoadedSkill, SkillDefinition, SkillSource};
    use std::path::PathBuf;

    let skill = LoadedSkill {
        definition: SkillDefinition::new("compact", "compact")
            .with_description("skill compact")
            .with_source(SkillSource::User),
        source_path: PathBuf::from("/tmp/compact/SKILL.md"),
        skill_dir: PathBuf::from("/tmp/compact"),
        resources: Default::default(),
    };

    let registry = build_command_registry_with_extensions(&[skill], &[]);
    let out = registry
        .execute("/compact", "/tmp", None, None)
        .expect("builtin compact");
    assert!(
        !matches!(
            out,
            CommandOutput::Action(CommandAction::InvokeSkill { .. })
        ),
        "builtin should win over skill, got {out:?}"
    );
}

#[test]
fn fork_and_rewind_commands_exist() {
    let registry = build_command_registry();
    assert!(registry.get("fork").is_some());
    assert!(registry.get("rewind").is_some());
    assert!(registry.get("new").is_some()); // alias of clear
    assert!(registry.get("always-approve").is_some());
    assert!(registry.get("auto").is_some());
    assert!(registry.get("ask").is_some());
}

#[test]
fn new_command_starts_session_action() {
    let registry = build_command_registry();
    match registry.execute("/new", "/tmp", None, None).expect("/new") {
        CommandOutput::Action(CommandAction::Session(SessionAction::New)) => {}
        other => panic!("expected Session::New, got {other:?}"),
    }
}

#[test]
fn rewind_parses_turn_count() {
    let registry = build_command_registry();
    match registry
        .execute("/rewind 3", "/tmp", None, None)
        .expect("/rewind")
    {
        CommandOutput::Action(CommandAction::Session(SessionAction::Rewind { turns, .. })) => {
            assert_eq!(turns, 3);
        }
        other => panic!("expected Rewind, got {other:?}"),
    }
}

#[test]
fn rewind_parses_dry_run_and_rejects_history_only() {
    let registry = build_command_registry();
    match registry
        .execute("/rewind --dry-run 2", "/tmp", None, None)
        .expect("/rewind flags")
    {
        CommandOutput::Action(CommandAction::Session(SessionAction::Rewind { turns, dry_run })) => {
            assert_eq!(turns, 2);
            assert!(dry_run);
        }
        other => panic!("expected flagged Rewind, got {other:?}"),
    }
    assert!(
        registry
            .execute("/rewind --history-only", "/tmp", None, None)
            .is_err()
    );
}

#[test]
fn rewind_parses_files_and_checkpoints_subcommands() {
    let registry = build_command_registry();
    match registry
        .execute("/rewind files", "/tmp", None, None)
        .expect("/rewind files")
    {
        CommandOutput::Action(CommandAction::Session(SessionAction::RewindFiles)) => {}
        other => panic!("expected RewindFiles, got {other:?}"),
    }
    match registry
        .execute("/rewind checkpoints", "/tmp", None, None)
        .expect("/rewind checkpoints")
    {
        CommandOutput::Action(CommandAction::Session(SessionAction::ListCheckpoints)) => {}
        other => panic!("expected ListCheckpoints, got {other:?}"),
    }
    assert!(
        registry
            .execute("/rewind files extra", "/tmp", None, None)
            .is_err()
    );
}

#[test]
fn btw_and_structured_plan_review_commands_parse() {
    let registry = build_command_registry();
    match registry
        .execute("/btw why is this queued?", "/tmp", None, None)
        .expect("/btw")
    {
        CommandOutput::Action(CommandAction::SideQuestion(question)) => {
            assert_eq!(question, "why is this queued?");
        }
        other => panic!("expected SideQuestion, got {other:?}"),
    }
    match registry
        .execute("/plan comment 3-7 handle errors", "/tmp", None, None)
        .expect("/plan comment")
    {
        CommandOutput::Action(CommandAction::PlanReview(PlanReviewAction::Comment {
            start_line,
            end_line,
            text,
        })) => {
            assert_eq!((start_line, end_line), (3, 7));
            assert_eq!(text, "handle errors");
        }
        other => panic!("expected plan comment, got {other:?}"),
    }
    assert!(matches!(
        registry
            .execute("/plan resolve #4", "/tmp", None, None)
            .expect("/plan resolve"),
        CommandOutput::Action(CommandAction::PlanReview(PlanReviewAction::Resolve {
            id: 4
        }))
    ));
    assert!(matches!(
        registry
            .execute("/plan reopen 4", "/tmp", None, None)
            .expect("/plan reopen"),
        CommandOutput::Action(CommandAction::PlanReview(PlanReviewAction::Reopen {
            id: 4
        }))
    ));
}

#[test]
fn double_slash_command_still_resolves() {
    let registry = build_command_registry();
    // Completion bug used to produce `//help`; execute must tolerate it.
    match registry
        .execute("//help", "/tmp", None, None)
        .expect("//help should resolve")
    {
        CommandOutput::Action(CommandAction::OpenPanel(ControlPanel::Help)) => {}
        other => panic!("expected help output for //help, got {other:?}"),
    }
    match registry
        .execute("///plan on", "/tmp", None, None)
        .expect("///plan should resolve")
    {
        CommandOutput::Action(CommandAction::SetPlanMode(true)) => {}
        other => panic!("expected SetPlanMode for ///plan on, got {other:?}"),
    }
}

#[test]
fn plan_and_permission_shortcuts_parse() {
    let registry = build_command_registry();
    match registry
        .execute("/plan", "/tmp", None, None)
        .expect("/plan")
    {
        CommandOutput::Action(CommandAction::SetPlanMode(true)) => {}
        other => panic!("expected SetPlanMode(true), got {other:?}"),
    }
    match registry
        .execute("/plan off", "/tmp", None, None)
        .expect("/plan off")
    {
        CommandOutput::Action(CommandAction::SetPlanMode(false)) => {}
        other => panic!("expected SetPlanMode(false), got {other:?}"),
    }
    match registry
        .execute("/plan approve", "/tmp", None, None)
        .expect("/plan approve")
    {
        CommandOutput::Action(CommandAction::ApprovePlan) => {}
        other => panic!("expected ApprovePlan, got {other:?}"),
    }
    match registry
        .execute("/view-plan", "/tmp", None, None)
        .expect("/view-plan")
    {
        CommandOutput::Action(CommandAction::ViewPlan) => {}
        other => panic!("expected ViewPlan, got {other:?}"),
    }
    match registry
        .execute("/always-approve", "/tmp", None, None)
        .expect("/always-approve")
    {
        CommandOutput::Action(CommandAction::SetApprovalMode(mode)) => {
            assert_eq!(mode, "yolo");
        }
        other => panic!("expected SetApprovalMode yolo, got {other:?}"),
    }
}

#[test]
fn tools_command_lists_tools_action() {
    let registry = build_command_registry();
    match registry
        .execute("/tools", "/tmp", None, None)
        .expect("/tools")
    {
        CommandOutput::Action(CommandAction::ShowTools) => {}
        other => panic!("expected ShowTools, got {other:?}"),
    }
}

#[test]
fn memory_and_continue_commands_parse() {
    let registry = build_command_registry();
    match registry
        .execute("/memory", "/tmp", None, None)
        .expect("/memory")
    {
        CommandOutput::Action(CommandAction::ShowMemory) => {}
        other => panic!("expected ShowMemory, got {other:?}"),
    }
    assert!(
        matches!(registry.execute("/memory save foo", "/tmp", None, None).unwrap(),
        CommandOutput::Action(CommandAction::Harness(HarnessAction::Apply(id))) if id == "foo")
    );
    match registry
        .execute("/continue", "/tmp", None, None)
        .expect("/continue")
    {
        CommandOutput::Action(CommandAction::Session(SessionAction::Continue)) => {}
        other => panic!("expected Session::Continue, got {other:?}"),
    }
}

#[test]
fn plugins_command_list_info_and_reload() {
    use crate::commands::PluginsAction;

    let registry = build_command_registry();

    match registry
        .execute("/plugins", "/tmp", None, None)
        .expect("/plugins")
    {
        CommandOutput::Action(CommandAction::Plugins(PluginsAction::List)) => {}
        other => panic!("expected Plugins::List, got {other:?}"),
    }

    match registry
        .execute("/plugins team-tools", "/tmp", None, None)
        .expect("/plugins team-tools")
    {
        CommandOutput::Action(CommandAction::Plugins(PluginsAction::Info(name))) => {
            assert_eq!(name, "team-tools");
        }
        other => panic!("expected Plugins::Info, got {other:?}"),
    }

    match registry
        .execute("/plugins reload", "/tmp", None, None)
        .expect("/plugins reload")
    {
        CommandOutput::Action(CommandAction::Plugins(PluginsAction::Reload)) => {}
        other => panic!("expected Plugins::Reload, got {other:?}"),
    }

    match registry
        .execute("/plugin info demo", "/tmp", None, None)
        .expect("/plugin info demo")
    {
        CommandOutput::Action(CommandAction::Plugins(PluginsAction::Info(name))) => {
            assert_eq!(name, "demo");
        }
        other => panic!("expected Plugins::Info via alias, got {other:?}"),
    }

    match registry
        .execute("/plugins marketplace", "/tmp", None, None)
        .expect("/plugins marketplace")
    {
        CommandOutput::Action(CommandAction::Plugins(PluginsAction::MarketplaceList)) => {}
        other => panic!("expected MarketplaceList, got {other:?}"),
    }

    match registry
        .execute(
            "/plugins marketplace install superpowers --trust",
            "/tmp",
            None,
            None,
        )
        .expect("/plugins marketplace install")
    {
        CommandOutput::Action(CommandAction::Plugins(PluginsAction::MarketplaceInstall {
            id,
            trust,
        })) => {
            assert_eq!(id, "superpowers");
            assert!(trust);
        }
        other => panic!("expected MarketplaceInstall, got {other:?}"),
    }
}

#[test]
fn goal_footer_attach_commands_parse() {
    use crate::commands::{AttachAction, FooterStyle, GoalAction};

    let registry = build_command_registry();

    match registry
        .execute("/goal create Ship release", "/tmp", None, None)
        .expect("/goal create")
    {
        CommandOutput::Action(CommandAction::Goal(GoalAction::Create {
            text,
            replace,
            max_turns,
            token_budget,
            ..
        })) => {
            assert_eq!(text, "Ship release");
            assert!(!replace);
            assert_eq!(max_turns, None);
            assert_eq!(token_budget, None);
        }
        other => panic!("expected Goal::Create, got {other:?}"),
    }

    match registry
        .execute(
            "/goal create --max-turns 3 --token-budget 9000 Ship release",
            "/tmp",
            None,
            None,
        )
        .expect("/goal create max-turns")
    {
        CommandOutput::Action(CommandAction::Goal(GoalAction::Create {
            text,
            max_turns: Some(3),
            token_budget: Some(9000),
            ..
        })) => {
            assert_eq!(text, "Ship release");
        }
        other => panic!("expected Goal::Create with max_turns 3 and budget, got {other:?}"),
    }

    match registry
        .execute(
            "/goal create --max-duration-secs 120 Ship release",
            "/tmp",
            None,
            None,
        )
        .expect("/goal create duration")
    {
        CommandOutput::Action(CommandAction::Goal(GoalAction::Create {
            max_duration_secs: Some(120),
            ..
        })) => {}
        other => panic!("expected Goal::Create with duration, got {other:?}"),
    }

    match registry
        .execute("/goal pause", "/tmp", None, None)
        .expect("/goal pause")
    {
        CommandOutput::Action(CommandAction::Goal(GoalAction::Pause)) => {}
        other => panic!("expected Goal::Pause, got {other:?}"),
    }

    match registry
        .execute("/goal auto off", "/tmp", None, None)
        .expect("/goal auto off")
    {
        CommandOutput::Action(CommandAction::Goal(GoalAction::AutoContinue { enabled: false })) => {
        }
        other => panic!("expected Goal::AutoContinue off, got {other:?}"),
    }

    match registry
        .execute("/footer solo", "/tmp", None, None)
        .expect("/footer solo")
    {
        CommandOutput::Action(CommandAction::SetFooterStyle(FooterStyle::Solo)) => {}
        other => panic!("expected SetFooterStyle::Solo, got {other:?}"),
    }

    match registry
        .execute("/attach /tmp/shot.png", "/tmp", None, None)
        .expect("/attach")
    {
        CommandOutput::Action(CommandAction::Attach(AttachAction::Add(path))) => {
            assert_eq!(path, "/tmp/shot.png");
        }
        other => panic!("expected Attach::Add, got {other:?}"),
    }

    match registry
        .execute("/attach list", "/tmp", None, None)
        .expect("/attach list")
    {
        CommandOutput::Action(CommandAction::Attach(AttachAction::List)) => {}
        other => panic!("expected Attach::List, got {other:?}"),
    }

    match registry
        .execute("/attach clear", "/tmp", None, None)
        .expect("/attach clear")
    {
        CommandOutput::Action(CommandAction::Attach(AttachAction::Clear)) => {}
        other => panic!("expected Attach::Clear, got {other:?}"),
    }

    match registry
        .execute("/attach remove 2", "/tmp", None, None)
        .expect("/attach remove")
    {
        CommandOutput::Action(CommandAction::Attach(AttachAction::Remove { index: 2 })) => {}
        other => panic!("expected Attach::Remove 2, got {other:?}"),
    }

    match registry
        .execute("/mcp-config", "/tmp", None, None)
        .expect("/mcp-config manager")
    {
        CommandOutput::Action(CommandAction::Mcp(McpAction::Status)) => {}
        other => panic!("expected MCP manager action, got {other:?}"),
    }
}

fn prefix_test_registry() -> CommandRegistry {
    let mut registry = CommandRegistry::new();
    registry.register(
        Command::new(
            "quit",
            "Quit the app",
            CommandCategory::Session,
            Box::new(|_| Ok(CommandOutput::Silent)),
        )
        .alias("exit"),
    );
    registry.register(
        Command::new(
            "queue",
            "Manage queued prompts",
            CommandCategory::Session,
            Box::new(|_| Ok(CommandOutput::Silent)),
        )
        .alias("q"),
    );
    registry.register(Command::new(
        "theme",
        "Pick a theme",
        CommandCategory::Ui,
        Box::new(|_| Ok(CommandOutput::Silent)),
    ));
    registry
}

#[test]
fn resolve_unique_prefix_unique_match() {
    let registry = prefix_test_registry();

    let resolved = registry
        .resolve_unique_prefix("qui")
        .expect("unique prefix")
        .expect("a match");
    assert_eq!(resolved.name, "quit");
}

#[test]
fn resolve_unique_prefix_via_alias() {
    let registry = prefix_test_registry();

    // "exi" is a prefix of the "exit" alias and resolves to the canonical command.
    let resolved = registry
        .resolve_unique_prefix("exi")
        .expect("unique prefix")
        .expect("a match");
    assert_eq!(resolved.name, "quit");
}

#[test]
fn resolve_unique_prefix_ambiguous() {
    let registry = prefix_test_registry();

    // "qu" is a prefix of both "quit" and "queue" (and alias "q" is exact, not prefix-relevant here).
    let mut candidates = registry.resolve_unique_prefix("qu").unwrap_err();
    candidates.sort();
    assert_eq!(candidates, vec!["queue".to_string(), "quit".to_string()]);
}

#[test]
fn resolve_unique_prefix_no_match() {
    let registry = prefix_test_registry();

    assert!(registry.resolve_unique_prefix("xyz").unwrap().is_none());
}

#[test]
fn resolve_unique_prefix_empty_is_no_match() {
    let registry = prefix_test_registry();

    assert!(registry.resolve_unique_prefix("").unwrap().is_none());
}

#[test]
fn resolve_unique_prefix_is_case_insensitive() {
    let registry = prefix_test_registry();

    let resolved = registry
        .resolve_unique_prefix("QUI")
        .expect("unique prefix")
        .expect("a match");
    assert_eq!(resolved.name, "quit");
}

#[test]
fn resolve_unique_prefix_real_registry_qui_is_quit() {
    let registry = build_command_registry();

    let resolved = registry
        .resolve_unique_prefix("qui")
        .expect("/qui should be unambiguous")
        .expect("/qui should match a command");
    assert_eq!(resolved.name, "quit");
}

#[test]
fn resolve_typo_single_edit_rescues() {
    let registry = prefix_test_registry();

    // Transposition: "quti" -> "quit"
    let resolved = registry
        .resolve_typo("quti")
        .expect("unique rescue")
        .expect("a match");
    assert_eq!(resolved.name, "quit");

    // Missing letter: "them" -> "theme"
    let resolved = registry
        .resolve_typo("them")
        .expect("unique rescue")
        .expect("a match");
    assert_eq!(resolved.name, "theme");
}

#[test]
fn resolve_typo_ignores_short_input() {
    let registry = prefix_test_registry();

    // "qu" is within distance 2 of "quit"/"queue" but short input never rescues.
    assert!(registry.resolve_typo("qu").unwrap().is_none());
}

#[test]
fn resolve_typo_no_match_when_too_far() {
    let registry = prefix_test_registry();

    assert!(registry.resolve_typo("zebra").unwrap().is_none());
}

#[test]
fn resolve_typo_ambiguous_returns_candidates() {
    let mut registry = CommandRegistry::new();
    registry.register(Command::new(
        "dump",
        "Dump state",
        CommandCategory::Diagnostics,
        Box::new(|_| Ok(CommandOutput::Silent)),
    ));
    registry.register(Command::new(
        "pump",
        "Pump state",
        CommandCategory::Diagnostics,
        Box::new(|_| Ok(CommandOutput::Silent)),
    ));

    // "xump" is distance 1 from both "dump" and "pump" (and a prefix of neither).
    let candidates = registry.resolve_typo("xump").unwrap_err();
    assert_eq!(candidates, vec!["dump".to_string(), "pump".to_string()]);
}

#[test]
fn edit_distance_basics() {
    assert_eq!(edit_distance("quit", "quit"), 0);
    assert_eq!(edit_distance("quti", "quit"), 1);
    assert_eq!(edit_distance("quit", "qui"), 1);
    assert_eq!(edit_distance("quit", "quite"), 1);
    assert_eq!(edit_distance("quit", "zebra"), 5);
}

#[test]
fn loop_command_parses_interval_and_prompt() {
    let registry = build_command_registry();

    match registry
        .execute("/loop 5m check the build", "/tmp", None, None)
        .expect("/loop 5m")
    {
        CommandOutput::Action(CommandAction::Loop(LoopAction::Start {
            interval_secs,
            prompt,
        })) => {
            assert_eq!(interval_secs, 300);
            assert_eq!(prompt, "check the build");
        }
        other => panic!("expected Loop::Start, got {other:?}"),
    }

    match registry
        .execute("/loop 30s tail logs", "/tmp", None, None)
        .expect("/loop 30s")
    {
        CommandOutput::Action(CommandAction::Loop(LoopAction::Start { interval_secs, .. })) => {
            assert_eq!(interval_secs, 30);
        }
        other => panic!("expected Loop::Start, got {other:?}"),
    }

    // Bare number = minutes.
    match registry
        .execute("/loop 2 standup", "/tmp", None, None)
        .expect("/loop 2")
    {
        CommandOutput::Action(CommandAction::Loop(LoopAction::Start { interval_secs, .. })) => {
            assert_eq!(interval_secs, 120);
        }
        other => panic!("expected Loop::Start, got {other:?}"),
    }

    match registry
        .execute("/loop stop", "/tmp", None, None)
        .expect("stop")
    {
        CommandOutput::Action(CommandAction::Loop(LoopAction::Stop)) => {}
        other => panic!("expected Loop::Stop, got {other:?}"),
    }

    match registry
        .execute("/loop", "/tmp", None, None)
        .expect("status")
    {
        CommandOutput::Action(CommandAction::Loop(LoopAction::Status)) => {}
        other => panic!("expected Loop::Status, got {other:?}"),
    }

    assert!(
        registry
            .execute("/loop nonsense", "/tmp", None, None)
            .is_err()
    );
    assert!(registry.execute("/loop 5m", "/tmp", None, None).is_err());
}

#[test]
fn parse_loop_interval_units() {
    assert_eq!(parse_loop_interval("30s"), Some(30));
    assert_eq!(parse_loop_interval("5m"), Some(300));
    assert_eq!(parse_loop_interval("1h"), Some(3600));
    assert_eq!(parse_loop_interval("2"), Some(120));
    // Minimum clamp: 10 seconds.
    assert_eq!(parse_loop_interval("1s"), Some(10));
    assert_eq!(parse_loop_interval("0m"), None);
    assert_eq!(parse_loop_interval("abc"), None);
}

#[test]
fn alerts_command_returns_show_alerts_action() {
    let registry = build_command_registry();
    let output = registry
        .execute("/alerts", "/tmp", None, None)
        .expect("alerts command");
    assert!(matches!(
        output,
        CommandOutput::Action(CommandAction::ShowAlerts)
    ));
}

#[test]
fn queue_command_parses_reorder_and_send_now_actions() {
    let registry = build_command_registry();

    assert!(matches!(
        registry
            .execute("/queue move 12 up", "/tmp", None, None)
            .expect("queue move"),
        CommandOutput::Action(CommandAction::Queue(QueueAction::Move {
            id: 12,
            direction: QueueMoveDirection::Up
        }))
    ));
    assert!(matches!(
        registry
            .execute("/queue send 12", "/tmp", None, None)
            .expect("queue send"),
        CommandOutput::Action(CommandAction::Queue(QueueAction::Move {
            id: 12,
            direction: QueueMoveDirection::Now
        }))
    ));
}

#[test]
fn computer_command_and_orb_alias_parse_native_task_controls() {
    let registry = build_command_registry();
    assert!(matches!(
        registry.execute("/computer list", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::Orb(OrbAction::List)))
    ));
    assert!(matches!(
        registry.execute("/computer status task-1", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::Orb(OrbAction::Status(id)))
            ) if id == "task-1"
    ));
    assert!(matches!(
        registry.execute("/computer followup task-1 check the build", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::Orb(OrbAction::Followup { id, prompt })))
            if id == "task-1" && prompt == "check the build"
    ));
    assert!(matches!(
        registry.execute(
            "/computer handoff create task-1 thread-2 --file src/lib.rs --include-diff",
            "/tmp",
            None,
            None
        ),
        Ok(CommandOutput::Action(CommandAction::Orb(OrbAction::HandoffCreate {
            source_id,
            target_thread_id,
            files,
            artifact_ids,
            include_diff,
        })))
            if source_id == "task-1"
                && target_thread_id == "thread-2"
                && files == vec!["src/lib.rs".to_string()]
                && artifact_ids.is_empty()
                && include_diff
    ));
    assert!(matches!(
        registry.execute(
            "/computer handoff read thread-2 package-1",
            "/tmp",
            None,
            None
        ),
        Ok(CommandOutput::Action(CommandAction::Orb(OrbAction::HandoffRead {
            target_thread_id,
            package_id,
        })))
            if target_thread_id == "thread-2" && package_id == "package-1"
    ));
    assert!(
        registry
            .execute("/computer pause", "/tmp", None, None)
            .is_err()
    );
    assert!(
        registry
            .execute(
                "/computer handoff create task-1 thread-2",
                "/tmp",
                None,
                None
            )
            .is_err()
    );
    assert!(matches!(
        registry.execute("/orb list", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::Orb(OrbAction::List)))
    ));
}

#[test]
fn handoff_command_parses_direct_and_computer_package_forms() {
    let registry = build_command_registry();
    assert!(matches!(
        registry.execute(
            "/handoff check the release queue",
            "/tmp",
            None,
            None
        ),
        Ok(CommandOutput::Action(CommandAction::A2a(A2aAction::Handoff {
            peer,
            text,
            computer_package: None,
        }))) if peer.is_none() && text == "check the release queue"
    ));
    assert!(matches!(
        registry.execute(
            "/handoff --peer chief check the release queue",
            "/tmp",
            None,
            None
        ),
        Ok(CommandOutput::Action(CommandAction::A2a(A2aAction::Handoff {
            peer,
            text,
            computer_package: None,
        }))) if peer.as_deref() == Some("chief") && text == "check the release queue"
    ));
    assert!(matches!(
        registry.execute(
            "/handoff --source-task task-1 --target-thread thread-2 --file src/lib.rs --artifact artifact-1 --include-diff -- continue the implementation",
            "/tmp",
            None,
            None
        ),
        Ok(CommandOutput::Action(CommandAction::A2a(A2aAction::Handoff {
            peer,
            text,
            computer_package: Some(A2aComputerHandoffSelection {
                source_task_id,
                target_thread_id,
                files,
                artifact_ids,
                include_diff,
            }),
        })))
            if peer.is_none()
                && text == "continue the implementation"
                && source_task_id == "task-1"
                && target_thread_id == "thread-2"
                && files == vec!["src/lib.rs".to_string()]
                && artifact_ids == vec!["artifact-1".to_string()]
                && include_diff
    ));
}

#[test]
fn handoff_command_rejects_missing_prompt_and_partial_computer_packages() {
    let registry = build_command_registry();
    assert!(registry.execute("/handoff", "/tmp", None, None).is_err());
    assert!(
        registry
            .execute("/handoff --peer", "/tmp", None, None)
            .is_err()
    );
    assert!(
        registry
            .execute(
                "/handoff --source-task task-1 --include-diff -- continue",
                "/tmp",
                None,
                None
            )
            .is_err()
    );
    assert!(
        registry
            .execute(
                "/handoff --source-task task-1 --target-thread thread-2 -- continue",
                "/tmp",
                None,
                None
            )
            .is_err()
    );
}

#[test]
fn rewind_both_and_context_exclusion_parse() {
    let registry = build_command_registry();
    assert!(matches!(
        registry.execute("/rewind 2 --files --dry-run", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::Session(
            SessionAction::RewindBoth {
                turns: 2,
                dry_run: true
            }
        )))
    ));
    assert!(
        matches!(registry.execute("/context exclude fixture_integration", "/tmp", None, None),
        Ok(CommandOutput::Action(CommandAction::SetContextTool { name, excluded: true })) if name == "fixture_integration")
    );
    assert!(
        registry
            .execute("/context exclude", "/tmp", None, None)
            .is_err()
    );
    assert!(
        registry
            .execute("/context include a b", "/tmp", None, None)
            .is_err()
    );
}

#[test]
fn dex_presentation_command_routes_to_ui_preferences() {
    let registry = build_command_registry();
    for setting in [
        "quiet",
        "standard",
        "expressive",
        "motion-on",
        "motion-off",
        "pet",
        "appearance",
        "recap",
        "next",
        "tips-off",
        "tips-on",
        "notifications-on",
        "notifications-off",
        "suggestions-on",
        "suggestions-off",
        "recap-on",
        "recap-off",
    ] {
        match registry
            .execute(&format!("/dex {setting}"), "/tmp", None, None)
            .unwrap()
        {
            CommandOutput::Action(CommandAction::SetDexPresentation(actual)) => {
                assert_eq!(actual, setting);
            }
            other => panic!("expected Dex presentation action, got {other:?}"),
        }
    }
    assert!(
        registry
            .execute("/dex unknown", "/tmp", None, None)
            .is_err()
    );
}

#[test]
fn worker_commands_preserve_target_and_exact_message() {
    let registry = build_command_registry();
    let output = registry
        .execute(
            "/workers steer subagent:fixture:2 keep the API; fix CLI",
            ".",
            None,
            None,
        )
        .unwrap();
    let CommandOutput::Action(CommandAction::Worker(action)) = output else {
        panic!("worker action expected")
    };
    let (tool, args) = action.tool_call();
    assert_eq!(tool, "control_subagent");
    assert_eq!(args["agent_ref"], "subagent:fixture:2");
    assert_eq!(args["message"], "keep the API; fix CLI");
    for invalid in [
        "/workers resume",
        "/workers resume id",
        "/workers cancel id another",
        "/workers list trailing",
    ] {
        assert!(
            registry.execute(invalid, ".", None, None).is_err(),
            "{invalid}"
        );
    }
}

#[test]
fn memory_commands_use_the_existing_reviewed_store_actions() {
    let registry = build_command_registry();
    assert!(
        matches!(registry.execute("/memory save proposal-1", ".", None, None).unwrap(),
        CommandOutput::Action(CommandAction::Harness(HarnessAction::Apply(id))) if id == "proposal-1")
    );
    assert!(
        matches!(registry.execute("/memory forget entry-1", ".", None, None).unwrap(),
        CommandOutput::Action(CommandAction::Harness(HarnessAction::Delete(id))) if id == "entry-1")
    );
    assert!(registry.execute("/memory save", ".", None, None).is_err());
}

#[test]
fn experiment_preferences_are_discoverable_and_invalid_consent_is_rejected() {
    let registry = build_command_registry();
    assert!(matches!(
        registry
            .execute("/preferences", "/tmp", None, None)
            .unwrap(),
        CommandOutput::Action(CommandAction::ShowPreferences)
    ));
    assert!(
        registry
            .execute("/experiments on off", "/tmp", None, None)
            .is_err()
    );
}
