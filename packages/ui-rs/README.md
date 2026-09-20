# Deixic Code terminal UI library

`maestro-ui` provides reusable native `ratatui` controls. It uses
`maestro-interaction` for typed actions and selection, and `crossterm` for key
codes. It does not start a terminal, call a provider, or save application state.

## Start with a complete picker

Use `ActionPicker<T>` for a searchable list with keyboard navigation,
scrolling, confirmation and cancellation. Supply your items, identity function
and row renderer. Keep the picker in the application's existing modal owner.

```rust
use maestro_ui::{ActionPicker, PickerOutcome};

let mut picker = ActionPicker::new(vec!["dark", "light"])
    .identified_by(|name| *name)?
    .searchable(|name| *name);
picker.open();
picker.select_id("light");
```

The complete [action picker example](examples/action_picker.rs) supplies the
palette, renders the control, feeds keyboard input and handles its typed result:

```sh
cargo run -p maestro-ui --example action_picker --locked
cargo test -p maestro-ui --locked
```

Route keys through `handle_key(code, ctrl)` and paste through `insert_str(text)`.
Handle both returned outcomes through the same application function:

| Outcome | Application responsibility |
| --- | --- |
| `Changed(Some(item))` | Preview the highlighted choice without saving it. |
| `Changed(None)` | Clear the preview when no result remains. |
| `Selected(item)` | Apply or save once; the picker has closed. |
| `Cancelled` | Restore the opening preview state; the picker has closed. |
| `Pending` | No selection effect. |

Dex derives its preview from saved preferences plus the highlighted appearance.
Theme selection keeps the opening palette in memory and restores it on cancel.
The picker never owns those effects. Opening resets the search; call `select_id`
afterward to highlight the current value. Unknown or filtered-out IDs do nothing.

## Keep changing lists predictable

`identified_by` rejects duplicate IDs. IDs must identify the actual item, such
as a provider-qualified model, and must stay stable across refreshes. Filtering
and `replace_items` preserve the selected item when it remains visible; removal
selects the first remaining row. Replacement validates before mutation, so a
duplicate-ID error leaves the old list intact. Handle its returned `Changed`
outcome to refresh a preview, including changed data under an existing ID.

Default search matches text anywhere in a label without distinguishing case. Use `matching` for a
custom predicate; it receives the query exactly as typed. For product-specific
ordering or special rows, compute ordered results in the existing owner and
feed them through `replace_items`. The model selector uses this approach for
its focused list, discovery updates and “show all” row.

Use `set_status(PickerStatus::Loading(message))` or `Error(message)` to replace
results with a message. Enter cannot confirm hidden results in these states;
Escape still cancels. Set `Ready` when the owner has usable results. Requests,
generation checks and retries remain with the application.

## Compose lower-level controls when needed

| Control | Use it when |
| --- | --- |
| `ActionPicker` | You want shared input, selection, search and scrolling. |
| `Picker` | An existing controller already owns those mechanics. |
| `Modal` | You need a centered, clipped surface with a content rectangle. |
| `SearchField` | You only need to draw a borrowed query and cursor. |
| `SettingsForm` | You render grouped fields and owner-supplied validation. |

Pass `UiTheme` each frame; controls do not read global theme state.
`PickerOptions` supplies placeholder and empty-result copy. Generated help uses
the shared keyboard bindings; `help_text` can replace it when the host handles
additional shortcuts. `position_when_clipped` adds the selected position to
plain-list help in a short terminal. Row styling remains with the caller.

Render inside the rectangle returned by `Modal::render`. Empty rectangles are
valid. Exercise narrow and offset panes, long Unicode queries, no results,
loading/errors and scrolling with `TestBackend` and the native visual tools.

## Review the real controls visually

From the public repository root, the quickest loop is:

```sh
./dev ui review --adapter shared-menu
./dev ui inspect menu-ready
./dev ui check menu-ready --require-contract
```

The inspection result names the production source, owning adapter, complete
case matrix, behavior-contract state, and copyable review commands. The live
workbench uses the same Rust buffers as pull request evidence and never accepts
a baseline automatically.

From `products/maestro`, use the existing capture suite with a debug `maestro`
binary. Choose a new output directory for each review:

