//! High-level bidirectional Feishu sync workflows (GPUI-free).
//!
//! All network / CLI I/O goes through [`FeishuTransport`], while local state
//! persistence and UI updates stay in the caller (`src/feishu.rs`).

use super::codec::{CloudDoc, MediaMap, cloud_xml, image_tokens, is_blank, new_local_images};
use super::link::{PastedLink, parse_link};
use super::planner::{blockers, blockers_in, merge_docs, plan_push_sent};
use super::state::FeishuLink;
use super::transport::{FeishuTransport, TransportError, TransportResult};
use crate::model::Document;
use crate::vfs;
use std::path::{Path, PathBuf};

/// Outcome of importing a Feishu `/docx/` or `/wiki/` link as a new local document.
#[derive(Clone, Debug)]
pub struct ImportOutcome {
    pub link: FeishuLink,
    pub cloud: CloudDoc,
}

/// Outcome of pulling remote updates for a linked document.
#[derive(Clone, Debug)]
pub enum PullOutcome {
    /// Cloud revision and content match what we already have.
    UpToDate { revision: u64, media: MediaMap },
    /// Initial pull into a blank local doc, or explicit overwrite from cloud.
    TakeCloud { cloud: CloudDoc, media: MediaMap },
    /// Three-way merged local + remote.
    Merged {
        doc: Document,
        cloud: CloudDoc,
        media: MediaMap,
        remote_changed: bool,
        local_changed: bool,
        conflict_count: usize,
    },
    /// First sync on a non-blank local document with no merge base yet.
    NeedsInitialChoice,
}

/// Outcome of pushing local changes to Feishu.
#[derive(Clone, Debug)]
pub enum PushOutcome {
    /// Nothing differed from the cloud.
    NoChanges { revision: u64, media: MediaMap, merged_doc: Option<Document>, new_base: Document },
    /// Pushed `ops_sent` incremental block operations and refreshed the base.
    Pushed {
        ops_sent: usize,
        after: CloudDoc,
        media: MediaMap,
        /// Cloud changes merged in before or during the push (when any).
        merged_from_cloud: bool,
        /// The exact document snapshot that was sent (caller merges against live editor).
        sent_target: Document,
    },
    /// Pull-before-push discovered conflicts: cloud changes merged locally (keeping
    /// local in conflict regions), push paused until resolved.
    ConflictsPaused {
        merged_doc: Document,
        cloud: CloudDoc,
        media: MediaMap,
        conflict_count: usize,
    },
    /// Blocked by local-only blocks inside the changed range.
    Blocked { reasons: Vec<String> },
    /// First sync on a non-blank local document with no merge base yet.
    NeedsInitialChoice,
}

/// Download any cloud image tokens missing from `media` into `assets_rel` under `root`.
/// Newly uploaded local files whose bytes match a downloaded file reuse the local path.
fn download_missing_media(
    transport: &dyn FeishuTransport,
    root: &Path,
    assets_rel: &str,
    xml: &str,
    media: &mut MediaMap,
    uploaded: &[String],
) -> TransportResult<()> {
    let todo: Vec<String> = image_tokens(xml).into_iter().filter(|t| !media.contains_key(t)).collect();
    if todo.is_empty() {
        return Ok(());
    }
    let assets_dir = root.join(assets_rel);
    let _ = vfs::create_dir_all(&assets_dir);
    let canonical_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

    const WORKERS: usize = 4;
    let mut results: Vec<(usize, TransportResult<PathBuf>)> = std::thread::scope(|s| {
        let count = WORKERS.min(todo.len());
        let handles: Vec<_> = (0..count).map(|w| {
            let todo_ref = &todo;
            s.spawn(move || {
                (w..todo_ref.len()).step_by(count).map(|i| {
                    let rel_out = format!("{assets_rel}/feishu-{}", todo_ref[i]);
                    (i, transport.download_media(&todo_ref[i], &rel_out, root))
                }).collect::<Vec<_>>()
            })
        }).collect();
        handles.into_iter().flat_map(|h| h.join().expect("feishu media download thread")).collect()
    });
    results.sort_by_key(|(i, _)| *i);

    for (token, res) in todo.into_iter().zip(results.into_iter().map(|(_, r)| r)) {
        let saved = res?;
        let saved_canon = saved.canonicalize().unwrap_or_else(|_| saved.clone());
        let rel = saved_canon.strip_prefix(&canonical_root)
            .or_else(|_| saved.strip_prefix(root))
            .map(|p| p.to_string_lossy().replace('\\', "/"))
            .unwrap_or_else(|_| {
                let s = saved_canon.to_string_lossy().replace('\\', "/");
                s.strip_prefix("//?/").unwrap_or(&s).to_string()
            });
        let bytes = vfs::read(&saved_canon).ok();
        let same = uploaded.iter().find(|p| bytes.is_some() && vfs::read(root.join(p)).ok() == bytes);
        if let Some(existing) = same {
            let _ = vfs::remove_file(&saved_canon);
            media.insert(token, existing.clone());
        } else {
            media.insert(token, rel);
        }
    }
    Ok(())
}

