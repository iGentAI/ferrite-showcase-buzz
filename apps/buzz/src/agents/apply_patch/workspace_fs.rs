//! `FileStore` backends over the versioned [`Workspace`].

use super::apply::{normalize_store_path, path_under, FileStore, VersionedFileStore};
use crate::agents::error::{AgentError, Result};

/// Workspace bytes as text. A file that is not UTF-8 is refused rather than
/// decoded lossily: rewriting it would replace every invalid byte outside the
/// patched lines with U+FFFD.
fn utf8_text(path: &str, bytes: Vec<u8>) -> Result<String> {
    String::from_utf8(bytes).map_err(|_| AgentError::RecordInvalid {
        reason: format!("{path} is not UTF-8 text"),
    })
}
use crate::agents::workspace::Workspace;

/// A [`FileStore`] over the event-sourced [`Workspace`]. Owns the workspace.
pub struct WorkspaceFs {
    ws: Workspace,
    scope: String,
}

impl WorkspaceFs {
    pub fn new(ws: Workspace, scope: impl Into<String>) -> Self {
        Self {
            ws,
            scope: scope.into(),
        }
    }
}

impl FileStore for WorkspaceFs {
    async fn read(&mut self, path: &str) -> Result<Option<String>> {
        // A deleted path reads as absent in the workspace itself.
        Ok(self
            .ws
            .read(&self.scope, &normalize_store_path(path))
            .await?
            .map(|bytes| utf8_text(path, bytes))
            .transpose()?)
    }
    async fn write(&mut self, path: &str, content: &str) -> Result<()> {
        self.ws.write(&self.scope, &normalize_store_path(path), content.as_bytes()).await?;
        Ok(())
    }
    async fn delete(&mut self, path: &str) -> Result<()> {
        self.ws.delete(&self.scope, &normalize_store_path(path)).await?;
        Ok(())
    }
    async fn exists(&mut self, path: &str) -> Result<bool> {
        Ok(self.ws.read(&self.scope, &normalize_store_path(path)).await?.is_some())
    }
    async fn mkdir_p(&mut self, _path: &str) -> Result<()> {
        Ok(())
    }
    async fn list(&mut self, prefix: &str) -> Result<Vec<String>> {
        let prefix = normalize_store_path(prefix);
        let mut out = Vec::new();
        for (path, _head) in self.ws.list(&self.scope).await? {
            if path_under(&path, &prefix) {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }
}

impl VersionedFileStore for WorkspaceFs {
    async fn history(&mut self, path: &str) -> Result<Vec<u64>> {
        self.ws.version_numbers(&self.scope, &normalize_store_path(path)).await
    }
    async fn read_version(&mut self, path: &str, version: u64) -> Result<Option<String>> {
        Ok(self
            .ws
            .read_version(&self.scope, &normalize_store_path(path), version)
            .await?
            .map(|bytes| utf8_text(path, bytes))
            .transpose()?)
    }
}

/// A borrowing [`FileStore`] over a [`Workspace`] held elsewhere — so
/// `apply_patch` can run against a workspace the caller still owns.
pub struct WorkspaceFsRef<'a> {
    ws: &'a mut Workspace,
    scope: &'a str,
}

impl<'a> WorkspaceFsRef<'a> {
    pub fn new(ws: &'a mut Workspace, scope: &'a str) -> Self {
        Self { ws, scope }
    }
}

impl FileStore for WorkspaceFsRef<'_> {
    async fn read(&mut self, path: &str) -> Result<Option<String>> {
        // A deleted path reads as absent in the workspace itself.
        Ok(self
            .ws
            .read(self.scope, &normalize_store_path(path))
            .await?
            .map(|bytes| utf8_text(path, bytes))
            .transpose()?)
    }
    async fn write(&mut self, path: &str, content: &str) -> Result<()> {
        self.ws.write(self.scope, &normalize_store_path(path), content.as_bytes()).await?;
        Ok(())
    }
    async fn delete(&mut self, path: &str) -> Result<()> {
        self.ws.delete(self.scope, &normalize_store_path(path)).await?;
        Ok(())
    }
    async fn exists(&mut self, path: &str) -> Result<bool> {
        Ok(self.ws.read(self.scope, &normalize_store_path(path)).await?.is_some())
    }
    async fn mkdir_p(&mut self, _path: &str) -> Result<()> {
        Ok(())
    }
    async fn list(&mut self, prefix: &str) -> Result<Vec<String>> {
        let prefix = normalize_store_path(prefix);
        let mut out = Vec::new();
        for (path, _head) in self.ws.list(self.scope).await? {
            if path_under(&path, &prefix) {
                out.push(path);
            }
        }
        out.sort();
        Ok(out)
    }
}
