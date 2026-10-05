//! Immutable discovery snapshots. A snapshot describes admission, never grants it.
use crate::{ModelCall, ModelOperation, ModelSelector, Tool, discovery, models};
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicU64, Ordering},
};

const CACHE_BYTES: usize = 4 * 1024 * 1024;
const CACHE_ENTRIES: usize = 64;
const PAGE_BYTES: usize = 16_384;
static NEXT_SNAPSHOT: AtomicU64 = AtomicU64::new(1);
static PROCESS_SNAPSHOT_ID: OnceLock<String> = OnceLock::new();

struct SearchText {
    name: String,
    description: String,
    parameters: String,
    namespace: Option<String>,
}
struct EncodedSchema {
    json: String,
    revision: String,
}
#[derive(Default)]
struct SchemaCache {
    entries: HashMap<usize, Arc<EncodedSchema>>,
    order: VecDeque<usize>,
    bytes: usize,
}
struct Inner {
    tools: Vec<Tool>,
    search: Vec<SearchText>,
    schemas: Mutex<SchemaCache>,
}

/// Cheaply cloned, immutable catalog with shared search metadata and a bounded
/// schema cache. Filtered views share storage but never widen visible membership.
#[derive(Clone)]
pub struct Catalog {
    inner: Arc<Inner>,
    members: Arc<[usize]>,
    names: Arc<HashMap<String, Option<usize>>>,
    membership: Arc<HashSet<usize>>,
    namespaces: Arc<BTreeSet<String>>,
    models: Arc<Result<Vec<usize>, String>>,
    snapshot: String,
}

impl Catalog {
    pub fn new(tools: Vec<Tool>) -> Self {
        let search = tools
            .iter()
            .map(|tool| SearchText {
                name: tool.name.to_lowercase(),
                description: discovery::bounded(&tool.description, discovery::MAX_METADATA_BYTES)
                    .to_lowercase(),
                parameters: discovery::schema_metadata(&tool.schema),
                namespace: discovery::namespace(tool),
            })
            .collect();
        let members = (0..tools.len()).collect();
        Self::view(
            Arc::new(Inner {
                tools,
                search,
                schemas: Mutex::default(),
            }),
            members,
        )
    }

    fn view(inner: Arc<Inner>, members: Vec<usize>) -> Self {
        let mut names = HashMap::new();
        // Exact names take precedence over normalized aliases.
        for &index in &members {
            let tool = &inner.tools[index];
            names
                .entry(tool.name.clone())
                .and_modify(|entry| *entry = None)
                .or_insert(Some(index));
        }
        let exact_names: HashSet<_> = names.keys().cloned().collect();
        for &index in &members {
            let tool = &inner.tools[index];
            let alias = discovery::identifier(&tool.name);
            if exact_names.contains(&alias) {
                continue;
            }
            names
                .entry(alias)
                .and_modify(|entry| *entry = None)
                .or_insert(Some(index));
        }
        let namespaces = members
            .iter()
            .filter_map(|i| inner.search[*i].namespace.clone())
            .collect();
        let model_indices = models::model_indices(members.iter().map(|i| (*i, &inner.tools[*i])));
        Self {
            inner,
            names: Arc::new(names),
            membership: Arc::new(members.iter().copied().collect()),
            members: members.into(),
            namespaces: Arc::new(namespaces),
            models: Arc::new(model_indices),
            // These tokens identify metadata, never authority. Include a process
            // identity so resumed callers cannot mix pages after a restart.
            snapshot: format!(
                "{}/{}",
                PROCESS_SNAPSHOT_ID.get_or_init(|| {
                    let identity =
                        format!("{}:{:?}", std::process::id(), std::time::SystemTime::now());
                    format!("{:x}", Sha256::digest(identity.as_bytes()))
                }),
                NEXT_SNAPSHOT.fetch_add(1, Ordering::Relaxed)
            ),
        }
    }

    pub fn filtered(&self, predicate: impl Fn(&Tool) -> bool) -> Self {
        Self::view(
            self.inner.clone(),
            self.members
                .iter()
                .copied()
                .filter(|i| predicate(&self.inner.tools[*i]))
                .collect(),
        )
    }

    pub fn iter(&self) -> impl ExactSizeIterator<Item = &Tool> {
        self.members.iter().map(|i| &self.inner.tools[*i])
    }

    pub fn lookup(&self, name: &str) -> Option<&Tool> {
        let index = self.names.get(name).copied().flatten()?;
        self.membership
            .contains(&index)
            .then_some(&self.inner.tools[index])
    }

    pub(crate) fn validate_models(&self) -> Result<(), String> {
        self.models
            .as_ref()
            .as_ref()
            .map(|_| ())
            .map_err(Clone::clone)
    }

    fn resolve_namespace(&self, requested: &str) -> Option<&str> {
        if let Some(name) = self.namespaces.get(requested) {
            return Some(name);
        }
        let alias = |name: &str| discovery::identifier(name.strip_prefix("mcp__").unwrap_or(name));
        let wanted = alias(requested);
        let mut matches = self.namespaces.iter().filter(|name| alias(name) == wanted);
        let first = matches.next()?;
        matches.next().is_none().then_some(first.as_str())
    }