/// Fetch cloud DocxXML, download missing images and parse into a [`CloudDoc`].
pub fn fetch_cloud(
    transport: &dyn FeishuTransport,
    root: &Path,
    assets_rel: &str,
    token: &str,
    media: &mut MediaMap,
    uploaded: &[String],
) -> TransportResult<CloudDoc> {
    let raw = transport.fetch(token, root)?;
    download_missing_media(transport, root, assets_rel, &raw.xml, media, uploaded)?;
    CloudDoc::parse(&raw.xml, raw.revision, media).map_err(|e| TransportError::Other(e.to_string()))
}

/// Resolve a pasted `/docx/` or `/wiki/` link and fetch the document for local import.
pub fn import_from_link(
    transport: &dyn FeishuTransport,
    root: &Path,
    assets_rel: &str,
    input_url: &str,
) -> TransportResult<ImportOutcome> {
    let pasted = parse_link(input_url).map_err(|e| TransportError::Other(e.to_string()))?;
    let (token, wiki_node, url) = match pasted {
        PastedLink::Docx { token, url } => (token, String::new(), url),
        PastedLink::Wiki { node_token, url } => {
            let resolved = transport.resolve_wiki(&node_token, root)?;
            (resolved.obj_token, node_token, url)
        }
    };
    let mut media = MediaMap::new();
    let cloud = fetch_cloud(transport, root, assets_rel, &token, &mut media, &[])?;
    let link = FeishuLink {
        url,
        token,
        wiki_node,
        revision: cloud.revision.max(1),
        media,
        synced: crate::time::now(),
        auto_sync: true,
        paused_reason: None,
        conflict_copy: None,
        conflict_count: 0,
        last_error: None,
    };
    Ok(ImportOutcome { link, cloud })
}

/// Fast 1-RTT probe: returns `Some(new_revision)` when the cloud revision moved
/// past `link.revision`, or `None` when unchanged.
pub fn check_remote_changed(transport: &dyn FeishuTransport, root: &Path, link: &FeishuLink) -> TransportResult<Option<u64>> {
    let probe = transport.probe_revision(&link.token, root)?;
    if probe.revision > 0 && probe.revision == link.revision {
        Ok(None)
    } else {
        Ok(Some(probe.revision))
    }
}

/// Pull remote changes and merge against `base` + `local`.
pub fn pull_doc(
    transport: &dyn FeishuTransport,
    root: &Path,
    assets_rel: &str,
    link: &FeishuLink,
    base: Option<&Document>,
    local: &Document,
    force_cloud: bool,
) -> TransportResult<PullOutcome> {
    let mut media = link.media.clone();
    let cloud = fetch_cloud(transport, root, assets_rel, &link.token, &mut media, &[])?;

    if force_cloud {
        return Ok(PullOutcome::TakeCloud { cloud, media });
    }
    let Some(base) = base.filter(|_| link.revision > 0) else {
        if is_blank(local) {
            return Ok(PullOutcome::TakeCloud { cloud, media });
        }
        return Ok(PullOutcome::NeedsInitialChoice);
    };

    let merged = merge_docs(base, local, &cloud.doc);
    if !merged.remote_changed && cloud.revision == link.revision {
        return Ok(PullOutcome::UpToDate { revision: cloud.revision.max(1), media });
    }
    let conflict_count = merged.conflict_count();
    Ok(PullOutcome::Merged {
        doc: merged.doc,
        cloud,
        media,
        remote_changed: merged.remote_changed,
        local_changed: merged.local_changed,
        conflict_count,
    })
}

