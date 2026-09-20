# Native UI previews

This development-only executable renders the widgets in `maestro-presentation`.
Production never depends on this crate. The source stamp belongs to its build
script so changing preview inputs cannot invalidate the native TUI library.

From the public repository root (or `products/maestro` in Mono), run:

```sh
cargo run --locked -p maestro-ui-preview -- --list
cargo run --locked -p maestro-ui-preview -- --scene startup --width 100 --height 10
cargo test --locked -p maestro-ui-preview
```

The executable prints ANSI terminal previews. In Mono, the optional
`make maestro-ui-review MAESTRO_UI_OUTPUT=/tmp/dex-review` wrapper builds a
comparison gallery; use a new output directory for each run. That wrapper and
its baseline acceptance checks are internal tooling.

Add structural scenes in `src/lib.rs::catalog` and render them with existing
production widgets. Appearance scenes come directly from the product's `LOOKS`
catalog. Stable IDs identify actions; row order is tested separately by the
native keyboard fixtures. Keep runtime facts supplied by the caller and time
supplied by `ViewClock`. Avoid network clients, persistence, and runtime startup.

See [the screenshot workflow](../../docs/tui-screenshots.md) for native tmux
captures, manifest checks, baseline acceptance, and reproducible comparisons.

## Conversation components

`conversation-typing`, `conversation-streaming`, `conversation-error`,
`conversation-approval`, `conversation-queued`, and `conversation-completed`
render the same composer and tool-result widgets used by the native transcript.
Each appears at 40, 60, and 100 columns. The examples supply state; they do not
execute tools or grant approvals.

For focused terminal previews from the same directory:

```sh
for scene in conversation-typing conversation-streaming conversation-error conversation-approval; do
  cargo run --locked -p maestro-ui-preview -- --scene "$scene" --width 100 --height 10
done
```

These commands render individual scenes, not complete screenshot baselines.
Native before/after checks in Mono still use `capture-tui-suite.py`.

## Interactive state library

Generate a portable page from the same Rust buffers:

```sh
cargo run --locked -p maestro-ui-preview -- --html > ui-library.html
cargo run --locked -p maestro-ui-preview -- --json > ui-library.json
```

For first boot and all onboarding pages alongside the component catalog:

```sh
COLORTERM=truecolor cargo run --locked -p maestro-tui --example onboarding-preview -- --html > ui-library.html
cargo test --locked -p maestro-tui --example onboarding-preview
```

Open the HTML file directly. Search named scenes, select terminal dimensions,
step through supplied timestamps, play animation, and export a frame as JSON.
Nothing is fetched from a server. The fixtures do not sign in, call a model,
execute tools, or persist configuration. Playback starts only when requested.
RGB, indexed/default colors, style flags and Unicode column widths are retained;
blink flags remain static for inspection. Interaction scenes use 80 ms per
input, so every prefix remains an ordinary capture consumed by the PR evidence
pipeline.

`review::capture(scene, |frame| ...)` accepts a production rendering closure.
`review::from_buffer` adapts existing buffer-based widgets. `review::html` and
`review::json` export the same captures. Add fixtures beside the owning adapter,
using its public state transitions and explicit typed mock reports. Assert the
expected page before rendering; never dispatch effects returned by those
transitions. Use a fixed `Scene::time_ms` instead of sleeping or reading a clock.

The lightweight component executable retains its existing dependency boundary.
Only the onboarding example links the full TUI; its preview dependency is
**development-only**. Production code never imports this library. Browser fonts
are an inspection convenience, not a pixel baseline: continue to use the existing
`make maestro-ui-review` / `maestro-ui-accept` workflow for pinned-font PNG
comparisons and the native terminal suite for interaction regressions.

## Author a menu or a scene

Studio keeps story creation inside an adapter-owned fixture directory and
updates only its marker-bounded registration block. Preview a plan before any
write, then verify the registered story through the same result stream used by
the gallery and PR evidence:

