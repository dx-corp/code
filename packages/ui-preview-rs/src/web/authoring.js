(function (root, factory) {
  const api = factory();
  if (typeof module === "object" && module.exports) module.exports = api;
  (root.MaestroUI ??= {}).authoring = api;
})(globalThis, () => {
  const fields = ["id", "label", "title", "placeholder", "empty", "items", "state", "message", "width", "height"];
  function isolateDraft(candidate) {
    if (!candidate || typeof candidate !== "object" || Array.isArray(candidate)) throw new Error("Invalid draft");
    return structuredClone(candidate);
  }
  function wire(schema, value) { return { schema, version: 1, value }; }
  function unwrap(candidate, schema) {
    if (!candidate || typeof candidate !== "object" || !("schema" in candidate)) return candidate;
    if (Object.keys(candidate).sort().join(",") !== "schema,value,version" || candidate.schema !== schema || candidate.version !== 1) throw new Error(`Unsupported ${schema} envelope`);
    return candidate.value;
  }
  function recipeValues(recipe) {
    return { id: recipe.id, label: recipe.label, title: recipe.title, placeholder: recipe.placeholder, empty: recipe.empty, items: recipe.items.map((item) => `${item.id} | ${item.label}`).join("\n"), state: recipe.state, message: recipe.status_message, width: String(recipe.width), height: String(recipe.height) };
  }
  function readRecipe(request, get) {
    const items = get("items").value.split(/\r?\n/).filter((line) => line.trim()).map((line) => {
      const separator = line.indexOf("|");
      if (separator < 1) throw new Error("Each item needs a stable-id | Display label.");
      return { id: line.slice(0, separator).trim(), label: line.slice(separator + 1).trim() };
    });
    return { ...request, id: get("id").value, label: get("label").value, title: get("title").value, placeholder: get("placeholder").value, empty: get("empty").value, items, state: get("state").value, status_message: get("message").value, width: Number(get("width").value), height: Number(get("height").value) };
  }
  function formValues(get) { return Object.fromEntries(fields.map((name) => [name, get(name).value])); }
  function applyForm(values, get) { for (const name of fields) if (typeof values[name] === "string") get(name).value = values[name]; }
  function addInputs(request, inputs, limit = 64) {
    if (request.inputs.length + inputs.length > limit) throw new Error(`Sequence limit reached (${limit} inputs).`);
    request.inputs.push(...inputs);
    return request;
  }
  function coverageCells(row) {
    return [row.variant, row.availability.replaceAll("-", " "), row.exercised ? "Exercised" : "Not exercised", row.automated_check === null ? "Not reported" : row.automated_check ? "Covered" : "Missing"];
  }
  function boundedSamples(values, limit = 3) { return values.slice(0, limit); }
  return { isolateDraft, wire, unwrap, recipeValues, readRecipe, formValues, applyForm, addInputs, coverageCells, boundedSamples };
});
