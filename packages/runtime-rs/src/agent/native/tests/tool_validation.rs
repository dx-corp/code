//! Tool-name validation retains the native host ownership boundary.

use super::*;

#[test]
fn external_tool_names_reject_duplicates_and_reserved_names() {
    let client = UnifiedClient::Scripted(crate::ai::ScriptedClient::new(
        "runtime-test/tool-validation",
        Vec::new(),
    ));
    let mut host = RuntimeTestHost::new(".", client);
    host.reserved_tools.insert("runtime_reserved".to_owned());
    let host = NativeExecutionHostHandle::new(Arc::new(host));

    let duplicate = validate_tools_with_host(
        &host,
        None,
        &[
            external_tool_definition("caller_tool"),
            external_tool_definition("CALLER_TOOL"),
        ],
    )
    .expect_err("case-insensitive duplicate external names must be rejected");
    assert!(duplicate.to_string().contains("multiple owners"));

    let host_collision = validate_tools_with_host(&host, None, &[external_tool_definition("BASH")])
        .expect_err("case-insensitive host tool collision must be rejected");
    assert!(
        host_collision
            .to_string()
            .contains("host, MCP, or reserved tool")
    );

    let reserved =
        validate_tools_with_host(&host, None, &[external_tool_definition("RUNTIME_RESERVED")])
            .expect_err("reserved external name must be rejected");
    assert!(reserved.to_string().contains("host, MCP, or reserved tool"));
}
