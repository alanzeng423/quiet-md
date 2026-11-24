//! 飞书实时双向同步: import a Feishu / Lark cloud document or wiki page by link,
//! edit locally at native speed, and keep local ↔ cloud in sync in real time.
//!
//! Architecture:
//! - Pure logic (link parsing, DocxXML normalization, `merge3` wrapper,
//!   minimal block push planner, per-root state, transport trait) lives in
//!   `fdl_core::feishu`.
//! - This module owns the per-window coordinator (`FeishuState`), debounced
//!   auto-push, lightweight revision polling, header status pill + popover,
//!   conflict/blocker banner, import/link dialog and "…" submenu.

use crate::app::Workspace;
use crate::design::{Button, ButtonSize, Dialog, Field, MenuItem, MenuLabel, MenuSeparator, Popover, Switch, color, geo, scrim};
use crate::input::{TextField, TextFieldEvent};
use crate::model::{Document, RichText};
use crate::settings::tr;
use crate::store::Store;
use crate::theme::{self, Theme, craft};
use crate::ui::{TipExt, TitleBarExt, ToastKind, icon};
use fdl_core::feishu::{
    self, AuthInfo, FeishuLink, FeishuTransport, Identity, ImportOutcome, PullOutcome, PushOutcome, SyncDir, TransportError,
};
use gpui::{prelude::FluentBuilder, *};
use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use web_time::Instant;

/// Auto-push waits this long after the last local save before syncing to Feishu.
const AUTO_PUSH_DELAY: Duration = Duration::from_millis(2500);
/// Lightweight `revision_id` polling interval while the window is active.
pub(crate) const POLL_ACTIVE: Duration = Duration::from_secs(5);
/// Slower polling interval when the window is in the background.
pub(crate) const POLL_IDLE: Duration = Duration::from_secs(30);
/// Ignore local save echo for this window after applying a remote pull.
const ECHO_GUARD: Duration = Duration::from_secs(2);

/// Transient per-document sync status shown on the header pill and sidebar.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiveStatus {
    Synced,
    Syncing,
    PendingPush,
    Offline(String),
    Error(String),
}

/// What the user triggers from the UI or automatic coordinator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SyncMode {
    /// Pull remote changes and three-way merge into local.
    Pull,
    /// Manual push (pull + merge + incremental block push).
    Push,
    /// Debounced real-time background push.
    AutoPush,
    /// Pull + merge + push if local changed.
    SyncNow,
    /// Overwrite local with cloud and clear conflicts.
    TakeCloud,
    /// Overwrite cloud with local and clear conflicts.
    TakeLocal,
    /// Upload an unlinked local document as a brand-new Feishu cloud document.
    Create,
}

/// Which workflow the Feishu URL dialog is running.
#[derive(Clone, Debug)]
pub enum DialogKind {
    /// Create a brand-new local document imported from a Feishu link.
    Import { parent: Option<String> },
    /// Link an existing local document to a Feishu link.
    Link { id: String, blank: bool },
}

pub struct FeishuDialog {
    pub kind: DialogKind,
    pub field: Entity<TextField>,
    pub busy: bool,
    pub error: Option<String>,
    _subs: Vec<Subscription>,
}

/// Per-window Feishu real-time sync coordinator state.
#[derive(Default)]
pub struct FeishuState {
    /// Documents with a sync network/CLI operation currently in flight.
    pub busy: HashSet<String>,
    /// Documents modified while a sync was already in flight (push again right after).
    pub pending_push: HashSet<String>,
    /// In-memory mirror of persisted links across open roots (for zero-I/O rendering).
    pub links: HashMap<String, FeishuLink>,
    /// Live runtime status per document.
    pub live: HashMap<String, LiveStatus>,
    /// Per-document debounced auto-push tasks.
    pub push_timers: HashMap<String, Task<()>>,
    /// Suppress auto-push triggered by our own remote-pull save.
    pub suppress_echo: HashMap<String, Instant>,
    /// Backoff deadline after rate-limit or network errors during background polling.
    pub backoff_until: Option<Instant>,
    /// Active import / link dialog.
    pub dlg: Option<FeishuDialog>,
    /// Whether the header status pill popover is open.
    pub status_menu: bool,
    /// Last-frame bounds of the header status pill (anchors its popover).
    pub status_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// Cached local `lark-cli` auth status.
    pub auth: Option<AuthInfo>,
    pub auth_loading: bool,
}

impl FeishuState {
    pub fn is_busy(&self, id: &str) -> bool {
        self.busy.contains(id)
    }

    pub fn live_of(&self, id: &str) -> LiveStatus {
        if self.busy.contains(id) {
            return LiveStatus::Syncing;
        }
        self.live.get(id).cloned().unwrap_or(LiveStatus::Synced)
    }
}

/// Build the configured [`FeishuTransport`] backend.
fn transport() -> Arc<dyn FeishuTransport> {
    #[cfg(not(target_family = "wasm"))]
    {
        Arc::new(feishu::LarkCliTransport::new(crate::settings::feishu_identity()))
    }
    #[cfg(target_family = "wasm")]
    {
        struct WasmUnsupported;
        impl FeishuTransport for WasmUnsupported {
            fn resolve_wiki(&self, _: &str, _: &Path) -> feishu::transport::TransportResult<feishu::WikiTarget> { Err(TransportError::NotInstalled("网页演示版不支持飞书同步".into())) }
            fn probe_revision(&self, _: &str, _: &Path) -> feishu::transport::TransportResult<feishu::DocProbe> { Err(TransportError::NotInstalled("网页演示版不支持飞书同步".into())) }
            fn fetch(&self, _: &str, _: &Path) -> feishu::transport::TransportResult<feishu::RawCloudDoc> { Err(TransportError::NotInstalled("网页演示版不支持飞书同步".into())) }
            fn download_media(&self, _: &str, _: &str, _: &Path) -> feishu::transport::TransportResult<PathBuf> { Err(TransportError::NotInstalled("网页演示版不支持飞书同步".into())) }
            fn apply_ops(&self, _: &str, _: Option<u64>, _: &[feishu::PushOp], _: &Path) -> feishu::transport::TransportResult<Option<u64>> { Err(TransportError::NotInstalled("网页演示版不支持飞书同步".into())) }
            fn create_doc(&self, _: Option<&str>, _: &str, _: &Path) -> feishu::transport::TransportResult<feishu::CreatedDoc> { Err(TransportError::NotInstalled("网页演示版不支持飞书同步".into())) }
            fn check_auth(&self, _: &Path) -> AuthInfo { AuthInfo { installed: false, detail: "网页演示版不支持飞书同步".into(), ..Default::default() } }
        }
        Arc::new(WasmUnsupported)
    }
}

fn assets_rel(root: &Path) -> &'static str {
    if Store::flat_at(root) { "assets" } else { ".assets" }
}

fn sep() -> &'static str {
    tr("；", "; ")
}

fn hhmm(secs: u64) -> String {
    let (_, _, _, h, mi) = crate::time::civil(secs);
    format!("{h:02}:{mi:02}")
}

fn identity_label(id: Identity) -> &'static str {
    match id {
        Identity::Auto => tr("自动", "Auto"),
        Identity::User => tr("用户", "User"),
        Identity::Bot => tr("机器人", "Bot"),
    }
}

fn next_identity(id: Identity) -> Identity {
    match id {
        Identity::Auto => Identity::User,
        Identity::User => Identity::Bot,
        Identity::Bot => Identity::Auto,
    }
}

// ---------------------------------------------------------------- Workspace helpers

impl Workspace {
    /// Locate `(root_key, root_path, SyncDir)` for document `id`.
    fn feishu_root_of(&self, id: &str) -> Option<(String, PathBuf, SyncDir)> {
        let key = self.store.root_of(id)?;
        let path = self.store.root_path(&key)?;
        let dir = SyncDir::for_root(&path, &key);
        Some((key, path, dir))
    }

