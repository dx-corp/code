//! Synchronous VM helpers inspect only host-admitted metadata and scratch data.
use super::*;

fn bridge_error(error: impl ToString) -> rquickjs::Error {
    rquickjs::Error::new_from_js_message("script helper", "valid input", error.to_string())
}
pub(crate) fn install_helpers(
    ctx: &rquickjs::Ctx<'_>,
    tools: &Catalog,
    output: Arc<Mutex<output::OutputSink>>,
    overflow: Arc<Mutex<Option<String>>>,
    store: Arc<Mutex<Store>>,
) -> Result<(), String> {
    let shared = tools.clone();
    let catalog = shared.clone();
    let search = Function::new(
        ctx.clone(),
        move |query: String, options: String| -> rquickjs::Result<String> {
            let options: Value = serde_json::from_str(&options).map_err(bridge_error)?;
            let limit = match options.get("limit") {
                None => 8,
                Some(value) => value
                    .as_u64()
                    .filter(|v| *v > 0 && *v <= 32)
                    .ok_or_else(|| bridge_error("search limit must be 1 to 32"))?
                    as usize,
            };
            let namespace = match options.get("namespace") {
                None => None,
                Some(value) => Some(
                    value
                        .as_str()
                        .ok_or_else(|| bridge_error("namespace must be a string"))?,
                ),
            };
            let names: Vec<String> = match options.get("names") {
                None => Vec::new(),
                Some(value) => serde_json::from_value(value.clone()).map_err(bridge_error)?,
            };
            serde_json::to_string(&catalog.search(&query, &names, limit, namespace).into_iter().map(|tool|
                serde_json::json!({"name":discovery::identifier(&tool.name),"description":discovery::bounded(&tool.description,256),"namespace":crate::namespace(tool)})
            ).collect::<Vec<_>>())
                .map_err(bridge_error)
        },
    )
    .map_err(|e| e.to_string())?;
    let catalog = shared.clone();
    let describe = Function::new(
        ctx.clone(),
        move |name: String| -> rquickjs::Result<String> {
            serde_json::to_string(&catalog.lookup(&name).map(describe_tool)).map_err(bridge_error)
        },
    )
    .map_err(|e| e.to_string())?;
    let catalog = shared.clone();
    let describe_ns = Function::new(
        ctx.clone(),
        move |name: String| -> rquickjs::Result<String> {
            let value = discovery::describe_namespace(catalog.iter(), &name);
            serde_json::to_string(&value).map_err(bridge_error)
        },
    )
    .map_err(|e| e.to_string())?;
    let catalog = shared.clone();
    let schema = Function::new(
        ctx.clone(),
        move |name: String, options: String| -> rquickjs::Result<String> {
            let options: Value = serde_json::from_str(&options).map_err(bridge_error)?;
            serde_json::to_string(&catalog.schema_page(&name, &options).map_err(bridge_error)?)
                .map_err(bridge_error)
        },
    )
    .map_err(|e| e.to_string())?;
    let catalog = shared.clone();
    let metadata = Function::new(
        ctx.clone(),
        move |name: String, field: String| -> rquickjs::Result<String> {
            let tool = catalog
                .lookup(&name)
                .ok_or_else(|| bridge_error("unknown admitted tool"))?;
            match field.as_str() {
                "description" => serde_json::to_string(&tool.description),
                "schema" => serde_json::to_string(&tool.schema),
                "output_schema" => serde_json::to_string(&tool.output_schema),
                "namespace" => serde_json::to_string(&tool.namespace),
                "model_operation" => serde_json::to_string(&tool.model_operation),
                "model_binding" => serde_json::to_string(&tool.model_binding),
                _ => return Err(bridge_error("unknown tool metadata field")),
            }
            .map_err(bridge_error)
        },
    )
    .map_err(|e| e.to_string())?;
    // Validate model bindings now, but keep their schemas outside the VM until
    // the script explicitly requests the available model catalog.
    tools.validate_models()?;
    let catalog = shared.clone();
    let available_models = Function::new(ctx.clone(), move || -> rquickjs::Result<String> {
        catalog.available_models_json().map_err(bridge_error)
    })
    .map_err(|e| e.to_string())?;
    let catalog = shared.clone();
    let namespace_page = Function::new(
        ctx.clone(),
        move |name: String, options: String| -> rquickjs::Result<String> {
            let options = serde_json::from_str(&options).map_err(bridge_error)?;
            serde_json::to_string(
                &catalog
                    .namespace_page(&name, &options)
                    .map_err(bridge_error)?,
            )
            .map_err(bridge_error)
        },
    )
    .map_err(|e| e.to_string())?;
    let catalog = shared.clone();
    let model_page = Function::new(
        ctx.clone(),
        move |options: String| -> rquickjs::Result<String> {
            let options = serde_json::from_str(&options).map_err(bridge_error)?;
            serde_json::to_string(&catalog.model_page(&options).map_err(bridge_error)?)
                .map_err(bridge_error)
        },
    )
    .map_err(|e| e.to_string())?;
    let snapshot = serde_json::to_string(
        &*store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner),
    )
    .map_err(|e| e.to_string())?;
    let store_write = Function::new(
        ctx.clone(),
        move |key: String, json: String, delete: bool| -> rquickjs::Result<()> {
            let mut current = store
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let mut next = current.clone();
            if key.is_empty() || key.len() > 256 {
                return Err(bridge_error("store key must contain 1 to 256 bytes"));
            }
            if delete {
                next.remove(&key);
            } else {
                next.insert(key, serde_json::from_str(&json).map_err(bridge_error)?);
            }
            validate_store(&next).map_err(bridge_error)?;
            *current = next;
            Ok(())
        },
    )
    .map_err(|e| e.to_string())?;
    let image = Function::new(ctx.clone(), move |json: String| -> rquickjs::Result<()> {
        let block = image_block(serde_json::from_str(&json).map_err(bridge_error)?)
            .map_err(bridge_error)?;
        let result = output
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .image(block);
        if let Err(error) = &result {
            *overflow
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(error.clone());
        }
        result.map_err(bridge_error)
    })
    .map_err(|e| e.to_string())?;
    let catalog = shared.clone();
    let models = Function::new(
        ctx.clone(),
        move |operation: String, selector: String, args: String| -> rquickjs::Result<String> {
            let operation = match operation.as_str() {
                "classify" => ModelOperation::Classify,
                "generateImages" => ModelOperation::GenerateImages,
                _ => return Err(bridge_error("unsupported model operation")),
            };
            let selector = serde_json::from_str(&selector).map_err(bridge_error)?;
            let call = catalog
                .resolve_model_call(
                    operation,
                    &selector,
                    serde_json::from_str(&args).map_err(bridge_error)?,
                )
                .map_err(bridge_error)?;
            serde_json::to_string(&serde_json::json!({"name":call.name,"args":call.args}))
                .map_err(bridge_error)
        },
    )
    .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_namespace_page", namespace_page)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_model_page", model_page)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_search", search)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_describe", describe)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_namespace", describe_ns)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_schema", schema)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_store", store_write)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__store", snapshot)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_image", image)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_model", models)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_metadata", metadata)
        .map_err(|e| e.to_string())?;
    ctx.globals()
        .set("__host_models", available_models)
        .map_err(|e| e.to_string())?;
    Ok(())
}
