(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  (root.MaestroUI ??= {}).workspace = api;
})(globalThis, () => {
  const layouts = ["browse", "design", "debug", "review"];

  function normalizeLayout(value) {
    return layouts.includes(value) ? value : "browse";
  }

  function resolveScene(requested, available) {
    const fallback = available[0] || "";
    if (!requested || available.includes(requested)) {
      return { selected: requested || fallback, missing: "" };
    }
    return { selected: fallback, missing: requested };
  }

  function routeScene(missing, selected) {
    return missing || selected;
  }

  function scopeSummary(update, availableCount) {
    const filter = update?.filter,
      filtered = Boolean(filter?.kind && filter?.value),
      noun = availableCount === 1 ? "SCENE" : "SCENES";
    let freshness = update ? "FRESH" : "STATIC";
    if (update?.building) freshness = update.stale ? "BUILDING — LAST GOOD" : "BUILDING";
    else if (update?.stale) freshness = "STALE — LAST GOOD";
    const scope = filtered
      ? `FILTERED · ${filter.kind.toUpperCase()} ${filter.value}`
      : "ALL EXPERIENCES";
    return {
      label: `${scope} · ${availableCount} ${noun} · ${freshness}`,
      filtered,
    };
  }

  function provenance(update) {
    if (!update) return "Static artifact · server provenance unavailable";
    const source = update.source || {},
      revision = source.revision ? source.revision.slice(0, 10) : "unknown",
      checkout = source.branch ? `${source.branch}@${revision}` : revision,
      sourceDigest = update.source_digest ? update.source_digest.slice(0, 10) : "unknown",
      rendererDigest = update.renderer_digest ? update.renderer_digest.slice(0, 10) : "unknown",
      generation = update.generation ? update.generation.slice(0, 10) : "none";
    return `${source.repository || "workspace"} · ${checkout} · source ${sourceDigest} · renderer ${rendererDigest} · generation ${generation} · ${update.build_ms || 0} ms`;
  }

  return { layouts, normalizeLayout, resolveScene, routeScene, scopeSummary, provenance };
});
