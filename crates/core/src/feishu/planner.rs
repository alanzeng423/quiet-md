//! Three-way merging (wrapping `fdl_core::merge::merge3` for Feishu's
//! local-only attributes), minimal incremental block push planning (preserving
//! Feishu comments and untouched cloud blocks), and cloud compatibility checks.

use super::codec::{CloudDoc, MediaMap, for_cloud, keys, preserve_folds, strip_fold};
use crate::docxml;
use crate::merge::{Conflict, Merge, merge3};
use crate::model::*;
use anyhow::{Result, anyhow};
use std::ops::Range;

/// Result of three-way merging a local document with a fetched Feishu version.
#[derive(Clone, Debug)]
pub struct MergeOutcome {
    pub doc: Document,
    pub local_changed: bool,
    pub remote_changed: bool,
    pub conflicts: Vec<Conflict>,
    pub title_conflict: bool,
}

impl MergeOutcome {
    pub fn has_conflicts(&self) -> bool {
        !self.conflicts.is_empty() || self.title_conflict
    }

    pub fn conflict_count(&self) -> usize {
        self.conflicts.len()
    }
}

/// Three-way merge `base`, `local` and `remote` using `fdl_core::merge::merge3`.
///
/// Local-only presentation state (`icon`, `cover`, and block `folded` flags) is
/// preserved and ignored when deciding whether a side changed, preventing echo
/// loops and false conflicts.
pub fn merge_docs(base: &Document, local: &Document, remote: &Document) -> MergeOutcome {
    let (kb, kl, kr) = (keys(&base.blocks), keys(&local.blocks), keys(&remote.blocks));
    let title_str = |d: &Document| docxml::serialize_inline(&d.title);
    let (tb, tl, tr) = (title_str(base), title_str(local), title_str(remote));
    let local_changed = kl != kb || tl != tb;
    let remote_changed = kr != kb || tr != tb;

    if !remote_changed {
        return MergeOutcome { doc: local.clone(), local_changed, remote_changed: false, conflicts: vec![], title_conflict: false };
    }
    if !local_changed {
        let mut doc = remote.clone();
        doc.icon = local.icon.clone();
        doc.cover = local.cover.clone();
        preserve_folds(&local.blocks, &mut doc.blocks);
        return MergeOutcome { doc, local_changed: false, remote_changed: true, conflicts: vec![], title_conflict: false };
    }

    // Align local-only fields so merge3 only sees content changes.
    let mut b_norm = strip_fold(base);
    let mut l_norm = strip_fold(local);
    let mut r_norm = strip_fold(remote);
    b_norm.icon = local.icon.clone();
    b_norm.cover = local.cover.clone();
    l_norm.icon = local.icon.clone();
    l_norm.cover = local.cover.clone();
    r_norm.icon = local.icon.clone();
    r_norm.cover = local.cover.clone();

    match merge3(&b_norm, &l_norm, &r_norm) {
        Merge::Clean(mut doc) => {
            preserve_folds(&local.blocks, &mut doc.blocks);
            MergeOutcome { doc, local_changed, remote_changed, conflicts: vec![], title_conflict: false }
        }
        Merge::Conflicts { mut merged, regions } => {
            let title_conflict = regions.iter().any(|c| matches!(c, Conflict::Title));
            preserve_folds(&local.blocks, &mut merged.blocks);
            MergeOutcome { doc: merged, local_changed, remote_changed, conflicts: regions, title_conflict }
        }
    }
}

// ---------------------------------------------------------------- push plan

/// One incremental block operation against `/open-apis/docs_ai/v1/documents/{id}`.
///
/// Uses only single-`block_id` (`block_replace`, `block_insert_after`) and
/// comma-separated `block_delete`, which are universally supported across all
/// `lark-cli` v1.0.x releases and preserve untouched blocks' Feishu comments.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PushOp {
    /// Replace the document's `<title>` block (anchored on `title_id`).
    Title { id: String, xml: String },
    /// `block_replace --block-id <block_id> --content <xml>`.
    Replace { block_id: String, xml: String },
    /// `block_insert_after --block-id <anchor> --content <xml>` (`"0"` = start).
    InsertAfter { anchor: String, xml: String },
    /// `block_delete --block-id <id1,id2,…>`.
    Delete { block_ids: Vec<String> },
}

