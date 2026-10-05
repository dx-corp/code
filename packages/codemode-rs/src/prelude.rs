pub(crate) const PRELUDE: &str = r#"
(() => {
  const call = globalThis.__host_call;
  const emit = globalThis.__host_text;
  const catalog = JSON.parse(globalThis.__catalog);
  delete globalThis.__host_call;
  delete globalThis.__host_text;
  delete globalThis.__catalog;
  const search = globalThis.__host_search;
  const describe = globalThis.__host_describe;
  const describeNamespace = globalThis.__host_namespace;
  const schema = globalThis.__host_schema;
  const writeStore = globalThis.__host_store;
  const values = new Map(Object.entries(JSON.parse(globalThis.__store)));
  const emitImage = globalThis.__host_image;
  const modelCall = globalThis.__host_model;
  const metadata = globalThis.__host_metadata;
  const availableModels = globalThis.__host_models;
  for (const name of ["__host_search","__host_describe","__host_namespace","__host_schema","__host_store","__store","__host_image","__host_model","__host_metadata","__host_models"]) delete globalThis[name];
  const copied = value => JSON.parse(JSON.stringify(value));
  const pending = new Map();
  const tools = Object.create(null);
  const identifiers = new Set();
  const metadataValues = new WeakMap();
  const metadataGetters = ["description","schema","output_schema","namespace","model_operation","model_binding"].map(field => [field,function() {
    let values = metadataValues.get(this);
    if (!values) { values = Object.create(null); metadataValues.set(this,values); }
    if (!Object.prototype.hasOwnProperty.call(values,field)) values[field] = JSON.parse(metadata(this.name,field));
    return values[field];
  }]);
  for (const tool of catalog) {
    delete tool.namespace_instructions;
    let identifier = tool.name.replace(/[^a-zA-Z0-9_$]/gu, "_");
    if (!/^[a-zA-Z_$]/.test(identifier)) identifier = "_" + identifier;
    if (identifiers.has(identifier)) throw new Error("ambiguous tool identifier: " + identifier);
    identifiers.add(identifier);
    const invoke = args => new Promise((resolve, reject) => {
      const index = call(tool.name, JSON.stringify(args));
      pending.set(index, { resolve, reject });
    });
    Object.defineProperty(tools, tool.name, { value: invoke });
    if (identifier !== tool.name) Object.defineProperty(tools, identifier, { value: invoke });
    for (const [field,get] of metadataGetters) Object.defineProperty(tool,field,{enumerable:true,get});
    tool.identifier = identifier;
    Object.freeze(tool);
  }
  const text = value => emit(typeof value === "string" ? value : (JSON.stringify(value) ?? String(value)));
  Object.defineProperty(globalThis, "tools", { value: Object.freeze(tools) });
  Object.defineProperty(globalThis, "ALL_TOOLS", { value: Object.freeze(catalog) });
  Object.defineProperty(globalThis, "text", { value: text });
  Object.defineProperty(globalThis, "image", { value: value => emitImage(JSON.stringify(value)) });
  Object.defineProperty(globalThis, "searchTools", { value: (query,options={}) => JSON.parse(search(query,JSON.stringify(options))) });
  Object.defineProperty(globalThis, "describeTool", { value: name => JSON.parse(describe(name)) ?? undefined });
  Object.defineProperty(globalThis, "getToolSchema", { value: (name,options={}) => JSON.parse(schema(name,JSON.stringify(options))) });
  Object.defineProperty(globalThis, "describeNamespace", { value: name => JSON.parse(describeNamespace(name)) ?? undefined });
  Object.defineProperty(globalThis, "store", { value: (key,value) => {
    if (typeof key !== "string") throw new TypeError("store key must be a string");
    const json = value === undefined ? "null" : JSON.stringify(value);
    if (json === undefined) throw new TypeError("store value must be JSON serializable");
    writeStore(key,json,value === undefined);
    if (value === undefined) values.delete(key); else values.set(key,JSON.parse(json));
  } });
  Object.defineProperty(globalThis, "load", { value: key => {
    if (typeof key !== "string") throw new TypeError("store key must be a string");
    return values.has(key) ? copied(values.get(key)) : undefined;
  } });
  const invokeModel = (operation,selector,args) => {
    const request=JSON.parse(modelCall(operation,JSON.stringify(selector ?? {}),JSON.stringify(args)));
    return tools[request.name](request.args);
  };
  Object.defineProperty(globalThis, "models", { value: Object.freeze({
    getAvailable: () => JSON.parse(availableModels()),
    classify: (selector,args) => invokeModel("classify",selector,args),
    generateImages: (selector,args) => invokeModel("generateImages",selector,args)
  }) });
  Object.defineProperty(globalThis, "console", { value: Object.freeze({
    log: (...values) => values.forEach(text),
    info: (...values) => values.forEach(text),
    warn: (...values) => values.forEach(text),
    error: (...values) => values.forEach(text)
  }) });
  return json => {
    for (const response of JSON.parse(json)) {
      const handlers = pending.get(response.index);
      pending.delete(response.index);
      if (response.error !== undefined) handlers.reject(new Error(response.error));
      else handlers.resolve(response.value);
    }
  };
})()
"#;