```sh
cargo run --locked -p maestro-ui-preview -- studio new --adapter shared-menu workspace-picker --check
cargo run --locked -p maestro-ui-preview -- studio new --adapter shared-menu workspace-picker
cargo run --locked -p maestro-ui-preview -- studio verify workspace-picker
cargo run --locked -p maestro-ui-preview -- studio promote --adapter shared-menu path/to/menu-recipe.json --check
cargo run --locked -p maestro-ui-preview -- studio coverage
```

Promotion validates the recipe before writing, refuses existing fixtures,
runs a fixed adapter verifier, and restores the invocation-owned fixture and
registration edit if verification fails. Adapter manifests publish the owner,
supported story kinds, coverage profiles, source directory, and lifecycle.

The complete authoring loop is:

1. Run `studio new ... --check` and inspect the two owned paths.
2. Run `studio new`, edit the bounded fixture data, and run `studio verify ID`.
3. For a browser-authored menu recipe, save the envelope inside the repository,
   run `studio promote ... --check`, then run the same command without `--check`.
4. Run `studio coverage` and inspect the selected profile, declared inventory,
   final story states, adapter owner, and lifecycle diagnostics.

Generated modules compile both as registered modules and standalone examples.
Their registry entry carries the adapter ID, so coverage and contribution
checks cannot silently classify the story as unowned. Creation and promotion
use create-new writes: repeating either command refuses the existing fixture.
Promotion removes only its own fixture and marker entry if its fixed behavior
verifier fails, while preserving concurrent unrelated registration edits.
Record tree-keyed verification receipts only after committing the story and
documentation changes. Rerun them after any fix; stale receipts fail closed.

Generate a minimal runnable menu story without overwriting an existing file:

```sh
cargo run --locked -p maestro-ui-preview -- \
  --scaffold workspace-menu \
  --output packages/ui-preview-rs/examples/workspace_menu.rs
cargo run --locked -p maestro-ui-preview --example workspace_menu > workspace-menu.html
```

The generated example declares its labels, items, and typed interaction inputs,
then calls `menu_story` and `export_story`. Those helpers use the real shared
`ActionPicker` and `Menu`; product-specific adapters can use the lower-level
`Story::replay`. Move a finished story beside its owning adapter and add it once
to that adapter's registry. IDs are lowercase and at most 64 bytes. The
generator uses create-new semantics and refuses to overwrite a file.

Use `maestro_ui::Menu` for menu chrome, empty/loading/error presentation and help.
Keep one caller-owned `ActionPicker<T>` for query, selection and keyboard input:

```rust
let mut state = ActionPicker::new(items).searchable(String::as_str);
state.open();
// Render:
Menu::new("Choose workspace", &mut state).render(frame, area, theme);
// Input: route state.handle_key(...) to the application; dispatch no effects here.
```

`render_items` accepts custom descriptions or current-item markers. The production
ThemeSelector uses this same menu. It retains its existing preview/cancel/commit
behavior. This is a composition of existing primitives, not another controller.

Register new scene families once in `registry()`:

```rust
registry.add(Story::new("my-menu", "My menu", file!(), |scene, frame| {
    // Construct typed fixture state and call the production renderer here.
}).matrix(&[(40, 20), (60, 24), (100, 30)], &[0, 80, 160]))?;
```

The registry derives the CLI catalog, rendering dispatch, capture matrix and
source links. Duplicate IDs/cases and unbounded dimensions fail at registration.
`src/menus.rs` is a complete example with ready, filtered, empty, loading and
error fixtures. Older scene families enter through `Registry::import` while
retaining their stable IDs; new stories do not need a second rendering switch.
Use repository-relative source paths for links to GitHub; `file!()` is useful
for local source identification but may need a repository path prefix.