    /// Common ranking for native and script discovery. Results retain canonical
    /// names; script presentation alone normalizes them into JS identifiers.
    pub fn search(
        &self,
        query: &str,
        exact_names: &[String],
        limit: usize,
        requested_namespace: Option<&str>,
    ) -> Vec<&Tool> {
        let words: Vec<_> = discovery::bounded(query, 2048)
            .split(|c: char| !c.is_alphanumeric())
            .filter(|s| !s.is_empty())
            .take(32)
            .map(str::to_lowercase)
            .collect();
        let exact: HashSet<_> = exact_names.iter().map(|s| s.to_lowercase()).collect();
        let resolved = requested_namespace.and_then(|name| self.resolve_namespace(name));
        if requested_namespace.is_some() && resolved.is_none() {
            return Vec::new();
        }
        let mut matches = Vec::new();
        for &index in self.members.iter() {
            let text = &self.inner.search[index];
            if resolved.is_some_and(|ns| text.namespace.as_deref() != Some(ns)) {
                continue;
            }
            let score: usize = usize::from(exact.contains(&text.name)) * 1000
                + words
                    .iter()
                    .map(|word| {
                        usize::from(text.name.contains(word)) * 6
                            + usize::from(text.description.contains(word)) * 2
                            + usize::from(text.parameters.contains(word))
                    })
                    .sum::<usize>();
            if score > 0 {
                matches.push((score, index));
            }
        }
        matches.sort_unstable_by(|(a, ia), (b, ib)| {
            b.cmp(a)
                .then(self.inner.tools[*ia].name.cmp(&self.inner.tools[*ib].name))
        });
        matches
            .into_iter()
            .take(limit.min(64))
            .map(|(_, i)| &self.inner.tools[i])
            .collect()
    }