    /// Locate target `(root_key, root_path, SyncDir)` for creating/importing under `parent`.
    fn feishu_target_root(&self, parent: Option<&str>) -> Option<(String, PathBuf, SyncDir)> {
        let key = match parent {
            Some(p) => self.store.root_of(p),
            None => self.store.active_root(),
        }?;
        let path = self.store.root_path(&key)?;
        let dir = SyncDir::for_root(&path, &key);
        Some((key, path, dir))
    }

    /// Refresh the in-memory map of all linked Feishu documents across every open root.
    pub(crate) fn refresh_feishu_links(&mut self) {
        let mut all = HashMap::new();
        for (key, _, path, err) in self.store.roots() {
            if err.is_some() {
                continue;
            }
            for (id, link) in SyncDir::for_root(&path, &key).list_links() {
                all.insert(id, link);
            }
        }
        self.feishu.links = all;
    }

    /// Refresh cached `lark-cli` auth status on a background thread.
    pub(crate) fn refresh_feishu_auth(&mut self, cx: &mut Context<Self>) {
        if self.feishu.auth_loading || cfg!(target_family = "wasm") {
            return;
        }
        let Some((_, root, _)) = self.feishu_target_root(None) else { return };
        self.feishu.auth_loading = true;
        let backend = transport();
        cx.spawn(async move |this, cx| {
            let info = cx.background_executor().spawn(async move { backend.check_auth(&root) }).await;
            let _ = this.update(cx, |w, cx| {
                w.feishu.auth = Some(info);
                w.feishu.auth_loading = false;
                cx.notify();
            });
        })
        .detach();
    }

    /// Current document contents (live editor copy including unsaved edits, else disk).
    fn feishu_doc_now(&self, id: &str, cx: &Context<Self>) -> anyhow::Result<Document> {
        if self.current.as_deref() == Some(id)
            && let Some(ed) = &self.editor
        {
            return Ok(ed.read(cx).core.doc.clone());
        }
        self.store.load(id)
    }

