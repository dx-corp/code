# Dex loop source

| Directory | Responsibility |
| --- | --- |
| `contracts/` | Events, host ports, and voice contracts. |
| `context/` | History reconstruction, page context, compaction, and summaries. |
| `execution/` | Engine, budgets, and codemode state. |
| `safety/` | Content policy and sanitization. |
| `testing/` | Feature-gated in-memory host adapters. |

`lib.rs` declares the modules and retains the existing imports and re-exports.