The browser groups states under their product experience, preserves
scene/size/frame URLs, links to renderer source, and supports a pinned
comparison. Comparison follows the selected timestamp using the nearest
available earlier frame. Its accessible preview is a line-oriented transcript;
hidden terminal glyphs stay in visual cell data but are omitted from that
transcript. Existing baseline acceptance remains explicit and is never
performed by the workbench.

## Watch while designing

From the Maestro workspace:

```sh
python3 scripts/dev/ui-workbench.py
# Faster, shared components only:
python3 scripts/dev/ui-workbench.py --components-only --port 8771
```

The command builds the real Rust renderer once, watches source changes, and serves
the generated page on loopback. In the **Interactive Rust replay** panel, focus
the terminal preview and type, paste, navigate, resize, cancel, or retry. The
browser sends a bounded typed sequence to a fixed Rust executable; the production
`ThemeSelector` controller and renderer rebuild the frame from the full sequence.
Export saves that portable JSON receipt and **Replay file** imports it. Effects
are returned as explicit simulated receipts. The preview never changes a theme,
signs in, executes a tool, or writes application settings.

The server rejects non-loopback Host/Origin values, oversized bodies, text,
event counts and dimensions. It invokes no browser-supplied command or path.
`--components-only` remains a static catalog and exposes no replay endpoint.

Choose **New menu scene** to open the Scene Inspector. Content edits keep stable
action IDs separate from labels and render through the same Rust `ActionPicker`
and `Menu` used by product code. State switches cover ready, loading, empty,
error, long-content, Unicode, and narrow fixtures. The Interaction tab replays
typing, navigation, selection, cancel, resize, and retry and reports simulated
effects by stable ID. The coverage table distinguishes available variants from
the variants exercised by the current draft; its automated-check column is
reported separately and is never inferred from a preview.

**Export recipe** saves the bounded declarative draft. **Export replay** saves
the ordinary deterministic input sequence consumed by capture tests. **Export
Rust fixture** emits a runnable Cargo example that uses the production widgets;
the caller still owns real action dispatch and persistence. Import validates a
recipe with Rust before replacing the current draft. Draft form fields live in
browser session storage so a source-watch reload preserves even a temporarily
invalid edit. This storage is editor state only and never changes application
settings.

To keep an exported scene, place its `.rs` file beside the owning preview
adapter. Its generated `register(&mut Registry)` function adds the recipe state
and every deterministic input prefix to the same registry used by `--json`,
`--html`, and PR visual evidence:

```rust
#[path = "workspace_menu.rs"]
pub mod exported_workspace_menu;

exported_workspace_menu::register(&mut registry)?;
```

Keep that generated module public when compiling with dead-code warnings: the
same file also exposes a public standalone `main` used by `cargo run --example`.

The built-in recipe states and four interaction journeys use this path. They
add 21 bounded cases, including select, filter/cancel, retry/select, and
Unicode/resize/cancel prefixes. Registration rejects duplicate IDs or invalid
dimensions; it never accepts a baseline.

The workbench preserves URL state and scroll across successful refreshes.
Failed builds retain the previous page and show an error; they never update
accepted baselines. It uses the configured `CARGO_TARGET_DIR` or a user cache
outside the checkout. In Mono it runs the existing build-capacity check before
each build. Ctrl+C stops serving.

## Coverage and browser modules

`studio coverage` emits the same registry-derived declared, visited, asserted,
passed, failed, skipped, and unavailable states embedded in the browser report,
along with adapter owner diagnostics. Legacy ownership is visible but is not an
authoring target. Browser helpers live in `src/web/`; Rust bundles them into the
single self-contained HTML artifact, while `node src/web/tests.mjs` exercises
their pure behavior without adding a JavaScript runtime to Maestro.

Before proposing a story change, run the crate tests, the onboarding story
tests, the workbench tests, and the browser module tests. PR evidence then
validates the same versioned captures independently; a gallery render alone is
never proof that controller expectations passed.