    /// Apply a remote-updated or merged document locally without triggering an
    /// echo auto-push or jumping the viewport.
    fn feishu_put_doc(&mut self, id: &str, doc: Document, window: &mut Window, cx: &mut Context<Self>) -> anyhow::Result<()> {
        self.feishu.suppress_echo.insert(id.to_string(), Instant::now() + ECHO_GUARD);
        if self.current.as_deref() == Some(id)
            && let Some(ed) = self.editor.clone()
        {
            let (title, icon, body, reminders, links) = (doc.title.text.clone(), doc.icon.clone(), doc.plain_text(), doc.reminders(), doc.doc_links());
            ed.update(cx, |e, cx| {
                e.restore_top = e.scroll_top();
                e.core.replace_document(doc);
                if e.read_only {
                    e.accept_read_only_contents();
                }
                e.changed_preserving_viewport(window, cx);
            });
            // Persist immediately so disk matches the merged state and doesn't stay dirty.
            let _ = self.flush(cx);
            if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
                e.title = title;
                e.icon = icon;
                e.body = body.into();
                e.reminders = reminders;
                e.links = links;
            }
            return Ok(());
        }
        let meta = self.store.save(id, &doc)?;
        if let Some(e) = self.entries.iter_mut().find(|e| e.id == id) {
            e.title = doc.title.text.clone();
            e.icon = doc.icon.clone();
            e.body = doc.plain_text().into();
            e.reminders = doc.reminders();
            e.links = doc.doc_links();
            e.meta = meta;
        }
        Ok(())
    }

    /// Save the remote cloud side of a conflict as a sibling document in the same folder.
    fn feishu_save_conflict_copy(&mut self, id: &str, mut cloud: Document) -> Option<String> {
        let (_, mo, d, h, mi) = crate::time::civil(crate::time::now());
        let title = crate::settings::display_title(&cloud.title.text);
        cloud.title = RichText::plain(crate::localized_format!(
            "{}（飞书云端 {:02}-{:02} {:02}:{:02}）",
            "{} (Feishu cloud {:02}-{:02} {:02}:{:02})",
            title, mo, d, h, mi
        ));
        let row = self.row_of(id);
        let parent = self.entries.iter().find(|e| e.id == row)
            .filter(|e| !e.meta.local())
            .and_then(|e| e.meta.parent.clone());
        let copy = self.store.create_from_in(&cloud, parent.as_deref()).ok()?;
        self.entries = self.store.list();
        self.sort();
        Some(copy)
    }

    /// Persist `link` + `base` and update the in-memory cache.
    fn feishu_record_state(&mut self, dir: &SyncDir, id: &str, link: &FeishuLink, base: Option<&Document>) -> anyhow::Result<()> {
        if let Some(doc) = base {
            dir.save_base(id, doc)?;
        }
        dir.save_link(id, link)?;
        self.feishu.links.insert(id.to_string(), link.clone());
        Ok(())
    }

    // ---------------------------------------------------------------- real-time hooks

    /// Called right after `Workspace::flush` writes `id` cleanly to disk.
    /// Schedules a debounced auto-push when real-time sync is active for `id`.
    pub(crate) fn feishu_on_saved(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(target_family = "wasm") || crate::devtools::scripted() {
            return;
        }
        if self.feishu.suppress_echo.get(id).is_some_and(|until| Instant::now() < *until) {
            return;
        }
        let Some(link) = self.feishu.links.get(id).cloned() else { return };
        if !link.auto_sync || link.revision == 0 || link.has_conflict() || link.paused_reason.is_some() {
            return;
        }
        if self.feishu.busy.contains(id) {
            self.feishu.pending_push.insert(id.to_string());
            self.feishu.live.insert(id.to_string(), LiveStatus::PendingPush);
            return;
        }
        self.feishu.live.insert(id.to_string(), LiveStatus::PendingPush);
        let doc_id = id.to_string();
        let task = cx.spawn_in(window, async move |this, cx| {
            cx.background_executor().timer(AUTO_PUSH_DELAY).await;
            let _ = this.update_in(cx, |w, window, cx| {
                w.feishu.push_timers.remove(&doc_id);
                w.feishu_sync(&doc_id, SyncMode::AutoPush, true, window, cx);
            });
        });
        self.feishu.push_timers.insert(id.to_string(), task);
    }

    /// Flush any pending debounced auto-push immediately (called on doc switch or window blur).
    pub(crate) fn feishu_flush_pending(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.feishu.push_timers.remove(id).is_some() {
            self.feishu_sync(id, SyncMode::AutoPush, true, window, cx);
        }
    }

    /// Called when a linked document is opened or the window regains focus:
    /// runs a lightweight revision probe and pulls only when the cloud moved.
    pub(crate) fn feishu_probe_and_pull(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(target_family = "wasm") || crate::devtools::scripted() || self.preview.is_some() {
            return;
        }
        if self.feishu.busy.contains(id) {
            return;
        }
        let Some(link) = self.feishu.links.get(id).cloned() else { return };
        if !link.auto_sync || link.revision == 0 || link.has_conflict() {
            return;
        }
        let Some((_, root, _)) = self.feishu_root_of(id) else { return };
        let backend = transport();
        let doc_id = id.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let probe = cx.background_executor().spawn(async move {
                feishu::check_remote_changed(backend.as_ref(), &root, &link)
            }).await;
            let _ = this.update_in(cx, |w, window, cx| match probe {
                Ok(Some(_)) => {
                    w.feishu.backoff_until = None;
                    w.feishu_sync(&doc_id, SyncMode::Pull, true, window, cx);
                }
                Ok(None) => {
                    w.feishu.backoff_until = None;
                    if matches!(w.feishu.live.get(&doc_id), Some(LiveStatus::Offline(_) | LiveStatus::Error(_))) {
                        w.feishu.live.insert(doc_id.clone(), LiveStatus::Synced);
                        cx.notify();
                    }
                }
                Err(TransportError::RateLimited(m)) => {
                    w.feishu.backoff_until = Some(Instant::now() + Duration::from_secs(60));
                    log::warn!("feishu probe rate limited: {m}");
                }
                Err(TransportError::Offline(m)) => {
                    w.feishu.backoff_until = Some(Instant::now() + Duration::from_secs(20));
                    w.feishu.live.insert(doc_id.clone(), LiveStatus::Offline(m));
                    cx.notify();
                }
                Err(TransportError::NotAuthenticated(m)) => {
                    w.feishu.backoff_until = Some(Instant::now() + Duration::from_secs(60));
                    w.feishu.live.insert(doc_id.clone(), LiveStatus::Error(m));
                    cx.notify();
                }
                Err(e) => {
                    w.feishu.backoff_until = Some(Instant::now() + Duration::from_secs(30));
                    log::warn!("feishu probe failed: {e}");
                }
            });
        })
        .detach();
    }

    /// Background real-time poll loop (started once per Workspace window).
    pub(crate) fn spawn_feishu_poller(&self, window: &mut Window, cx: &mut Context<Self>) {
        if cfg!(target_family = "wasm") || crate::devtools::scripted() {
            return;
        }
        cx.spawn_in(window, async move |this, cx| {
            loop {
                let active = this.update_in(cx, |_, window, _| window.is_window_active()).unwrap_or(false);
                let interval = if active { POLL_ACTIVE } else { POLL_IDLE };
                cx.background_executor().timer(interval).await;
                let ok = this.update_in(cx, |w, window, cx| {
                    if w.feishu.backoff_until.is_some_and(|t| Instant::now() < t) {
                        return;
                    }
                    if w.view != crate::app::View::Doc || w.preview.is_some() {
                        return;
                    }
                    if let Some(id) = w.current.clone() {
                        w.feishu_probe_and_pull(&id, window, cx);
                    }
                });
                if ok.is_err() {
                    break;
                }
            }
        })
        .detach();
    }

    // ---------------------------------------------------------------- sync actions

    /// Run a sync operation for document `id`.
    pub fn feishu_sync(&mut self, id: &str, mode: SyncMode, quiet: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.doc_menu = false;
        self.doc_sub = None;
        self.feishu.status_menu = false;
        self.feishu.push_timers.remove(id);
        if !self.feishu.busy.insert(id.to_string()) {
            if matches!(mode, SyncMode::Push | SyncMode::AutoPush | SyncMode::SyncNow) {
                self.feishu.pending_push.insert(id.to_string());
            }
            return;
        }
        self.feishu.live.insert(id.to_string(), LiveStatus::Syncing);
        if !quiet {
            self.show_toast_kind(ToastKind::Info, tr("正在同步飞书文档…", "Syncing with Feishu…"), cx);
        }
        cx.notify();
        let doc_id = id.to_string();
        cx.spawn_in(window, async move |this, cx| {
            let res = Self::feishu_run(this.clone(), doc_id.clone(), mode, cx).await;
            log::info!("feishu sync {mode:?} {doc_id}: {res:?}");
            let _ = this.update_in(cx, |w, window, cx| {
                w.feishu.busy.remove(&doc_id);
                let run_pending = w.feishu.pending_push.remove(&doc_id);
                match res {
                    Ok(Some(msg)) => {
                        w.feishu.live.insert(doc_id.clone(), LiveStatus::Synced);
                        w.feishu.backoff_until = None;
                        if !quiet || msg.contains("冲突") || msg.contains("conflict") || msg.contains("不支持") || msg.contains("unsupported") {
                            w.show_toast_kind(ToastKind::Info, &msg, cx);
                        }
                    }
                    Ok(None) => {
                        w.feishu.live.insert(doc_id.clone(), LiveStatus::Synced);
                        w.feishu.backoff_until = None;
                        if !quiet {
                            w.show_toast_kind(ToastKind::Info, tr("已是最新", "Already up to date"), cx);
                        }
                    }
                    Err(e) => {
                        let status = match &e {
                            TransportError::Offline(m) => {
                                w.feishu.backoff_until = Some(Instant::now() + Duration::from_secs(20));
                                LiveStatus::Offline(m.clone())
                            }
                            TransportError::RateLimited(m) => {
                                w.feishu.backoff_until = Some(Instant::now() + Duration::from_secs(60));
                                LiveStatus::Error(m.clone())
                            }
                            other => LiveStatus::Error(other.to_string()),
                        };
                        w.feishu.live.insert(doc_id.clone(), status);
                        if let Some((_, _, dir)) = w.feishu_root_of(&doc_id)
                            && let Some(mut l) = w.feishu.links.get(&doc_id).cloned()
                        {
                            l.last_error = Some(e.to_string());
                            let _ = w.feishu_record_state(&dir, &doc_id, &l, None);
                        }
                        if !quiet {
                            w.show_toast_kind(
                                ToastKind::Error,
                                &crate::localized_format!("飞书同步失败：{}", "Feishu sync failed: {}", e),
                                cx,
                            );
                        }
                    }
                }
                cx.notify();
                // Coalesced follow-up push if the user kept typing while sync ran.
                if run_pending
                    && w.feishu.links.get(&doc_id).is_some_and(|l| l.auto_sync && !l.has_conflict() && l.paused_reason.is_none())
                {
                    w.feishu_sync(&doc_id, SyncMode::AutoPush, true, window, cx);
                }
            });
        })
        .detach();
    }

    async fn feishu_run(this: WeakEntity<Self>, id: String, mode: SyncMode, cx: &mut AsyncWindowContext) -> Result<Option<String>, TransportError> {
        let (_, root, sync_dir) = this.update(cx, |w, _| w.feishu_root_of(&id))
            .map_err(|_| TransportError::Other("窗口已关闭".into()))?
            .ok_or_else(|| TransportError::Other("找不到文档所在的文档库".into()))?;
        let assets = assets_rel(&root);
        let backend = transport();
        let bg = cx.background_executor().clone();

        if mode == SyncMode::Create {
            let local = this.update(cx, |w, cx| w.feishu_doc_now(&id, cx))
                .map_err(|_| TransportError::Other("窗口已关闭".into()))?
                .map_err(|e| TransportError::Other(e.to_string()))?;
            let (r, d) = (root.clone(), local.clone());
            let ImportOutcome { mut link, cloud } = bg.spawn(async move {
                feishu::create_remote(backend.as_ref(), &r, assets, &d)
            }).await?;
            link.last_error = None;
            this.update_in(cx, |w, window, cx| {
                let live_now = w.feishu_doc_now(&id, cx).unwrap_or_else(|_| local.clone());
                let merged = feishu::merge_docs(&local, &live_now, &cloud.doc);
                if merged.remote_changed {
                    let _ = w.feishu_put_doc(&id, merged.doc, window, cx);
                }
                let _ = w.feishu_record_state(&sync_dir, &id, &link, Some(&cloud.doc));
            }).map_err(|_| TransportError::Other("窗口已关闭".into()))?;
            return Ok(Some(tr("已上传为新的飞书文档并开启实时同步", "Uploaded as a new Feishu document with real-time sync").into()));
        }

        let mut link = sync_dir.load_link(&id)
            .ok_or_else(|| TransportError::Other("这篇文档没有关联飞书文档".into()))?;
        let base = sync_dir.load_base(&id);
        let local = this.update(cx, |w, cx| w.feishu_doc_now(&id, cx))
            .map_err(|_| TransportError::Other("窗口已关闭".into()))?
            .map_err(|e| TransportError::Other(e.to_string()))?;

        match mode {
            SyncMode::Pull | SyncMode::TakeCloud => {
                let force = mode == SyncMode::TakeCloud;
                let (r, l, b, loc) = (root.clone(), link.clone(), base.clone(), local.clone());
                let outcome = bg.spawn(async move {
                    feishu::pull_doc(backend.as_ref(), &r, assets, &l, b.as_ref(), &loc, force)
                }).await?;
                match outcome {
                    PullOutcome::UpToDate { revision, media } => {
                        link.revision = revision;
                        link.media = media;
                        link.synced = crate::time::now();
                        link.last_error = None;
                        let _ = this.update(cx, |w, _| { let _ = w.feishu_record_state(&sync_dir, &id, &link, None); });
                        Ok(None)
                    }
                    PullOutcome::TakeCloud { cloud, media } => {
                        link.revision = cloud.revision.max(1);
                        link.media = media;
                        link.synced = crate::time::now();
                        link.last_error = None;
                        link.clear_conflict();
                        if link.paused_reason.as_deref().is_some_and(|p| p.starts_with("blocker:")) {
                            link.paused_reason = None;
                        }
                        this.update_in(cx, |w, window, cx| {
                            let _ = w.feishu_put_doc(&id, cloud.doc.clone(), window, cx);
                            let _ = w.feishu_record_state(&sync_dir, &id, &link, Some(&cloud.doc));
                        }).map_err(|_| TransportError::Other("窗口已关闭".into()))?;
                        Ok(Some(if force {
                            tr("已用云端版本覆盖本地", "Replaced the local version with the cloud version").into()
                        } else {
                            tr("已从飞书拉取文档", "Pulled the document from Feishu").into()
                        }))
                    }
                    PullOutcome::Merged { doc, cloud, media, remote_changed, conflict_count, .. } => {
                        link.revision = cloud.revision.max(1);
                        link.media = media;
                        link.synced = crate::time::now();
                        link.last_error = None;
                        let mut notes = vec![];
                        this.update_in(cx, |w, window, cx| {
                            if remote_changed {
                                let _ = w.feishu_put_doc(&id, doc.clone(), window, cx);
                                notes.push(tr("已同步飞书云端改动", "Merged Feishu cloud changes").to_string());
                            }
                            if conflict_count > 0 {
                                let copy_id = w.feishu_save_conflict_copy(&id, cloud.doc.clone());
                                link.conflict_count = conflict_count;
                                link.conflict_copy = copy_id;
                                link.paused_reason = Some("conflict".into());
                                notes.push(crate::localized_format!(
                                    "{} 处改动两边同时修改：保留了本地并已暂停自动推送，云端副本已另存",
                                    "{} conflicting edits: kept local and paused auto-push; cloud copy saved",
                                    conflict_count
                                ));
                            }
                            let _ = w.feishu_record_state(&sync_dir, &id, &link, Some(&cloud.doc));
                        }).map_err(|_| TransportError::Other("窗口已关闭".into()))?;
                        Ok((!notes.is_empty()).then(|| notes.join(sep())))
                    }
                    PullOutcome::NeedsInitialChoice => {
                        Err(TransportError::Other("还没有同步过：请先选择“用云端覆盖本地”或“用本地覆盖云端”".into()))
                    }
                }
            }
            SyncMode::Push | SyncMode::AutoPush | SyncMode::SyncNow | SyncMode::TakeLocal => {
                let force_local = mode == SyncMode::TakeLocal;
                let (r, l, b, loc) = (root.clone(), link.clone(), base.clone(), local.clone());
                let outcome = bg.spawn(async move {
                    feishu::push_doc(backend.as_ref(), &r, assets, &l, b.as_ref(), &loc, force_local)
                }).await?;
                match outcome {
                    PushOutcome::NoChanges { revision, media, merged_doc, new_base } => {
                        link.revision = revision;
                        link.media = media;
                        link.synced = crate::time::now();
                        link.last_error = None;
                        if link.paused_reason.as_deref().is_some_and(|p| p.starts_with("blocker:")) {
                            link.paused_reason = None;
                        }
                        let mut msg = None;
                        this.update_in(cx, |w, window, cx| {
                            if let Some(doc) = merged_doc {
                                let _ = w.feishu_put_doc(&id, doc, window, cx);
                                msg = Some(tr("已同步飞书云端改动", "Merged Feishu cloud changes").to_string());
                            }
                            let _ = w.feishu_record_state(&sync_dir, &id, &link, Some(&new_base));
                        }).map_err(|_| TransportError::Other("窗口已关闭".into()))?;
                        Ok(msg)
                    }
                    PushOutcome::Pushed { ops_sent, after, media, merged_from_cloud, sent_target } => {
                        link.revision = after.revision.max(1);
                        link.media = media;
                        link.synced = crate::time::now();
                        link.last_error = None;
                        link.clear_conflict();
                        if link.paused_reason.as_deref().is_some_and(|p| p.starts_with("blocker:")) {
                            link.paused_reason = None;
                        }
                        this.update_in(cx, |w, window, cx| {
                            // Preserve any keystrokes the user typed while the push was in flight!
                            if let Ok(live_now) = w.feishu_doc_now(&id, cx) {
                                let post = feishu::merge_docs(&sent_target, &live_now, &after.doc);
                                if post.remote_changed {
                                    let _ = w.feishu_put_doc(&id, post.doc, window, cx);
                                }
                                if post.local_changed {
                                    w.feishu.pending_push.insert(id.clone());
                                }
                            }
                            let _ = w.feishu_record_state(&sync_dir, &id, &link, Some(&after.doc));
                        }).map_err(|_| TransportError::Other("窗口已关闭".into()))?;
                        let mut notes = vec![crate::localized_format!("已推送 {} 处改动到飞书", "Pushed {} changes to Feishu", ops_sent)];
                        if merged_from_cloud {
                            notes.push(tr("同时合并了云端改动", "also merged cloud changes").into());
                        }
                        Ok(Some(notes.join(sep())))
                    }
                    PushOutcome::ConflictsPaused { merged_doc, cloud, media, conflict_count } => {
                        link.revision = cloud.revision.max(1);
                        link.media = media;
                        link.synced = crate::time::now();
                        link.last_error = None;
                        this.update_in(cx, |w, window, cx| {
                            let _ = w.feishu_put_doc(&id, merged_doc, window, cx);
                            let copy_id = w.feishu_save_conflict_copy(&id, cloud.doc.clone());
                            link.conflict_count = conflict_count;
                            link.conflict_copy = copy_id;
                            link.paused_reason = Some("conflict".into());
                            let _ = w.feishu_record_state(&sync_dir, &id, &link, Some(&cloud.doc));
                        }).map_err(|_| TransportError::Other("窗口已关闭".into()))?;
                        Ok(Some(crate::localized_format!(
                            "{} 处改动两边同时修改：保留了本地并已暂停推送，云端版本已另存",
                            "{} conflicting edits: kept local and paused push; cloud copy saved",
                            conflict_count
                        )))
                    }
                    PushOutcome::Blocked { reasons } => {
                        let summary = reasons.join("、");
                        link.paused_reason = Some(format!("blocker:{summary}"));
                        let _ = this.update(cx, |w, _| { let _ = w.feishu_record_state(&sync_dir, &id, &link, None); });
                        Ok(Some(crate::localized_format!(
                            "有飞书不支持的内容，暂未推送：{}",
                            "Skipped push due to Feishu-unsupported blocks: {}",
                            summary
                        )))
                    }
                    PushOutcome::NeedsInitialChoice => {
                        Err(TransportError::Other("还没有同步过：请先选择“用云端覆盖本地”或“用本地覆盖云端”".into()))
                    }
                }
            }
            SyncMode::Create => unreachable!(),
        }
    }

    // ---------------------------------------------------------------- link / import / resolve

    /// Open the "从飞书链接导入…" dialog.
    pub fn open_feishu_import(&mut self, parent: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        self.doc_menu = false;
        self.doc_sub = None;
        self.row_menu = None;
        self.mode_menu = false;
        self.import_menu = None;
        self.create_menu = None;
        self.feishu.status_menu = false;
        let field = cx.new(|cx| TextField::new(tr("粘贴飞书 / Lark 文档或知识库链接", "Paste a Feishu / Lark document or wiki URL"), cx));
        // Auto-prefill when clipboard already holds a Feishu / Lark URL!
        if let Some(clip) = cx.read_from_clipboard().and_then(|c| c.text())
            && feishu::link::looks_like_feishu_url(&clip)
        {
            field.update(cx, |f, cx| {
                f.set_text(clip.trim(), cx);
                f.select_all_text(cx);
            });
        }
        window.focus(&field.focus_handle(cx), cx);
        let subs = vec![cx.subscribe_in(&field, window, |w, _, ev: &TextFieldEvent, window, cx| match ev {
            TextFieldEvent::Submit => w.feishu_dialog_submit(SyncMode::TakeCloud, window, cx),
            TextFieldEvent::Cancel => { w.feishu.dlg = None; cx.notify(); }
            _ => {}
        })];
        self.feishu.dlg = Some(FeishuDialog { kind: DialogKind::Import { parent }, field, busy: false, error: None, _subs: subs });
        self.refresh_feishu_auth(cx);
        cx.notify();
    }

    /// Open the "关联飞书文档…" dialog for an existing local document `id`.
    pub fn open_feishu_link(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        self.doc_menu = false;
        self.doc_sub = None;
        self.row_menu = None;
        self.mode_menu = false;
        self.feishu.status_menu = false;
        let blank = self.feishu_doc_now(id, cx).is_ok_and(|d| feishu::is_blank(&d));
        let field = cx.new(|cx| TextField::new(tr("粘贴飞书 / Lark 文档或知识库链接", "Paste a Feishu / Lark document or wiki URL"), cx));
        if let Some(clip) = cx.read_from_clipboard().and_then(|c| c.text())
            && feishu::link::looks_like_feishu_url(&clip)
        {
            field.update(cx, |f, cx| {
                f.set_text(clip.trim(), cx);
                f.select_all_text(cx);
            });
        }
        window.focus(&field.focus_handle(cx), cx);
        let subs = vec![cx.subscribe_in(&field, window, |w, _, ev: &TextFieldEvent, window, cx| match ev {
            TextFieldEvent::Submit if w.feishu.dlg.as_ref().is_some_and(|d| matches!(d.kind, DialogKind::Link { blank: true, .. })) => {
                w.feishu_dialog_submit(SyncMode::TakeCloud, window, cx);
            }
            TextFieldEvent::Cancel => { w.feishu.dlg = None; cx.notify(); }
            _ => {}
        })];
        self.feishu.dlg = Some(FeishuDialog { kind: DialogKind::Link { id: id.to_string(), blank }, field, busy: false, error: None, _subs: subs });
        self.refresh_feishu_auth(cx);
        cx.notify();
    }

    fn feishu_dialog_submit(&mut self, mode: SyncMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some(d) = &self.feishu.dlg else { return };
        if d.busy {
            return;
        }
        let url = d.field.read(cx).text.trim().to_string();
        if let Err(e) = feishu::parse_link(&url) {
            if let Some(d) = self.feishu.dlg.as_mut() {
                d.error = Some(crate::settings::diagnostic(&e.to_string()));
            }
            cx.notify();
            return;
        }
        let kind = d.kind.clone();
        match kind {
            DialogKind::Import { parent } => self.feishu_import_url(url, parent, window, cx),
            DialogKind::Link { id, .. } => self.feishu_link_existing(&id, url, mode, window, cx),
        }
    }

    /// Import a Feishu / Lark URL as a brand-new linked local document (or jump
    /// straight to the already-linked document if present in this root).
    fn feishu_import_url(&mut self, url: String, parent: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, root, sync_dir)) = self.feishu_target_root(parent.as_deref()) else {
            if let Some(d) = self.feishu.dlg.as_mut() {
                d.error = Some(crate::shared_store::no_root().to_string());
            }
            cx.notify();
            return;
        };
        if let Some(d) = self.feishu.dlg.as_mut() {
            d.busy = true;
            d.error = None;
        }
        cx.notify();
        let assets = assets_rel(&root);
        let backend = transport();
        cx.spawn_in(window, async move |this, cx| {
            let r = cx.background_executor().spawn(async move {
                feishu::import_from_link(backend.as_ref(), &root, assets, &url)
            }).await;
            let _ = this.update_in(cx, |w, window, cx| match r {
                Ok(ImportOutcome { link, cloud }) => {
                    w.feishu.dlg = None;
                    // If this Feishu doc is already linked in this root, open and refresh it!
                    if let Some(existing_id) = sync_dir.find_by_token(&link.token)
                        && w.entries.iter().any(|e| e.id == existing_id)
                    {
                        w.open(&existing_id, window, cx);
                        w.feishu_sync(&existing_id, SyncMode::Pull, false, window, cx);
                        return;
                    }
                    let doc = cloud.doc.clone();
                    let created = match parent.as_deref() {
                        Some(p) => w.store.create_from_in(&doc, Some(p)),
                        None => w.store.create_from(&doc),
                    };
                    match created {
                        Ok(new_id) => {
                            let _ = w.feishu_record_state(&sync_dir, &new_id, &link, Some(&cloud.doc));
                            w.entries = w.store.list();
                            w.sort();
                            w.reveal_in_tree(&new_id);
                            w.open(&new_id, window, cx);
                            w.feishu.live.insert(new_id, LiveStatus::Synced);
                            w.show_toast_kind(ToastKind::Info, tr("已导入飞书文档并开启实时双向同步", "Imported Feishu document with real-time sync"), cx);
                        }
                        Err(e) => {
                            w.show_toast_kind(ToastKind::Error, &crate::localized_format!("创建本地文档失败：{}", "Could not create local document: {}", e), cx);
                        }
                    }
                    cx.notify();
                }
                Err(e) => {
                    if let Some(d) = w.feishu.dlg.as_mut() {
                        d.busy = false;
                        d.error = Some(crate::settings::diagnostic(&e.to_string()));
                    }
                    cx.notify();
                }
            });
        })
        .detach();
    }

    fn feishu_link_existing(&mut self, id: &str, url: String, mode: SyncMode, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, root, sync_dir)) = self.feishu_root_of(id) else {
            if let Some(d) = self.feishu.dlg.as_mut() {
                d.error = Some(crate::shared_store::no_root().to_string());
            }
            cx.notify();
            return;
        };
        let pasted = match feishu::parse_link(&url) {
            Ok(p) => p,
            Err(e) => {
                if let Some(d) = self.feishu.dlg.as_mut() {
                    d.error = Some(crate::settings::diagnostic(&e.to_string()));
                }
                cx.notify();
                return;
            }
        };
        match pasted {
            feishu::PastedLink::Docx { token, url } => {
                let link = FeishuLink { url, token, auto_sync: true, ..Default::default() };
                if let Err(e) = self.feishu_record_state(&sync_dir, id, &link, None) {
                    if let Some(d) = self.feishu.dlg.as_mut() {
                        d.error = Some(e.to_string());
                    }
                    cx.notify();
                    return;
                }
                self.feishu.dlg = None;
                self.feishu_sync(id, mode, false, window, cx);
                cx.notify();
            }
            feishu::PastedLink::Wiki { node_token, url } => {
                if let Some(d) = self.feishu.dlg.as_mut() {
                    d.busy = true;
                    d.error = None;
                }
                cx.notify();
                let doc_id = id.to_string();
                let backend = transport();
                cx.spawn_in(window, async move |this, cx| {
                    let node = node_token.clone();
                    let resolved = cx.background_executor().spawn(async move {
                        backend.resolve_wiki(&node, &root)
                    }).await;
                    let _ = this.update_in(cx, |w, window, cx| match resolved {
                        Ok(target) => {
                            let link = FeishuLink { url, token: target.obj_token, wiki_node: node_token, auto_sync: true, ..Default::default() };
                            match w.feishu_record_state(&sync_dir, &doc_id, &link, None) {
                                Ok(()) => {
                                    w.feishu.dlg = None;
                                    w.feishu_sync(&doc_id, mode, false, window, cx);
                                }
                                Err(e) => {
                                    if let Some(d) = w.feishu.dlg.as_mut() {
                                        d.busy = false;
                                        d.error = Some(e.to_string());
                                    }
                                }
                            }
                            cx.notify();
                        }
                        Err(e) => {
                            if let Some(d) = w.feishu.dlg.as_mut() {
                                d.busy = false;
                                d.error = Some(crate::settings::diagnostic(&e.to_string()));
                            }
                            cx.notify();
                        }
                    });
                })
                .detach();
            }
        }
    }

    pub fn feishu_toggle_auto(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, _, dir)) = self.feishu_root_of(id) else { return };
        let Some(mut link) = self.feishu.links.get(id).cloned() else { return };
        link.auto_sync = !link.auto_sync;
        let on = link.auto_sync;
        let _ = self.feishu_record_state(&dir, id, &link, None);
        if !on {
            self.feishu.push_timers.remove(id);
            self.feishu.pending_push.remove(id);
            self.show_toast_kind(ToastKind::Info, tr("已暂停飞书实时同步（仍可手动同步）", "Paused Feishu real-time sync"), cx);
        } else {
            self.show_toast_kind(ToastKind::Info, tr("已开启飞书实时双向同步", "Enabled Feishu real-time bidirectional sync"), cx);
            if !link.has_conflict() && link.revision > 0 {
                self.feishu_sync(id, SyncMode::SyncNow, true, window, cx);
            }
        }
        cx.notify();
    }

    pub fn feishu_cycle_identity(&mut self, cx: &mut Context<Self>) {
        let next = next_identity(crate::settings::feishu_identity());
        self.set_pref(|p| p.feishu_identity = next.as_pref().into(), cx);
        self.feishu.backoff_until = None;
        self.refresh_feishu_auth(cx);
        self.show_toast_kind(
            ToastKind::Info,
            &crate::localized_format!("飞书身份模式已切换为：{}", "Feishu identity mode: {}", identity_label(next)),
            cx,
        );
        cx.notify();
    }

    pub fn feishu_resolve_keep_local(&mut self, id: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some((_, _, dir)) = self.feishu_root_of(id) else { return };
        let Some(mut link) = self.feishu.links.get(id).cloned() else { return };
        link.clear_conflict();
        let _ = self.feishu_record_state(&dir, id, &link, None);
        self.feishu_sync(id, SyncMode::TakeLocal, false, window, cx);
        cx.notify();
    }

    pub fn feishu_open_in_browser(&mut self, id: &str, cx: &mut Context<Self>) {
        self.doc_menu = false;
        self.doc_sub = None;
        self.feishu.status_menu = false;
        if let Some(url) = self.feishu.links.get(id).and_then(FeishuLink::open_url) {
            cx.open_url(&url);
        }
        cx.notify();
    }

    pub fn feishu_unlink(&mut self, id: &str, cx: &mut Context<Self>) {
        if let Some((_, _, dir)) = self.feishu_root_of(id) {
            dir.remove(id);
        }
        self.feishu.links.remove(id);
        self.feishu.live.remove(id);
        self.feishu.push_timers.remove(id);
        self.feishu.pending_push.remove(id);
        self.feishu.status_menu = false;
        self.doc_menu = false;
        self.doc_sub = None;
        self.show_toast_kind(ToastKind::Info, tr("已取消关联飞书文档", "Unlinked from Feishu"), cx);
        cx.notify();
    }

    // ---------------------------------------------------------------- UI: top bar status pill

    /// Clickable real-time sync status pill placed in the document header bar.
    pub(crate) fn render_feishu_status_pill(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let id = self.current.as_deref()?;
        let link = self.feishu.links.get(id)?;
        let live = self.feishu.live_of(id);
        let open = self.feishu.status_menu;
        let record = self.feishu.status_bounds.clone();

        let (dot_color, label_text, tip_text): (Hsla, String, String) = if link.has_conflict() {
            (
                theme::c(craft::O400),
                crate::localized_format!("飞书冲突 ({})", "Conflict ({})", link.conflict_count),
                tr("飞书云端与本地存在冲突，点击处理", "Conflict between local and Feishu cloud; click to resolve").into(),
            )
        } else if let Some(reason) = link.paused_reason.as_deref().and_then(|r| r.strip_prefix("blocker:")) {
            (
                theme::c(craft::O400),
                tr("飞书 · 未支持块", "Feishu · Blocked").into(),
                reason.to_string(),
            )
        } else {
            match &live {
                LiveStatus::Syncing => (
                    Theme::primary_500(),
                    tr("飞书 · 同步中…", "Feishu · Syncing…").into(),
                    tr("正在与飞书云文档同步", "Syncing with Feishu cloud document").into(),
                ),
                LiveStatus::PendingPush => (
                    theme::c(craft::O400),
                    tr("飞书 · 待推送", "Feishu · Pending").into(),
                    tr("本地有改动，停笔后将自动同步到飞书", "Local changes queued for automatic push to Feishu").into(),
                ),
                LiveStatus::Offline(err) => (
                    color::destructive(),
                    tr("飞书 · 离线", "Feishu · Offline").into(),
                    err.clone(),
                ),
                LiveStatus::Error(err) => (
                    color::destructive(),
                    tr("飞书 · 同步出错", "Feishu · Error").into(),
                    err.clone(),
                ),
                LiveStatus::Synced if !link.auto_sync => (
                    color::muted_foreground(),
                    tr("飞书 · 已暂停", "Feishu · Paused").into(),
                    tr("已暂停实时同步，点击手动同步或恢复", "Real-time sync paused; click to manage").into(),
                ),
                LiveStatus::Synced => {
                    let when = if link.synced > 0 { hhmm(link.synced) } else { String::new() };
                    let tip = if link.revision > 0 && !when.is_empty() {
                        crate::localized_format!("实时双向同步中 · rev {} · {} 同步", "Real-time sync on · rev {} · synced {}", link.revision, when)
                    } else {
                        tr("已关联飞书文档，点击管理同步", "Linked to Feishu; click to manage sync").into()
                    };
                    (crate::design::chip::colors(1).1, tr("飞书 · 已同步", "Feishu · Synced").into(), tip)
                }
            }
        };

        Some(
            div()
                .id("feishu-status-pill")
                .no_drag()
                .relative()
                .h(px(26.))
                .px(px(9.))
                .flex_none()
                .flex()
                .items_center()
                .gap(px(6.))
                .rounded(px(geo().badge_r))
                .border_1()
                .border_color(if open { color::ring() } else { color::border() })
                .bg(if open { color::accent() } else { color::secondary() })
                .hover(|s| s.bg(color::accent()))
                .cursor_pointer()
                .text_size(px(12.))
                .line_height(px(16.))
                .text_color(color::foreground())
                .when(!open, |d| d.tip(&tip_text, None))
                .child(div().size(px(7.)).rounded(px(99.)).flex_none().bg(dot_color))
                .child(div().whitespace_nowrap().child(label_text))
                .child(canvas(move |b, _, _| record.set(Some(b)), |_, _, _, _| {}).absolute().inset_0())
                .on_mouse_down(MouseButton::Left, cx.listener(move |w, _, _, cx| {
                    cx.stop_propagation();
                    w.feishu.status_menu = !open;
                    if w.feishu.status_menu {
                        w.doc_menu = false;
                        w.doc_sub = None;
                        w.refresh_feishu_auth(cx);
                    }
                    cx.notify();
                }))
                .into_any_element(),
        )
    }

    /// Popover hanging below the header status pill.
    pub(crate) fn render_feishu_status_menu(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.feishu.status_menu {
            return None;
        }
        let id = self.current.clone()?;
        let link = self.feishu.links.get(&id)?;
        let busy = self.feishu.is_busy(&id);
        let vw = window.viewport_size().width;
        let anchor = self.feishu.status_bounds.get();
        let right = anchor.map_or(vw - px(140.), |b| b.right());
        let top = anchor.map_or(px(48.), |b| b.bottom() + px(6.));
        let menu = self.feishu_linked_popover("feishu-status-popover", &id, link, busy, cx)
            .on_mouse_down_out(cx.listener(|w, _, _, cx| {
                w.feishu.status_menu = false;
                cx.notify();
            }));
        Some(
            deferred(anchored().anchor(Anchor::TopRight).position(point(right, top)).snap_to_window_with_margin(px(8.)).child(menu))
                .with_priority(3)
                .into_any_element(),
        )
    }

    // ---------------------------------------------------------------- UI: conflict / blocker banner

    /// Banner rendered right under the top bar when a linked document has an
    /// unresolved conflict or cloud-unsupported block pausing auto-push.
    pub(crate) fn render_feishu_banner(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        if self.preview.is_some() {
            return None;
        }
        let id = self.current.clone()?;
        let link = self.feishu.links.get(&id)?;
        if link.has_conflict() {
            let copy_id = link.conflict_copy.clone();
            let id_cloud = id.clone();
            let id_local = id.clone();
            let msg = crate::localized_format!(
                "飞书云端与本地同时修改了同一处（{} 处冲突），已保留本地并暂停自动推送",
                "Local and Feishu cloud edited the same section ({} conflicts); kept local and paused auto-push",
                link.conflict_count.max(1)
            );
            return Some(
                div()
                    .id("feishu-conflict-banner")
                    .flex_none()
                    .min_h(px(40.))
                    .px(px(16.))
                    .py(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .bg(theme::ca(craft::O50, 0.7))
                    .border_b_1()
                    .border_color(Theme::line_divider())
                    .child(div().size(px(8.)).rounded(px(99.)).flex_none().bg(theme::c(craft::O400)))
                    .child(div().flex_1().min_w_0().text_size(px(12.)).text_color(Theme::text()).child(msg))
                    .when_some(copy_id, |d, cid| {
                        d.child(
                            Button::outline("feishu-view-copy")
                                .sized(ButtonSize::Tool)
                                .label(tr("查看云端副本", "Open cloud copy"))
                                .on_click(cx.listener(move |w, _, window, cx| w.open(&cid, window, cx))),
                        )
                    })
                    .child(
                        Button::outline("feishu-take-cloud-banner")
                            .sized(ButtonSize::Tool)
                            .label(tr("采用云端版本", "Use cloud"))
                            .on_click(cx.listener(move |w, _, window, cx| w.feishu_sync(&id_cloud, SyncMode::TakeCloud, false, window, cx))),
                    )
                    .child(
                        Button::primary("feishu-keep-local-banner")
                            .sized(ButtonSize::Tool)
                            .label(tr("保留本地并恢复同步", "Keep local & resume"))
                            .on_click(cx.listener(move |w, _, window, cx| w.feishu_resolve_keep_local(&id_local, window, cx))),
                    )
                    .into_any_element(),
            );
        }
        if let Some(reason) = link.paused_reason.as_deref().and_then(|r| r.strip_prefix("blocker:")) {
            let id_retry = id.clone();
            return Some(
                div()
                    .id("feishu-blocker-banner")
                    .flex_none()
                    .min_h(px(38.))
                    .px(px(16.))
                    .py(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .bg(theme::ca(craft::O50, 0.5))
                    .border_b_1()
                    .border_color(Theme::line_divider())
                    .child(div().flex_1().min_w_0().text_size(px(12.)).text_color(Theme::text())
                        .child(crate::localized_format!("自动推送已暂停（含飞书不支持的块）：{}", "Auto-push paused (unsupported blocks): {}", reason)))
                    .child(
                        Button::outline("feishu-retry-blocker")
                            .sized(ButtonSize::Tool)
                            .label(tr("重试推送", "Retry push"))
                            .on_click(cx.listener(move |w, _, window, cx| w.feishu_sync(&id_retry, SyncMode::Push, false, window, cx))),
                    )
                    .into_any_element(),
            );
        }
        None
    }

    // ---------------------------------------------------------------- UI: "…" submenu & popover

    fn feishu_linked_popover(&self, pop_id: &'static str, id: &str, link: &FeishuLink, busy: bool, cx: &mut Context<Self>) -> Popover {
        let mut menu = Popover::new(pop_id).menu().min_w(px(250.));
        let kind_text = if link.is_wiki() { tr("飞书知识库", "Feishu Wiki") } else { tr("飞书文档", "Feishu Doc") };
        let header = if link.revision > 0 && link.synced > 0 {
            crate::localized_format!("{} · rev {} · {}", "{} · rev {} · {}", kind_text, link.revision, hhmm(link.synced))
        } else {
            crate::localized_format!("{} · 尚未同步", "{} · Not synced yet", kind_text)
        };
        menu = menu.child(MenuLabel::new(header));
        if let Some(err) = &link.last_error {
            menu = menu.child(
                div().px(px(10.)).pb(px(4.)).text_size(px(11.)).line_height(px(15.)).text_color(color::destructive()).child(err.clone()),
            );
        }
        if busy {
            return menu.child(MenuItem::new("fs-busy").label(tr("正在同步…", "Syncing…")).disabled(true));
        }
        let act = |item_id: &'static str, glyph: &'static str, label: &'static str, mode: SyncMode| {
            let doc_id = id.to_string();
            MenuItem::new(item_id).icon(glyph).label(label).on_click(cx.listener(move |w, _, window, cx| w.feishu_sync(&doc_id, mode, false, window, cx)))
        };
        let auto_id = id.to_string();
        let open_id = id.to_string();
        let unlink_id = id.to_string();
        let cur_identity = crate::settings::feishu_identity();

        menu = menu
            .child(
                MenuItem::new("fs-auto")
                    .icon(if link.auto_sync { "play-filled" } else { "pause-filled" })
                    .label(tr("实时双向同步", "Real-time two-way sync"))
                    .child(Switch::new(link.auto_sync))
                    .on_click(cx.listener(move |w, _, window, cx| w.feishu_toggle_auto(&auto_id, window, cx))),
            )
            .child(MenuSeparator::new());

        if link.revision > 0 {
            menu = menu
                .child(act("fs-sync-now", "restore", tr("立即双向同步", "Sync now"), SyncMode::SyncNow))
                .child(act("fs-pull", "download", tr("拉取云端更新", "Pull from Feishu"), SyncMode::Pull))
                .child(act("fs-push", "upload", tr("推送到飞书", "Push to Feishu"), SyncMode::Push));
        }

        menu = menu
            .when(link.open_url().is_some(), |d| {
                d.child(
                    MenuItem::new("fs-open-web")
                        .icon("globe")
                        .label(tr("在飞书中打开", "Open in Feishu"))
                        .on_click(cx.listener(move |w, _, _, cx| w.feishu_open_in_browser(&open_id, cx))),
                )
            })
            .child(MenuSeparator::new())
            .child(act("fs-take-cloud", "download", tr("用云端覆盖本地", "Overwrite local with cloud"), SyncMode::TakeCloud))
            .child(act("fs-take-local", "upload", tr("用本地覆盖云端", "Overwrite cloud with local"), SyncMode::TakeLocal))
            .child(MenuSeparator::new())
            .child(
                MenuItem::new("fs-identity")
                    .icon("user")
                    .label(crate::localized_format!("身份：{}", "Identity: {}", identity_label(cur_identity)))
                    .on_click(cx.listener(|w, _, _, cx| w.feishu_cycle_identity(cx))),
            )
            .child(
                MenuItem::new("fs-unlink")
                    .icon("unlink")
                    .label(tr("取消关联", "Unlink"))
                    .destructive(true)
                    .on_click(cx.listener(move |w, _, _, cx| w.feishu_unlink(&unlink_id, cx))),
            );
        menu
    }

    /// Submenu shown under `…` → `飞书同步`.
    pub(crate) fn render_feishu_submenu(&self, id: &str, cx: &mut Context<Self>) -> Popover {
        if let Some(link) = self.feishu.links.get(id) {
            return self.feishu_linked_popover("dm-feishu-menu", id, link, self.feishu.is_busy(id), cx);
        }
        let link_id = id.to_string();
        let create_id = id.to_string();
        let parent = self.entries.iter().find(|e| e.id == id).and_then(|e| e.meta.parent.clone());
        Popover::new("dm-feishu-menu")
            .menu()
            .min_w(px(230.))
            .child(
                MenuItem::new("fs-import-new")
                    .icon("import")
                    .label(tr("从飞书链接导入新文档…", "Import from Feishu link…"))
                    .on_click(cx.listener(move |w, _, window, cx| w.open_feishu_import(parent.clone(), window, cx))),
            )
            .child(
                MenuItem::new("fs-link")
                    .icon("link")
                    .label(tr("关联已有飞书文档…", "Link existing Feishu doc…"))
                    .on_click(cx.listener(move |w, _, window, cx| w.open_feishu_link(&link_id, window, cx))),
            )
            .child(
                MenuItem::new("fs-create")
                    .icon("upload")
                    .label(tr("上传为新的飞书文档", "Upload as new Feishu doc"))
                    .on_click(cx.listener(move |w, _, window, cx| w.feishu_sync(&create_id, SyncMode::Create, false, window, cx))),
            )
    }

    // ---------------------------------------------------------------- UI: import / link dialog

    pub(crate) fn render_feishu_dialog(&self, window: &Window, cx: &mut Context<Self>) -> Option<AnyElement> {
        let d = self.feishu.dlg.as_ref()?;
        let (title, description, is_import, blank) = match &d.kind {
            DialogKind::Import { .. } => (
                tr("从飞书链接导入", "Import from Feishu"),
                tr("粘贴飞书或 Lark 的 /docx/ 或 /wiki/ 链接，将正文与图片导入本地，并自动开启实时双向同步。",
                   "Paste a Feishu / Lark /docx/ or /wiki/ URL to import locally with real-time two-way sync."),
                true,
                true,
            ),
            DialogKind::Link { blank, .. } => (
                tr("关联飞书文档", "Link a Feishu document"),
                if *blank {
                    tr("关联后会拉取云端正文与图片，并开启实时双向同步：本地编辑自动增量回传（保留原文档评论），云端更新自动合并。",
                       "Pulls cloud contents and enables real-time two-way sync while preserving Feishu comments.")
                } else {
                    tr("这篇本地文档已有内容，首次关联请选择以哪一边为准；另一边将被覆盖，随后开启实时双向同步。",
                       "This document already has content. Choose which side wins on first sync; real-time sync starts afterwards.")
                },
                false,
                *blank,
            ),
        };

        let cur_id = crate::settings::feishu_identity();
        let auth_note = match &self.feishu.auth {
            Some(info) if !info.installed => Some((true, tr("未检测到 lark-cli，请先运行：npm i -g @larksuite/cli && lark-cli auth login", "lark-cli not found. Install with: npm i -g @larksuite/cli && lark-cli auth login").to_string())),
            Some(info) if !info.ready_for(cur_id) => Some((true, tr("飞书用户身份未登录，请在终端运行：lark-cli auth login（或点击下方切换身份）", "Feishu user not logged in. Run `lark-cli auth login` (or switch identity below)").to_string())),
            Some(info) => {
                let who = if !info.user_name.is_empty() { info.user_name.clone() } else { info.brand.clone() };
                Some((false, crate::localized_format!("已就绪（{} · 身份：{}）", "Ready ({} · identity: {})", who, identity_label(cur_id))))
            }
            None => None,
        };

        let focused = d.field.read(cx).is_focused(window);
        let busy = d.busy;
        let footer = div()
            .flex()
            .items_center()
            .justify_between()
            .gap(px(8.))
            .child(
                Button::ghost("feishu-dlg-identity")
                    .sized(ButtonSize::Sm)
                    .label(crate::localized_format!("身份：{}", "Identity: {}", identity_label(cur_id)))
                    .on_click(cx.listener(|w, _, _, cx| w.feishu_cycle_identity(cx))),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(
                        Button::outline("feishu-cancel")
                            .label(tr("取消", "Cancel"))
                            .min_w(px(76.))
                            .on_click(cx.listener(|w, _, _, cx| { w.feishu.dlg = None; cx.notify(); })),
                    )
                    .when(is_import || blank, |div_el| {
                        div_el.child(
                            Button::primary("feishu-ok")
                                .label(if busy {
                                    tr("正在导入…", "Importing…")
                                } else if is_import {
                                    tr("导入并开启同步", "Import & sync")
                                } else {
                                    tr("关联并拉取", "Link & pull")
                                })
                                .disabled(busy)
                                .min_w(px(110.))
                                .on_click(cx.listener(|w, _, window, cx| w.feishu_dialog_submit(SyncMode::TakeCloud, window, cx))),
                        )
                    })
                    .when(!is_import && !blank, |div_el| {
                        div_el
                            .child(
                                Button::outline("feishu-take-local")
                                    .label(tr("用本地覆盖云端", "Overwrite cloud"))
                                    .disabled(busy)
                                    .on_click(cx.listener(|w, _, window, cx| w.feishu_dialog_submit(SyncMode::TakeLocal, window, cx))),
                            )
                            .child(
                                Button::primary("feishu-take-cloud")
                                    .label(tr("用云端覆盖本地", "Overwrite local"))
                                    .disabled(busy)
                                    .on_click(cx.listener(|w, _, window, cx| w.feishu_dialog_submit(SyncMode::TakeCloud, window, cx))),
                            )
                    }),
            );

        let dialog = Dialog::new("feishu-dialog")
            .w(px(520.))
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .title(title)
            .description(description)
            .closable(cx.listener(|w, _, _, cx| { w.feishu.dlg = None; cx.notify(); }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(8.))
                    .child(
                        Field::new("feishu-url-field")
                            .focused(focused)
                            .invalid(d.error.is_some())
                            .disabled(busy)
                            .child(div().flex_1().min_w_0().child(d.field.clone())),
                    )
                    .when_some(auth_note, |el, (warn, text)| {
                        el.child(
                            div()
                                .text_size(px(12.))
                                .line_height(px(18.))
                                .text_color(if warn { theme::c(craft::O400) } else { color::muted_foreground() })
                                .child(text),
                        )
                    })
                    .when_some(d.error.clone(), |el, err| {
                        el.child(div().text_size(px(12.)).line_height(px(18.)).text_color(color::destructive()).child(err))
                    }),
            )
            .footer(footer);

        Some(
            deferred(
                scrim()
                    .id("feishu-mask")
                    .occlude()
                    .on_mouse_down(MouseButton::Left, cx.listener(|w, _, _, cx| {
                        if w.feishu.dlg.as_ref().is_none_or(|d| !d.busy) {
                            w.feishu.dlg = None;
                            cx.notify();
                        }
                    }))
                    .child(dialog),
            )
            .with_priority(10)
            .into_any_element(),
        )
    }

    // ---------------------------------------------------------------- UI: sidebar badge

    /// Tiny subtle cloud-sync indicator shown on Feishu-linked sidebar rows.
    pub(crate) fn render_feishu_sidebar_mark(&self, id: &str) -> Option<AnyElement> {
        let link = self.feishu.links.get(id)?;
        let tint = if link.has_conflict() || link.paused_reason.is_some() {
            theme::c(craft::O400)
        } else if matches!(self.feishu.live.get(id), Some(LiveStatus::Offline(_) | LiveStatus::Error(_))) {
            color::destructive()
        } else if link.auto_sync {
            Theme::primary_500()
        } else {
            Theme::icon()
        };
        Some(div().ml(px(4.)).flex_none().flex().items_center().child(icon("globe", 12., tint)).into_any_element())
    }
}