```sh
uv run scripts/capture-tui-suite.py --binary target/debug/maestro \
  --output /tmp/picker-review --case theme-picker --case theme-picker-empty \
  --case theme-picker-long-query --case dex-appearance-picker-scrolled \
  --case theme-preview-cancel --case dex-preview-cancel --case dex-preview-save
```

Each selected scenario runs at all its declared terminal sizes. The suite
writes `gallery.md`, native screenshots and machine-readable results. It
exercises the actual application controls, including preview cancellation and
saving, through local scripted fixtures. It needs no provider credentials.
Pass `--font` for the reviewed `Menlo` font when comparing image baselines.
The `TestBackend` regressions separately cover loading/error state and duplicate
list updates without launching the full application.

## Shared visual defaults

Choose `ModalSize::Compact` (54 × 16) for short decisions,
`Standard` (72 × 22) for searchable lists, or `Wide` (80 × 25) for
longer details. Sizes include borders and shrink within the parent. Call
`Modal::sized(title, size).theme(theme)` each frame: the shared decoration uses
the supplied border and surface colors, a bold primary-text title, and one
cell of horizontal content padding. `Modal::new` remains available for measured
content heights. `margin`, `border_style`, and `block` remain explicit overrides;
apply overrides after `theme`. `render_buffer` provides the same decoration
inside a `Ratatui` `Widget` implementation.

`UiTheme::text_style` and `muted_style` establish text hierarchy against the
current surface. Lists use `SELECTION_MARKER` plus `selection_style`: bold
emphasis preserves semantic foreground colors, so a selected invalid setting
still reads as an error. Do not introduce a second palette or global theme.

Use typed hints for actual bindings handled by the screen:

```rust
use maestro_ui::{KeyHint, Modal, ModalSize, Notice, NoticeTone, UiTheme, key_hints};

let theme = UiTheme::default(); // In the application, pass the current palette.
let surface = Modal::sized("Select session", ModalSize::Standard).theme(theme);
let help = key_hints(&[
    KeyHint::new("↑↓", "navigate"),
    KeyHint::new("Enter", "open"),
    KeyHint::new("Esc", "close"),
], theme);
let error = Notice::themed("Session could not be opened", NoticeTone::Error, theme);
```

The returned hint line borrows the key and label strings, not the hint slice;
temporary slices are safe. Keys use the focus color and bold emphasis, labels
use muted text, and actions are separated by ` · `. In `PickerOptions`, `hints`
takes precedence over `help_text`, then generated help. Existing custom wording
and separators in `PickerHelp` still apply to generated help. Use `Picker::help`
or `SettingsForm::help` to supply a `key_hints` line to a lower-level renderer.

Use `NoticeTone::{Neutral, Busy, Success, Attention, Error}` for explicit
application-owned states. `Picker::notice` replaces rows with the same styled
notice; `ActionPicker` chooses Busy for Loading and Error for Error. Notices
never start timers, infer outcomes, or repeat a successful state already visible
in the selected object. `SearchField::theme` gives standalone search fields the
same entered-text, placeholder and border colors as `Picker`.

## Shared theme surfaces

Pass one `UiTheme` from the application. `surface` is the canvas; optional
`panel` colors editors and dialogs, and optional `selection` colors selected
rows while preserving success, attention and error foregrounds. Omitted fields
retain the existing surface. Use `..UiTheme::default()` for palettes that do not
need layers. `theme.on_panel()`, `text_style()`, `muted_style()` and
`selection_style()` keep child controls consistent without global state.

The presentation crate's `ThemePreview(theme)` renders the same sample and real
composer across palettes. `DexCompanion::theme(Some(theme))` matches its portrait
and label to the palette without changing activity or motion. `None` preserves
the caller's chosen cosmetic accent. Native opaque themes supply the palette;
the existing transparent dark theme retains its cosmetic colors.

Native `/theme` includes `green` / `green-dark`, `pink` / `pink-dark`, and
`blue` / `blue-dark`. These are custom gentle palettes. Escape restores the
opening theme; Enter saves the highlighted choice using the existing settings.
The true-color regression checks text and status contrast on all layered
surfaces; limited-color tests check foreground/background separation. Actual
16-color RGB values remain terminal-defined.