    pub fn schema_page(&self, name: &str, options: &Value) -> Result<Value, String> {
        let Some(index) = self
            .names
            .get(name)
            .copied()
            .flatten()
            .filter(|i| self.membership.contains(i))
        else {
            return Ok(Value::Null);
        };
        let max = integer(options, "maxBytes", PAGE_BYTES)?;
        if !(1..=PAGE_BYTES).contains(&max) {
            return Err("maxBytes must be 1 to 16384".into());
        }
        let offset = integer(options, "offsetBytes", 0)?;
        let tool = &self.inner.tools[index];
        let mut cache = self
            .inner
            .schemas
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let encoded = if let Some(encoded) = cache.entries.get(&index).cloned() {
            cache.order.retain(|i| *i != index);
            cache.order.push_back(index);
            encoded
        } else {
            #[derive(Serialize)]
            #[serde(rename_all = "camelCase")]
            struct Schemas<'a> {
                input_schema: &'a Value,
                output_schema: &'a Option<Value>,
            }
            let json = serde_json::to_string(&Schemas {
                input_schema: &tool.schema,
                output_schema: &tool.output_schema,
            })
            .map_err(|e| e.to_string())?;
            let revision = format!("sha256:{:x}", Sha256::digest(json.as_bytes()));
            let encoded = Arc::new(EncodedSchema { json, revision });
            let cost = encoded.json.capacity() + encoded.revision.capacity();
            if cost <= CACHE_BYTES {
                while cache.bytes + cost > CACHE_BYTES || cache.entries.len() >= CACHE_ENTRIES {
                    let oldest = cache.order.pop_front().expect("nonempty bounded cache");
                    let evicted = cache.entries.remove(&oldest).expect("cached entry");
                    cache.bytes -= evicted.json.capacity() + evicted.revision.capacity();
                }
                cache.bytes += cost;
                cache.entries.insert(index, encoded.clone());
                cache.order.push_back(index);
            }
            encoded
        };
        drop(cache);
        if offset > encoded.json.len() || !encoded.json.is_char_boundary(offset) {
            return Err("offsetBytes must be a UTF-8 boundary within the schema".into());
        }
        let end = offset + discovery::bounded(&encoded.json[offset..], max).len();
        if end == offset && offset != encoded.json.len() {
            return Err("maxBytes is too small for the next UTF-8 character".into());
        }
        Ok(
            json!({"name":tool.name,"revision":encoded.revision,"offsetBytes":offset,"nextOffsetBytes":end,"totalBytes":encoded.json.len(),"complete":end == encoded.json.len(),"json":&encoded.json[offset..end]}),
        )
    }

    /// Retained encoded bytes, excluding the already admitted tool schemas.
    pub fn cached_schema_bytes(&self) -> usize {
        self.inner
            .schemas
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .bytes
    }

    pub(crate) fn namespace_page(&self, requested: &str, options: &Value) -> Result<Value, String> {
        let Some(name) = self.resolve_namespace(requested) else {
            return Ok(Value::Null);
        };
        let mut members: Vec<_> = self
            .iter()
            .filter(|tool| discovery::namespace(tool).as_deref() == Some(name))
            .collect();
        members.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        let instructions: BTreeSet<_> = members
            .iter()
            .filter_map(|tool| tool.namespace_instructions.as_deref())
            .filter(|s| !s.is_empty())
            .collect();
        let mut header = json!({"name":name});
        if instructions.len() == 1 {
            header["instructions"] = json!(discovery::bounded(
                instructions.first().unwrap(),
                discovery::MAX_METADATA_BYTES
            ));
            header["instructionsTrust"] = json!("untrusted");
        }
        // Guidance may expand sixfold when escaped. Trim guidance only; tool
        // names and schemas must remain exact, or fail explicitly.
        while serde_json::to_vec(&header)
            .map_err(|e| e.to_string())?
            .len()
            > PAGE_BYTES / 2
        {
            let Some(text) = header.get("instructions").and_then(Value::as_str) else {
                return Err("namespace name exceeds page budget".into());
            };
            if text.is_empty() {
                return Err("namespace metadata exceeds page budget".into());
            }
            header["instructions"] = json!(discovery::bounded(text, text.len() / 2));
        }
        self.page(
            "tools",
            members
                .iter()
                .map(|tool| Value::String(discovery::identifier(&tool.name))),
            options,
            header,
        )
    }

    pub(crate) fn model_page(&self, options: &Value) -> Result<Value, String> {
        let indices = self.models.as_ref().as_ref().map_err(Clone::clone)?;
        let mut tools: Vec<_> = indices.iter().map(|i| &self.inner.tools[*i]).collect();
        tools.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        self.page("models",tools.iter().map(|tool| json!({"tool":tool.name,"operation":tool.model_operation,"binding":tool.model_binding})),options,json!({}))
    }

    fn page(
        &self,
        key: &str,
        entries: impl ExactSizeIterator<Item = Value>,
        options: &Value,
        mut header: Value,
    ) -> Result<Value, String> {
        let snapshot = &self.snapshot;
        if options
            .get("snapshot")
            .is_some_and(|value| value.as_str() != Some(snapshot.as_str()))
        {
            return Err("inspection snapshot changed; restart paging".into());
        }
        let offset = integer(options, "offset", 0)?;
        let limit = integer(options, "limit", 16)?;
        if !(1..=64).contains(&limit) {
            return Err("inspection limit must be 1 to 64".into());
        }
        let total = entries.len();
        if offset > total {
            return Err("inspection offset is outside the catalog".into());
        }
        header["snapshot"] = json!(snapshot);
        header["offset"] = json!(offset);
        header["nextOffset"] = json!(total);
        header["total"] = json!(total);
        header["complete"] = json!(false);
        header[key] = json!([]);
        let mut bytes = serde_json::to_vec(&header)
            .map_err(|e| e.to_string())?
            .len();
        let mut page = Vec::new();
        for entry in entries.skip(offset).take(limit) {
            let cost = serde_json::to_vec(&entry).map_err(|e| e.to_string())?.len() + 1;
            if bytes + cost > PAGE_BYTES {
                break;
            }
            bytes += cost;
            page.push(entry);
        }
        if page.is_empty() && offset < total {
            return Err("inspection entry exceeds page budget; use exact tool lookup".into());
        }
        let next = offset + page.len();
        header["nextOffset"] = json!(next);
        header["complete"] = json!(next == total);
        header[key] = json!(page);
        Ok(header)
    }

    pub(crate) fn available_models_json(&self) -> Result<String, String> {
        let indices = self.models.as_ref().as_ref().map_err(Clone::clone)?;
        // Preserve the legacy ModelAlias field order and serialize borrowed
        // schemas directly instead of constructing another full JSON catalog.
        #[derive(Serialize)]
        struct Alias<'a> {
            operation: ModelOperation,
            tool: &'a str,
            binding: &'a crate::ModelBinding,
            input_schema: &'a Value,
            output_schema: &'a Option<Value>,
        }
        let aliases: Vec<_> = indices
            .iter()
            .map(|i| {
                let tool = &self.inner.tools[*i];
                Alias {
                    operation: tool.model_operation.expect("validated model operation"),
                    tool: &tool.name,
                    binding: tool
                        .model_binding
                        .as_ref()
                        .expect("validated model binding"),
                    input_schema: &tool.schema,
                    output_schema: &tool.output_schema,
                }
            })
            .collect();
        serde_json::to_string(&aliases).map_err(|error| error.to_string())
    }

    pub(crate) fn resolve_model_call(
        &self,
        operation: ModelOperation,
        selector: &ModelSelector,
        args: Value,
    ) -> Result<ModelCall, String> {
        let indices = self.models.as_ref().as_ref().map_err(Clone::clone)?;
        models::resolve_model_tools(
            indices.iter().map(|i| &self.inner.tools[*i]),
            operation,
            selector,
            args,
        )
    }
}

fn integer(options: &Value, key: &str, default: usize) -> Result<usize, String> {
    match options.get(key) {
        None => Ok(default),
        Some(value) => value
            .as_u64()
            .and_then(|n| usize::try_from(n).ok())
            .ok_or_else(|| format!("{key} must be an integer")),
    }
}

#[cfg(test)]
#[path = "catalog_tests.rs"]
mod tests;
