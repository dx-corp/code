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

The browser has grouped thumbnails, scene/size/frame URLs, renderer links and a
pinned comparison. Comparison follows the selected timestamp using the nearest
available earlier frame. Existing baseline acceptance remains explicit and is
never performed by the workbench.

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
The workbench preserves URL state
and scroll across successful refreshes. Failed builds retain the previous page
and show an error; they never update accepted baselines. It uses the configured
`CARGO_TARGET_DIR` or a user cache outside the checkout. In Mono it runs the
existing build-capacity check before each build. Ctrl+C stops serving.
