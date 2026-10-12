# /// script
# requires-python = ">=3.11"
# dependencies = ["json5==0.12.1"]
# ///
"""Map pinned VS Code color assets into native themes; no extension code runs.

Normal execution is offline. --refresh downloads the pinned upstream inputs.
"""
import argparse
import json
import posixpath
import re
import urllib.request
from pathlib import Path

COMMIT = "d9637b3f2faa8f8ce0636e556291fdb1ba714c0b"
DEST = Path(__file__).resolve().parents[1] / "packages/tui-kit-rs/src/themes/vscode"


def rgba(value):
    if not isinstance(value, str) or not re.fullmatch(r"#[0-9a-fA-F]{3,8}", value):
        return None
    value = value[1:]
    if len(value) in (3, 4):
        value = "".join(c * 2 for c in value)
    if len(value) not in (6, 8):
        return None
    return tuple(int(value[i:i + 2], 16) for i in range(0, len(value), 2)) + ((255,) if len(value) == 6 else ())


def color(value, background):
    parsed = rgba(value)
    if parsed is None:
        return None
    r, g, b, a = parsed
    base = rgba(background)
    return "#" + "".join(f"{round(v * a / 255 + base[i] * (1 - a / 255)):02x}" for i, v in enumerate((r, g, b)))


def mix(a, b, amount):
    return "#" + "".join(f"{round(x * (1 - amount) + y * amount):02x}" for x, y in zip(rgba(a)[:3], rgba(b)[:3]))


def token(theme, scopes, fallback, background, semantic=()):
    chosen, rank = fallback, -1
    for rule in theme.get("tokenColors", []):
        selectors = rule.get("scope", [])
        if isinstance(selectors, str):
            selectors = selectors.split(",")
        for selector in selectors:
            selector = selector.strip()
            # Only project simple scopes; language/context-specific rules need a grammar.
            if not selector or any(c in selector for c in " -()|"):
                continue
            if any(scope == selector or scope.startswith(selector + ".") for scope in scopes):
                ink = color(rule.get("settings", {}).get("foreground"), background)
                if ink and len(selector) >= rank:
                    chosen, rank = ink, len(selector)
    for name in semantic:
        entry = theme.get("semanticTokenColors", {}).get(name)
        if isinstance(entry, dict):
            entry = entry.get("foreground")
        ink = color(entry, background)
        if ink:
            return ink
    return chosen


def map_theme(entry):
    theme = entry["theme"]
    colors = theme.get("colors", {})
    light = entry["uiTheme"] in ("vs", "hc-light")
    base = "#ffffff" if light else "#000000" if entry["uiTheme"] == "hc-black" else "#1e1e1e"
    canvas = color(colors.get("editor.background"), base) or base
    def pick(keys, fallback, bg=canvas):
        return next((c for key in keys if (c := color(colors.get(key), bg))), fallback)
    text = pick(["editor.foreground", "foreground"], "#333333" if light else "#d4d4d4")
    panel = pick(["input.background", "editorWidget.background", "sideBar.background"], mix(canvas, text, .06))
    accent = pick(["textLink.foreground", "focusBorder", "terminal.ansiBlue"], "#006ab1" if light else "#4daafc")
    muted = pick(["descriptionForeground", "editorLineNumber.foreground"], mix(canvas, text, .75))
    selection = pick(["list.inactiveSelectionBackground", "editor.selectionBackground", "list.activeSelectionBackground"], mix(canvas, accent, .20), panel)
    border = pick(["contrastBorder", "widget.border", "panel.border", "input.border"], mix(canvas, text, .3), panel)
    success = pick(["testing.iconPassed", "terminal.ansiGreen"], "#388a34" if light else "#89d185")
    error = pick(["editorError.foreground", "terminal.ansiRed"], "#b5200d" if light else "#f48771")
    warning = pick(["editorWarning.foreground", "terminal.ansiYellow"], "#895503" if light else "#cca700")
    syntax = {
        "comment": token(theme, ["comment.line", "comment.block"], muted, canvas),
        "keyword": token(theme, ["keyword.control", "storage.type"], accent, canvas, ["keyword"]),
        "function": token(theme, ["entity.name.function", "support.function"], accent, canvas, ["function", "method"]),
        "variable": token(theme, ["variable.other.readwrite", "variable"], text, canvas, ["variable"]),
        "string": token(theme, ["string.quoted.double", "string"], success, canvas, ["string"]),
        "number": token(theme, ["constant.numeric"], warning, canvas, ["number"]),
        "type": token(theme, ["entity.name.type", "support.type", "entity.name.class"], accent, canvas, ["type", "class"]),
    }
    mapped = dict(accent=accent, border=border, text=text, muted=muted, dim=muted,
                  success=success, error=error, warning=warning,
                  assistant_message_bg=canvas, assistant_message_text=text,
                  user_message_bg=panel, user_message_text=text,
                  tool_pending_bg=selection, tool_success_bg=panel, tool_error_bg=panel,
                  md_heading=token(theme, ["markup.heading"], accent, canvas),
                  md_link=accent, md_code=token(theme, ["markup.inline.raw"], syntax["string"], canvas),
                  md_code_block=panel, md_code_block_border=border, md_quote=muted,
                  thinking_off=muted, thinking_low=warning, thinking_medium=accent, thinking_high=accent)
    mapped.update({"syntax_" + key: value for key, value in syntax.items()})
    return {"name": entry["name"], "colors": mapped, "vars": {}}


