(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  (root.MaestroUI ??= {}).rendering = api;
})(globalThis, () => {
  const colors = ["#000000", "#800000", "#008000", "#808000", "#000080", "#800080", "#008080", "#c0c0c0", "#808080", "#ff0000", "#00ff00", "#ffff00", "#0000ff", "#ff00ff", "#00ffff", "#ffffff"];
  function color(value, fallback) {
    if (value === "reset") return fallback;
    if (!value.startsWith("indexed:")) return value;
    let index = Number(value.slice(8));
    if (index < 16) return colors[index];
    if (index >= 232) { const channel = 8 + (index - 232) * 10; return `rgb(${channel},${channel},${channel})`; }
    index -= 16;
    const ramp = [0, 95, 135, 175, 215, 255];
    return `rgb(${ramp[Math.floor(index / 36)]},${ramp[Math.floor(index / 6) % 6]},${ramp[index % 6]})`;
  }
  function glyph(cell) { return cell.modifiers & 128 ? "" : cell.text; }
  function draw(screen, capture, doc = document) {
    screen.replaceChildren();
    const { width, height } = capture.scene;
    for (let y = 0; y < height; y++) {
      for (let x = 0; x < width;) {
        const cell = capture.cells[y * width + x], span = doc.createElement("span"), mark = cell.modifiers;
        let foreground = color(cell.foreground, "#efebff"), background = color(cell.background, "#171624");
        if (mark & 64) [foreground, background] = [background, foreground];
        span.className = "cell";
        const text = doc.createElement("span");
        text.textContent = cell.text; text.style.opacity = mark & 2 ? ".55" : "1"; text.style.visibility = mark & 128 ? "hidden" : "visible";
        span.append(text);
        span.style.cssText = `width:${cell.columns}ch;color:${foreground};background:${background};font-weight:${mark & 1 ? "700" : "400"};font-style:${mark & 4 ? "italic" : "normal"};text-decoration:${[mark & 8 ? "underline" : "", mark & 256 ? "line-through" : ""].filter(Boolean).join(" ") || "none"}`;
        screen.append(span); x += cell.columns;
      }
      screen.append(doc.createTextNode("\n"));
    }
  }
  function readable(capture) {
    const lines = [];
    for (let y = 0; y < capture.scene.height; y++) {
      let line = "";
      for (let x = 0; x < capture.scene.width;) { const cell = capture.cells[y * capture.scene.width + x]; line += cell.modifiers & 128 ? " ".repeat(cell.columns) : cell.text; x += cell.columns; }
      lines.push(line.trimEnd());
    }
    while (lines.at(-1) === "") lines.pop();
    return lines.join("\n");
  }
  function semanticStatus(result) { if (!result) return "Not asserted"; return result.expectations?.some((item) => !item.passed) ? "Failed" : "Passed"; }
  function samples(values, limit = 3) { return { values: values.slice(0, limit), truncated: values.length > limit }; }
  function sourceUrl(path) { return path ? `https://github.com/dx-corp/mono/blob/main/${path.split("/").map(encodeURIComponent).join("/")}` : ""; }
  return { color, glyph, draw, readable, semanticStatus, samples, sourceUrl };
});