/// Longest common subsequence: matched index pairs, strictly increasing.
fn lcs(a: &[String], b: &[String]) -> Vec<(usize, usize)> {
    let pre = a.iter().zip(b).take_while(|(x, y)| x == y).count();
    let suf = a[pre..].iter().rev().zip(b[pre..].iter().rev()).take_while(|(x, y)| x == y).count();
    let (am, bm) = (&a[pre..a.len() - suf], &b[pre..b.len() - suf]);
    let mut out: Vec<(usize, usize)> = (0..pre).map(|i| (i, i)).collect();
    if !am.is_empty() && !bm.is_empty() && am.len() * bm.len() <= 4_000_000 {
        let (n, m) = (am.len(), bm.len());
        let mut t = vec![0u32; (n + 1) * (m + 1)];
        for i in (0..n).rev() {
            for j in (0..m).rev() {
                t[i * (m + 1) + j] = if am[i] == bm[j] { t[(i + 1) * (m + 1) + j + 1] + 1 } else { t[(i + 1) * (m + 1) + j].max(t[i * (m + 1) + j + 1]) };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < n && j < m {
            if am[i] == bm[j] {
                out.push((pre + i, pre + j));
                i += 1;
                j += 1;
            } else if t[(i + 1) * (m + 1) + j] >= t[i * (m + 1) + j + 1] {
                i += 1;
            } else {
                j += 1;
            }
        }
    }
    out.extend((0..suf).map(|k| (a.len() - suf + k, b.len() - suf + k)));
    out
}

/// Plan the minimal sequence of [`PushOp`]s turning `remote` into `target`.
pub fn plan_push(remote: &CloudDoc, target: &Document, media: &MediaMap) -> Result<Vec<PushOp>> {
    Ok(plan_push_sent(remote, target, media)?.0)
}

/// [`plan_push`], plus the ranges of `target.blocks` whose XML will actually be
/// sent to the cloud (so [`blockers_in`] only validates changed blocks).
pub fn plan_push_sent(remote: &CloudDoc, target: &Document, media: &MediaMap) -> Result<(Vec<PushOp>, Vec<Range<usize>>)> {
    let mut out = Vec::new();
    let mut ranges = Vec::new();
    let mut sent = target.clone();
    for_cloud(&mut sent.blocks, media);

    if docxml::serialize_inline(&remote.doc.title) != docxml::serialize_inline(&target.title) {
        if remote.title_id.is_empty() {
            return Err(anyhow!("云端文档缺少标题 block id，无法更新标题"));
        }
        out.push(PushOp::Title {
            id: remote.title_id.clone(),
            xml: format!("<title>{}</title>", docxml::serialize_inline(&target.title)),
        });
    }

    // Special case: cloud document had zero real blocks (synthetic placeholder in CloudDoc::parse).
    if remote.ids.iter().all(Option::is_none) {
        let xml = docxml::serialize_blocks_compact(&sent.blocks);
        if !sent.blocks.is_empty() && !is_single_empty_p(&target.blocks) {
            ranges.push(0..target.blocks.len());
            out.push(PushOp::InsertAfter { anchor: "0".into(), xml });
        }
        return Ok((out, ranges));
    }

    let (kr, kt) = (keys(&remote.doc.blocks), keys(&target.blocks));
    let id = |i: usize| remote.ids.get(i).and_then(|o| o.clone()).ok_or_else(|| anyhow!("云端第 {} 个块没有 id", i + 1));
    let (mut ir, mut it) = (0, 0);
    for (sr, st) in lcs(&kr, &kt).into_iter().chain([(kr.len(), kt.len())]) {
        if it < st {
            ranges.push(it..st);
        }
        match (ir < sr, it < st) {
            (true, true) => {
                let xml = docxml::serialize_blocks_compact(&sent.blocks[it..st]);
                // Multi-block replacement: delete trailing blocks first so `ir` stays untouched,
                // then replace `ir` in place with all replacement blocks.
                if sr - ir > 1 {
                    let extras: Result<Vec<String>> = (ir + 1..sr).map(id).collect();
                    out.push(PushOp::Delete { block_ids: extras? });
                }
                out.push(PushOp::Replace { block_id: id(ir)?, xml });
            }
            (true, false) => {
                let ids: Result<Vec<String>> = (ir..sr).map(id).collect();
                out.push(PushOp::Delete { block_ids: ids? });
            }
            (false, true) => {
                let xml = docxml::serialize_blocks_compact(&sent.blocks[it..st]);
                let anchor = if ir == 0 { "0".into() } else { id(ir - 1)? };
                out.push(PushOp::InsertAfter { anchor, xml });
            }
            (false, false) => {}
        }
        (ir, it) = (sr + 1, st + 1);
    }
    Ok((out, ranges))
}

fn is_single_empty_p(blocks: &[Block]) -> bool {
    blocks.len() == 1 && matches!(blocks[0].kind, BlockKind::Text) && blocks[0].text.text.is_empty() && blocks[0].children.is_empty()
}

// ---------------------------------------------------------------- blockers

/// Cloud-incompatible constructs across the whole document.
pub fn blockers(blocks: &[Block]) -> Vec<String> {
    check_blockers(blocks, |_| true)
}

/// Cloud-incompatible constructs restricted to `ranges` (the blocks a push sends).
pub fn blockers_in(blocks: &[Block], ranges: &[Range<usize>]) -> Vec<String> {
    check_blockers(blocks, |i| ranges.iter().any(|r| r.contains(&i)))
}

fn check_blockers(blocks: &[Block], checked: impl Fn(usize) -> bool) -> Vec<String> {
    let mut out = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        if !checked(i) {
            continue;
        }
        let n = i + 1;
        let xml = docxml::serialize_blocks_compact(std::slice::from_ref(b));
        if xml.contains("<countdown") {
            out.push(format!("第 {n} 块：倒计时只在 Quiet.md 里有，飞书文档里没有对应的块"));
        }
        if xml.contains("is-whole-day=\"true\"") {
            out.push(format!("第 {n} 块：全天提醒只在 Quiet.md 里有"));
        }
        if matches!(b.kind, BlockKind::Board(_)) || xml.contains("<whiteboard type=\"excalidraw\"") {
            out.push(format!("第 {n} 块：Excalidraw 白板只在 Quiet.md 本地支持，暂不能直接推送到飞书"));
        }
        if matches!(b.kind, BlockKind::DataTable(_)) || xml.contains("<datatable") {
            out.push(format!("第 {n} 块：本地 SQLite 数据表暂不能推送到飞书"));
        }
        if matches!(b.kind, BlockKind::SubPageList) || xml.contains("<sub-page-list") {
            out.push(format!("第 {n} 块：子页面列表只在 Quiet.md 本地支持"));
        }
        if matches!(b.kind, BlockKind::Callout { .. })
            && let Some(c) = b.children.iter().find(|c| !matches!(c.kind, BlockKind::Text | BlockKind::Bullet | BlockKind::Ordered { .. } | BlockKind::Todo { .. }))
        {
            out.push(format!("第 {n} 块：飞书的高亮块里只能放正文、列表和待办，不能放{}", kind_name(&c.kind)));
        }
    }
    out
}

fn kind_name(k: &BlockKind) -> &'static str {
    match k {
        BlockKind::Heading { .. } => "标题",
        BlockKind::Code { .. } => "代码块",
        BlockKind::Quote => "引用",
        BlockKind::Divider => "分割线",
        BlockKind::Image { .. } => "图片",
        BlockKind::Table(_) => "表格",
        BlockKind::Grid | BlockKind::Column { .. } => "分栏",
        BlockKind::File(_) => "文件",
        BlockKind::Board(_) => "白板",
        BlockKind::DataTable(_) => "数据表",
        _ => "这种块",
    }
}