def refresh():
    import json5
    def get(path):
        url = f"https://raw.githubusercontent.com/microsoft/vscode/{COMMIT}/{path}"
        with urllib.request.urlopen(url, timeout=30) as response:
            return response.read().decode()
    def resolve(path, stack=()):
        path = posixpath.normpath(path)
        if not path.startswith("extensions/") or path in stack:
            raise ValueError(f"Invalid or cyclic theme include: {path}")
        theme = json5.loads(get(path))
        parent = resolve(posixpath.join(posixpath.dirname(path), theme["include"]), (*stack, path)) if "include" in theme else {}
        if isinstance(theme.get("tokenColors"), str):
            raise ValueError(f"External TextMate file needs an explicit mapping: {path}")
        return {"colors": {**parent.get("colors", {}), **theme.get("colors", {})},
                "tokenColors": parent.get("tokenColors", []) + theme.get("tokenColors", []),
                "semanticTokenColors": {**parent.get("semanticTokenColors", {}), **theme.get("semanticTokenColors", {})}}
    url = f"https://api.github.com/repos/microsoft/vscode/contents/extensions?ref={COMMIT}"
    with urllib.request.urlopen(url, timeout=30) as response:
        directories = json.load(response)
    entries = []
    for directory in sorted(d["name"] for d in directories if d["name"].startswith("theme-")):
        root = f"extensions/{directory}"
        package = json.loads(get(root + "/package.json"))
        contributions = package.get("contributes", {}).get("themes", [])
        if not contributions:
            continue
        labels = json.loads(get(root + "/package.nls.json"))
        for contribution in contributions:
            label = contribution["label"]
            label = labels[label.strip("%")] if label.startswith("%") else label
            identity = contribution.get("id", label)
            name = "vscode-" + re.sub(r"[^a-z0-9]+", "-", identity.lower().replace("+", "-plus")).strip("-")
            path = posixpath.normpath(root + "/" + contribution["path"])
            entries.append({"name": name, "label": label, "path": path, "uiTheme": contribution["uiTheme"], "theme": resolve(path)})
    if len({entry["name"] for entry in entries}) != len(entries):
        raise ValueError("Duplicate mapped theme names")
    DEST.mkdir(parents=True, exist_ok=True)
    (DEST / "source.json").write_text(json.dumps({"repository": "https://github.com/microsoft/vscode", "commit": COMMIT, "themes": entries}, indent=2) + "\n")
    (DEST / "LICENSE.txt").write_text(get("LICENSE.txt"))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--refresh", action="store_true")
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if args.refresh:
        refresh()
    source = json.loads((DEST / "source.json").read_text())
    result = json.dumps([map_theme(entry) for entry in source["themes"]], indent=2) + "\n"
    target = DEST / "themes.json"
    if args.check:
        if target.read_text() != result:
            raise SystemExit("VS Code theme mappings are stale")
    else:
        target.write_text(result)
    print(f"Mapped {len(source['themes'])} VS Code color themes")


if __name__ == "__main__":
    main()
