(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  (root.MaestroUI ??= {}).catalog = api;
})(globalThis, () => {
  const experienceOrder = ["First launch", "Onboarding", "Conversation", "Workspace menu", "Theme selector", "Appearance picker", "Companion appearance", "Other scenes"];

  function unwrap(value, schema) {
    if (!value || typeof value !== "object" || !("schema" in value)) return value;
    const keys = Object.keys(value).sort().join(",");
    if (keys !== "schema,value,version" || value.schema !== schema || value.version !== 1) throw new Error(`Unsupported ${schema} envelope`);
    return value.value;
  }

  function group(id) {
    if (id.startsWith("first-boot")) return "First launch";
    if (id.startsWith("onboarding-")) return "Onboarding";
    if (id.startsWith("conversation-")) return "Conversation";
    if (id.startsWith("menu-")) return "Workspace menu";
    if (id.startsWith("theme-selector")) return "Theme selector";
    if (id.startsWith("picker")) return "Appearance picker";
    if (["startup", "header", "quiet", "motion-off", "working", "finished", "failed", "needs-input", "pet"].includes(id) || id.startsWith("accessory-") || id.startsWith("accent-")) return "Companion appearance";
    return "Other scenes";
  }

  function stateLabel(capture) {
    if (capture.scene.id.startsWith("onboarding-")) {
      const id = capture.scene.id.slice(11);
      return ({ welcome: "Welcome", role: "Role", "use-case": "Use case", workflow: "Workflow", connect: "Connect", verify: "Verify connection", checking: "Checking", ready: "Results · ready", failed: "Results · failed", waiting: "Waiting", provider: "Provider", key: "API key" })[id] || capture.scene.label;
    }
    return capture.scene.label.replace(/^.*?\s[\/·]\s/, "");
  }

  function options(element, entries, doc = document) {
    const prior = element.value;
    element.replaceChildren(...entries.map(([value, label]) => {
      const option = doc.createElement("option");
      option.value = value;
      option.textContent = label;
      return option;
    }));
    if (entries.some(([value]) => value === prior)) element.value = prior;
  }

  function sceneEntries(captures, query = "") {
    const needle = query.toLowerCase();
    const entries = new Map();
    for (const capture of captures) {
      if (`${capture.scene.id} ${capture.scene.label}`.toLowerCase().includes(needle)) entries.set(capture.scene.id, `${capture.scene.label} / ${capture.scene.id}`);
    }
    return [...entries];
  }

  function sizeEntries(captures, sceneId) {
    const entries = new Map();
    for (const capture of captures) {
      if (capture.scene.id === sceneId) entries.set(`${capture.scene.width}x${capture.scene.height}`, `${capture.scene.width} × ${capture.scene.height}`);
    }
    return [...entries];
  }

  function grouped(entries) {
    const groups = new Map();
    for (const [id] of entries) {
      const name = group(id);
      if (!groups.has(name)) groups.set(name, []);
      groups.get(name).push(id);
    }
    return groups;
  }

  function activeGroup(groups, sceneId) {
    const current = group(sceneId);
    return groups.has(current) ? current : experienceOrder.find((name) => groups.has(name));
  }

  function coverageLabel(state) {
    return ({ declared: "Declared", visited: "Visited", "asserted-passed": "Asserted · passed", "asserted-failed": "Asserted · failed", skipped: "Skipped", unavailable: "Unavailable" })[state] || "Unknown";
  }

  return { unwrap, group, stateLabel, options, sceneEntries, sizeEntries, grouped, activeGroup, coverageLabel, experienceOrder };
});
