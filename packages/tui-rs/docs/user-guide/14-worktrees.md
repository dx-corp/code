# Worktrees

Deixic Code can run a whole session inside a fresh git worktree, Droid-style: pass `-w <name>` (or `--worktree <name>`) and the agent works on a new branch in a sibling checkout, isolated from your main tree.

---

## Usage

```bash
# Interactive TUI in a worktree
maestro -w feat-x "implement feature X"
maestro --worktree feat-x "implement feature X"

# Exec and print modes work the same way
maestro exec -w feat-x "implement feature X"
maestro -w feat-x -p "summarize this repo"
```

Native binary equivalent:

```bash
maestro -w feat-x
maestro --worktree=feat-x "prompt"
```

The name is required; it is sanitized into a valid git branch name (spaces, `/`, and other invalid characters become `-`).

---

## Layout and lifecycle

Worktrees are created next to the repository, not inside it:

```text
../<repo-name>-wt-<name>/   (branch: <sanitized name>, created from HEAD)
```

- Requires a git repository; fails cleanly outside one.
- Fails with a clear message if the branch or target path already exists.
- The entire session — agent tools, sessions, hooks, config — runs with the worktree as its working directory.
- On exit, a clean worktree (no uncommitted changes, no untracked files) is removed and its branch deleted. If the branch picked up commits, the worktree is removed but the branch is kept.
- A dirty worktree is kept, and Deixic Code prints its path and branch so you can find it.

---

## When to use

| Scenario | Why |
|----------|-----|
| Parallel experiments | Keep dirty trees out of your main checkout |
| Long agent tasks | Isolate file writes and branch state |
| Review / try a risky change | A clean tree disappears on exit; a dirty one is reported |

Manual cleanup of a kept worktree (standard git):

```bash
git worktree list
git worktree remove ../<repo-name>-wt-<name>
git branch -D <name>
```

---

## Interaction with sessions and config

- Sessions key off the worktree cwd, so transcripts stay per-worktree path.
- Hooks, MCP, and skills resolve from the active cwd’s project files plus user home config.

---

## Tips

- Name worktrees after branches or tickets (`feat-login`, `fix-1234`).
- Do not commit secrets into worktree-only scratch files; treat them like normal git checkouts.
