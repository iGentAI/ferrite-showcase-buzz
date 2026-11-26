use ferrite_app::serde_json::{self, Value};

use crate::agents::local_memory::MemoryDoc;

pub(super) fn memory_docs_json(memories: &[MemoryDoc]) -> Vec<Value> {
    memories
        .iter()
        .map(|memory| {
            serde_json::json!({
                "id": memory.id.clone(),
                "text": memory.text.clone(),
                "category": memory.category.clone(),
                "entities": memory.entities.clone(),
            })
        })
        .collect()
}

/// A memory tool call's category (`fact` when absent or empty) and entities.
pub(super) fn memory_labels(args: &Value) -> (&str, Vec<String>) {
    let category = args
        .get("category")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or("fact");
    let entities = args
        .get("entities")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    (category, entities)
}