/// Pull + merge (unless `force_local`), then push minimal incremental block ops
/// to Feishu with optimistic revision locking and automatic retry on race.
pub fn push_doc(
    transport: &dyn FeishuTransport,
    root: &Path,
    assets_rel: &str,
    link: &FeishuLink,
    base: Option<&Document>,
    local: &Document,
    force_local: bool,
) -> TransportResult<PushOutcome> {
    const MAX_ATTEMPTS: usize = 2;
    let mut media = link.media.clone();
    let mut cur_local = local.clone();
    let mut cur_base = base.cloned();
    let mut cur_rev = link.revision;
    let mut merged_from_cloud = false;

    for attempt in 0..MAX_ATTEMPTS {
        let remote = fetch_cloud(transport, root, assets_rel, &link.token, &mut media, &[])?;
        let target = if force_local {
            cur_local.clone()
        } else {
            let Some(b) = cur_base.as_ref().filter(|_| cur_rev > 0) else {
                return Ok(PushOutcome::NeedsInitialChoice);
            };
            let m = merge_docs(b, &cur_local, &remote.doc);
            if m.remote_changed {
                merged_from_cloud = true;
            }
            if m.has_conflicts() {
                let conflict_count = m.conflict_count();
                return Ok(PushOutcome::ConflictsPaused {
                    merged_doc: m.doc,
                    cloud: remote,
                    media,
                    conflict_count,
                });
            }
            m.doc
        };

        let (ops, sent_ranges) = plan_push_sent(&remote, &target, &media)
            .map_err(|e| TransportError::Other(e.to_string()))?;
        if ops.is_empty() {
            return Ok(PushOutcome::NoChanges {
                revision: remote.revision.max(1),
                media,
                merged_doc: merged_from_cloud.then_some(target),
                new_base: remote.doc,
            });
        }
        let blocked = blockers_in(&target.blocks, &sent_ranges);
        if !blocked.is_empty() {
            return Ok(PushOutcome::Blocked { reasons: blocked });
        }

        let uploaded = new_local_images(&target, &media);
        let expected = (!force_local && remote.revision > 0).then_some(remote.revision);
        match transport.apply_ops(&link.token, expected, &ops, root) {
            Ok(_) => {
                let after = fetch_cloud(transport, root, assets_rel, &link.token, &mut media, &uploaded)?;
                let extra_cloud = after.revision != remote.revision + ops.len() as u64 && remote.revision > 0;
                return Ok(PushOutcome::Pushed {
                    ops_sent: ops.len(),
                    after,
                    media,
                    merged_from_cloud: merged_from_cloud || extra_cloud,
                    sent_target: target,
                });
            }
            Err(TransportError::RevisionConflict(_)) if attempt + 1 < MAX_ATTEMPTS && !force_local => {
                // Cloud moved between fetch and apply: update baseline and retry once.
                cur_base = Some(remote.doc);
                cur_local = target;
                cur_rev = remote.revision.max(1);
                continue;
            }
            Err(e) => return Err(e),
        }
    }

    Err(TransportError::RevisionConflict("云端正在持续编辑，请稍后重试".into()))
}

/// Upload a local document as a brand-new Feishu cloud document and link it.
pub fn create_remote(
    transport: &dyn FeishuTransport,
    root: &Path,
    assets_rel: &str,
    doc: &Document,
) -> TransportResult<ImportOutcome> {
    let blocked = blockers(&doc.blocks);
    if !blocked.is_empty() {
        return Err(TransportError::Other(format!("文档里有飞书不支持的内容，未上传：{}", blocked.join("、"))));
    }
    let mut media = MediaMap::new();
    let uploaded = new_local_images(doc, &media);
    let xml = cloud_xml(doc, &media);
    let title = (!doc.title.text.trim().is_empty()).then_some(doc.title.text.as_str());
    let created = transport.create_doc(title, &xml, root)?;
    let cloud = fetch_cloud(transport, root, assets_rel, &created.token, &mut media, &uploaded)?;
    let link = FeishuLink {
        url: created.url,
        token: created.token,
        wiki_node: String::new(),
        revision: cloud.revision.max(1),
        media,
        synced: crate::time::now(),
        auto_sync: true,
        paused_reason: None,
        conflict_copy: None,
        conflict_count: 0,
        last_error: None,
    };
    Ok(ImportOutcome { link, cloud })
}
