//! Declared skills and the persona's local memory.

use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use crate::agents::error::Result;
use crate::agents::local_memory::LocalMemory;
use crate::agents::local_memory::MemoryScope;
use memory::memory_docs_json;
use super::{CloudToolRegistry, memory};

impl<'a> CloudToolRegistry<'a> {
    /// Load one declared skill document (docs/_archive/agent-pack-definition.md,
    /// Phase 4). Only names on the definition's declared list are readable —
    /// a stale or hand-seeded document for an undeclared skill is
    /// unreachable. Results ride the ordinary 16 KiB tool-result cap.
    pub(super) async fn load_skill(&self, args: &[u8]) -> Result<Vec<u8>> {
        let Some(skills) = &self.skills else {
            return Self::err("no skills are configured for this agent");
        };
        let v: Value = serde_json::from_slice(args).unwrap_or(serde_json::json!({}));
        let name = v.get("name").and_then(|s| s.as_str()).unwrap_or("");
        if name.is_empty() {
            return Self::err("load_skill requires a 'name'");
        }
        if !skills.names.iter().any(|s| s == name) {
            return Self::err(format!(
                "unknown skill: {name}; available skills: {}",
                skills.names.join(", ")
            ));
        }
        let mut store = self.ctx.store(&skills.config_store_handle)?;
        let key = crate::agents::definition::skill_key(&skills.agent, name);
        match store.get(&key).await {
            Ok(Some(bytes)) => Self::ok(serde_json::json!({
                "name": name,
                "content": String::from_utf8_lossy(&bytes),
            })),
            Ok(None) => Self::err(format!(
                "skill '{name}' is declared but its document is not seeded"
            )),
            Err(e) => Self::err(format!("skill store read failed: {e}")),
        }
    }

    pub(super) fn local_memory(&self) -> Result<Option<(LocalMemory, MemoryScope)>> {
        let Some(mem) = &self.local_memory else {
            return Ok(None);
        };
        let store = self.ctx.store(&mem.store_handle)?;
        let scope = MemoryScope::new(&mem.tenant, &mem.user, &mem.space)?;
        Ok(Some((LocalMemory::new(store), scope)))
    }

    pub(super) async fn remember_local(
        &self,
        text: &str,
        category: &str,
        entities: Vec<String>,
    ) -> Result<Option<Value>> {
        let Some((mut memory, scope)) = self.local_memory()? else {
            return Ok(None);
        };
        let id = match memory.remember(&scope, text, category, entities).await {
            Ok(id) => id,
            Err(e) if is_model_error(&e) => {
                return Ok(Some(serde_json::json!({"error": e.to_string(), "source": "local-store"})))
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Some(serde_json::json!({"ok": true, "id": id, "source": "local-store"})))
    }

    pub(super) async fn recall_local(&self, query: &str, limit: u64) -> Result<Option<Value>> {
        let Some((mut memory, scope)) = self.local_memory()? else {
            return Ok(None);
        };
        let recalled = memory
            .recall_by_text(&scope, query, &[], limit as usize, 0.01, 400, true)
            .await?;
        Ok(Some(serde_json::json!({
            "ok": true,
            "source": "local-store",
            "recall": {
                "memories": memory_docs_json(&recalled.memories),
                "preamble": recalled.preamble,
            }
        })))
    }

    pub(super) async fn get_local_memory(&self, id: &str) -> Result<Option<Value>> {
        let Some((mut memory, scope)) = self.local_memory()? else {
            return Ok(None);
        };
        let record = memory.get(&scope, id).await?;
        Ok(Some(serde_json::json!({
            "ok": true,
            "source": "local-store",
            "memory": record.map(|doc| memory_docs_json(&[doc]).into_iter().next().unwrap_or(Value::Null)),
        })))
    }

    pub(super) async fn list_local_memory(&self, include_superseded: bool) -> Result<Option<Value>> {
        let Some((mut memory, scope)) = self.local_memory()? else {
            return Ok(None);
        };
        let records = memory.list(&scope, include_superseded).await?;
        Ok(Some(serde_json::json!({
            "ok": true,
            "source": "local-store",
            "memories": memory_docs_json(&records),
        })))
    }

    pub(super) async fn correct_local_memory(&self, id: &str, text: &str) -> Result<Option<Value>> {
        let Some((mut memory, scope)) = self.local_memory()? else {
            return Ok(None);
        };
        match memory.correct_text(&scope, id, text).await {
            Ok(_) => {}
            Err(e) if is_model_error(&e) => {
                return Ok(Some(serde_json::json!({"error": e.to_string(), "source": "local-store"})))
            }
            Err(e) => return Err(e.into()),
        }
        Ok(Some(serde_json::json!({"ok": true, "id": id, "source": "local-store"})))
    }

    pub(super) async fn supersede_local_memory(
        &self,
        supersedes: &str,
        text: &str,
        category: &str,
        entities: Vec<String>,
    ) -> Result<Option<Value>> {
        let Some((mut memory, scope)) = self.local_memory()? else {
            return Ok(None);
        };
        let id = match memory
            .supersede_text(&scope, supersedes, text, category, entities)
            .await
        {
            Ok(id) => id,
            Err(e) if is_model_error(&e) => {
                return Ok(Some(serde_json::json!({"error": e.to_string(), "source": "local-store"})))
            }
            Err(e) => return Err(e.into()),
        };
        Ok(Some(serde_json::json!({
            "ok": true,
            "id": id,
            "supersedes": supersedes,
            "source": "local-store",
        })))
    }

    pub(super) async fn forget_local_memory(&self, id: &str) -> Result<Option<Value>> {
        let Some((mut memory, scope)) = self.local_memory()? else {
            return Ok(None);
        };
        memory.forget(&scope, id).await?;
        Ok(Some(serde_json::json!({"ok": true, "id": id, "source": "local-store"})))
    }
}

/// A memory error the model caused and can correct (an unknown id, oversized
/// or malformed text, a full quota) is a soft tool result. Only a plane fault
/// fails the recorded call, which would leave a fenced turn to redeliver the
/// same bad arguments forever.
fn is_model_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::QuotaExceeded
            | std::io::ErrorKind::NotFound
            | std::io::ErrorKind::InvalidInput
    )
}
