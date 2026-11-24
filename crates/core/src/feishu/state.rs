//! Per-root, per-document Feishu sync metadata and merge baselines stored in
//! local application data (`quiet-md/feishu/<root-key>/`, or
//! `<root>/.quiet/feishu/` under `QUIET_MD_HOME` for isolated tests).
//!
//! Keeping runtime sync state out of the synced library folder ensures zero
//! `store::FORMAT` changes and avoids conflicts with iCloud / Git sync.

use super::codec::MediaMap;
use crate::docxml;
use crate::model::Document;
use crate::store::write_atomic;
use crate::vfs::{self, PathExt as _};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// Persisted link and sync state for one local document.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct FeishuLink {
    /// Original Feishu / Lark URL pasted or returned on create.
    #[serde(default)]
    pub url: String,
    /// Underlying docx document token (resolved from wiki node if needed).
    pub token: String,
    /// Original wiki node token when linked from `/wiki/…` (empty for `/docx/`).
    #[serde(default)]
    pub wiki_node: String,
    /// Cloud `revision_id` last seen / acknowledged (0 before first sync).
    #[serde(default)]
    pub revision: u64,
    /// Cloud media token → library-relative local path (e.g. `.assets/feishu-…`).
    #[serde(default)]
    pub media: MediaMap,
    /// Unix seconds when the last sync completed.
    #[serde(default)]
    pub synced: u64,
    /// Real-time bidirectional auto-sync enabled for this document.
    #[serde(default = "default_true")]
    pub auto_sync: bool,
    /// Non-empty when auto-push was automatically paused (e.g. unresolved conflict or blocker).
    #[serde(default)]
    pub paused_reason: Option<String>,
    /// Local document id of the saved cloud conflict copy, when a conflict is open.
    #[serde(default)]
    pub conflict_copy: Option<String>,
    /// Number of unresolved conflicting sections from the last merge.
    #[serde(default)]
    pub conflict_count: usize,
    /// Last sync error (cleared on next successful sync).
    #[serde(default)]
    pub last_error: Option<String>,
}

fn default_true() -> bool {
    true
}

impl FeishuLink {
    pub fn is_wiki(&self) -> bool {
        !self.wiki_node.is_empty()
    }

    pub fn has_conflict(&self) -> bool {
        self.conflict_count > 0 || self.conflict_copy.is_some()
    }

    pub fn clear_conflict(&mut self) {
        self.conflict_count = 0;
        self.conflict_copy = None;
        if self.paused_reason.as_deref() == Some("conflict") {
            self.paused_reason = None;
        }
    }

    /// Best URL to open this document in the browser / Feishu client.
    pub fn open_url(&self) -> Option<String> {
        if self.url.starts_with("http://") || self.url.starts_with("https://") {
            return Some(self.url.clone());
        }
        if !self.token.is_empty() {
            return Some(format!("https://feishu.cn/docx/{}", self.token));
        }
        None
    }
}

/// Directory holding `<doc_id>.json` and `<doc_id>.base.xml` for one workspace root.
#[derive(Clone, Debug)]
pub struct SyncDir(pub PathBuf);

impl SyncDir {
    /// Local data directory for root `root_key` (rooted at `root_path`).
    pub fn for_root(root_path: &Path, root_key: &str) -> Self {
        if std::env::var_os("QUIET_MD_HOME").is_some() || cfg!(target_family = "wasm") {
            return SyncDir(root_path.join(".quiet").join("feishu"));
        }
        let key = if root_key.is_empty() { crate::store::library_key(root_path) } else { root_key.to_string() };
        let base = dirs::data_local_dir().unwrap_or_else(vfs::temp_dir).join("quiet-md").join("feishu").join(key);
        SyncDir(base)
    }

    fn link_path(&self, id: &str) -> PathBuf {
        self.0.join(format!("{id}.json"))
    }

    fn base_path(&self, id: &str) -> PathBuf {
        self.0.join(format!("{id}.base.xml"))
    }

    pub fn load_link(&self, id: &str) -> Option<FeishuLink> {
        let bytes = vfs::read(self.link_path(id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    pub fn save_link(&self, id: &str, link: &FeishuLink) -> Result<()> {
        vfs::create_dir_all(&self.0)?;
        let bytes = serde_json::to_vec_pretty(link)?;
        write_atomic(&self.link_path(id), &bytes)
    }

    pub fn load_base(&self, id: &str) -> Option<Document> {
        let xml = vfs::read_to_string(self.base_path(id)).ok()?;
        docxml::parse(&xml).ok()
    }

    pub fn save_base(&self, id: &str, doc: &Document) -> Result<()> {
        vfs::create_dir_all(&self.0)?;
        write_atomic(&self.base_path(id), docxml::serialize(doc).as_bytes())
    }

    pub fn remove(&self, id: &str) {
        let _ = vfs::remove_file(self.link_path(id));
        let _ = vfs::remove_file(self.base_path(id));
    }

    /// All linked document ids and their links in this root.
    pub fn list_links(&self) -> BTreeMap<String, FeishuLink> {
        let mut out = BTreeMap::new();
        let Ok(rd) = vfs::read_dir(&self.0) else { return out };
        for entry in rd.flatten() {
            let path = entry.path();
            if !path.fs_is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|n| n.to_str()) else { continue };
            let Some(id) = name.strip_suffix(".json") else { continue };
            if let Ok(bytes) = vfs::read(&path)
                && let Ok(link) = serde_json::from_slice::<FeishuLink>(&bytes)
            {
                out.insert(id.to_string(), link);
            }
        }
        out
    }

    /// Find an already-linked local document id pointing at `doc_token`.
    pub fn find_by_token(&self, doc_token: &str) -> Option<String> {
        self.list_links().into_iter().find(|(_, l)| l.token == doc_token).map(|(id, _)| id)
    }
}
