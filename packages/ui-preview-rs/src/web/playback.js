(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  (root.MaestroUI ??= {}).playback = api;
})(globalThis, () => {
  function acceptsGeneration(active, incoming) { return !active || active === incoming; }
  function selectedFrames(captures, scene, size) {
    return captures.filter((item) => item.scene.id === scene && `${item.scene.width}x${item.scene.height}` === size).sort((left, right) => left.scene.time_ms - right.scene.time_ms);
  }
  function comparisonFrame(captures, pinned, time) {
    if (!pinned) return null;
    const frames = captures.filter((capture) => capture.scene.id === pinned.id && capture.scene.width === pinned.width && capture.scene.height === pinned.height).sort((left, right) => left.scene.time_ms - right.scene.time_ms);
    return frames.filter((capture) => capture.scene.time_ms <= Number(time)).at(-1) || frames[0] || null;
  }
  function routeString(scene, size, time, pinned) {
    const params = new URLSearchParams({ scene, size, time });
    if (pinned) { params.set("compare", pinned.id); params.set("compareSize", `${pinned.width}x${pinned.height}`); }
    return `#${params.toString()}`;
  }
  function route(params, allowed) {
    const result = {};
    for (const key of ["scene", "size", "time", "mode"]) if (allowed[key]?.includes(params[key])) result[key] = params[key];
    return result;
  }
  function nextDelay(frames, index) {
    if (index + 1 < frames.length) return frames[index + 1].scene.time_ms - frames[index].scene.time_ms;
    if (frames.length > 1) return frames[1].scene.time_ms - frames[0].scene.time_ms;
    return 80;
  }
  return { acceptsGeneration, selectedFrames, comparisonFrame, routeString, route, nextDelay };
});
