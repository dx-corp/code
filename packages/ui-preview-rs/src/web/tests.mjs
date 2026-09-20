import assert from "node:assert/strict";
await import("./catalog.js");
await import("./playback.js");
await import("./authoring.js");
await import("./rendering.js");
await import("./workspace.js");
const { catalog, playback, authoring, rendering, workspace } = globalThis.MaestroUI;

const hiddenCapture = {
  scene: { id: "menu-ready", label: "Menu / Ready", width: 2, height: 1, time_ms: 0 },
  cells: [
    { text: "x", modifiers: 128, columns: 1 },
    { text: "y", modifiers: 0, columns: 1 },
  ],
};
assert.equal(rendering.glyph(hiddenCapture.cells[0]), "");
assert.equal(rendering.readable(hiddenCapture), " y");
assert.equal(rendering.semanticStatus({ expectations: [{ passed: false }] }), "Failed");
assert.equal(rendering.semanticStatus({ expectations: [{ passed: true }] }), "Passed");

assert.equal(playback.acceptsGeneration("g2", "g1"), false);
assert.equal(playback.acceptsGeneration("g2", "g2"), true);
const frames = [
  { scene: { id: "menu-ready", width: 40, height: 20, time_ms: 80 } },
  { scene: { id: "menu-ready", width: 40, height: 20, time_ms: 0 } },
];
assert.deepEqual(playback.selectedFrames(frames, "menu-ready", "40x20").map((frame) => frame.scene.time_ms), [0, 80]);
assert.equal(playback.comparisonFrame(frames, { id: "menu-ready", width: 40, height: 20 }, 40).scene.time_ms, 0);
assert.equal(playback.routeString("menu ready", "40x20", "80", { id: "base", width: 40, height: 20 }), "#scene=menu+ready&size=40x20&time=80&compare=base&compareSize=40x20");
assert.equal(playback.routeString("menu-ready", "40x20", "80", null, "debug"), "#scene=menu-ready&size=40x20&time=80&layout=debug");
assert.deepEqual(playback.route({ scene: "a", mode: "bad" }, { scene: ["a"], mode: ["side"] }), { scene: "a" });
assert.deepEqual(playback.route({ scene: "a", layout: "review" }, { scene: ["a"], layout: workspace.layouts }), { scene: "a", layout: "review" });

assert.equal(workspace.normalizeLayout("unknown"), "browse");
assert.equal(workspace.normalizeLayout("design"), "design");
assert.deepEqual(workspace.resolveScene("missing", ["ready", "error"]), { selected: "ready", missing: "missing" });
assert.deepEqual(workspace.resolveScene("error", ["ready", "error"]), { selected: "error", missing: "" });
assert.equal(workspace.routeScene("does-not-exist", "ready"), "does-not-exist");
assert.equal(workspace.routeScene("", "ready"), "ready");
assert.deepEqual(workspace.scopeSummary({ filter: { kind: "story", value: "first-boot" }, building: false, stale: false }, 1), {
  label: "FILTERED · STORY first-boot · 1 SCENE · FRESH",
  filtered: true,
});
assert.deepEqual(workspace.scopeSummary({ filter: { kind: "", value: "" }, building: true, stale: true }, 70), {
  label: "ALL EXPERIENCES · 70 SCENES · BUILDING — LAST GOOD",
  filtered: false,
});
assert.deepEqual(workspace.scopeSummary(null, 70), {
  label: "ALL EXPERIENCES · 70 SCENES · STATIC",
  filtered: false,
});

const captures = [{ scene: { id: "menu-ready", label: "Menu / Ready", width: 40, height: 20 } }];
assert.deepEqual(catalog.sceneEntries(captures, "READY"), [["menu-ready", "Menu / Ready / menu-ready"]]);
assert.deepEqual(catalog.sizeEntries(captures, "menu-ready"), [["40x20", "40 × 20"]]);
assert.deepEqual([...catalog.grouped([["menu-ready", "Ready"]])], [["Workspace menu", ["menu-ready"]]]);
assert.equal(catalog.coverageLabel("asserted-passed"), "Asserted · passed");
assert.equal(catalog.coverageLabel("skipped"), "Skipped");
assert.deepEqual(catalog.unwrap({ schema: "maestro.ui.replay", version: 1, value: { id: "x" } }, "maestro.ui.replay"), { id: "x" });

assert.deepEqual(rendering.samples([1, 2, 3, 4], 3), { values: [1, 2, 3], truncated: true });
assert.deepEqual(authoring.boundedSamples([1, 2, 3, 4], 2), [1, 2]);
assert.throws(() => authoring.isolateDraft(null), /Invalid draft/);
const fields = Object.fromEntries(["id", "label", "title", "placeholder", "empty", "items", "state", "message", "width", "height"].map((name) => [name, { value: "" }]));
fields.items.value = "missing separator";
assert.throws(() => authoring.readRecipe({ inputs: [] }, (name) => fields[name]), /stable-id/);
assert.deepEqual(authoring.coverageCells({ variant: "error", availability: "fixture-only", exercised: false, automated_check: null }), ["error", "fixture only", "Not exercised", "Not reported"]);
assert.throws(() => authoring.addInputs({ inputs: [1, 2] }, [3], 2), /Sequence limit/);

console.log("35 browser module assertions passed");
