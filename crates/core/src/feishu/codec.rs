//! Cloud ↔ local DocxXML normalization, block-ID extraction, palette mapping,
//! fold preservation and media token rewriting.

use crate::docxml;
use crate::model::*;
use crate::palette;
use anyhow::{Result, anyhow, bail};
use quick_xml::Reader;
use quick_xml::Writer;
use quick_xml::events::{BytesStart, Event};
use std::collections::BTreeMap;

/// Cloud image token → library-relative local path (e.g. `.assets/feishu-<tok>.png`).
pub type MediaMap = BTreeMap<String, String>;

/// A fetched cloud document, cleaned and parsed with its cloud block IDs aligned.
#[derive(Clone, Debug)]
pub struct CloudDoc {
    pub revision: u64,
    /// The `<title>`'s cloud id (the document id).
    pub title_id: String,
    /// One cloud block id per top-level block of `doc`, in order.
    pub ids: Vec<Option<String>>,
    pub doc: Document,
}

impl CloudDoc {
    /// Parse `xml` returned by `docs_ai/v1/.../fetch` with `export_block_id: true`.
    pub fn parse(xml: &str, revision: u64, media: &MediaMap) -> Result<CloudDoc> {
        let (clean, title_id, mut ids) = clean_cloud_xml(xml)?;
        let mut doc = docxml::parse(&clean)?;
        // A truly empty cloud document with `<title>…</title>` and zero body
        // blocks is given a single empty paragraph by `docxml::parse`.
        if ids.is_empty() && doc.blocks.len() == 1 && matches!(doc.blocks[0].kind, BlockKind::Text) && doc.blocks[0].text.text.is_empty() && doc.blocks[0].children.is_empty() {
            ids.push(None);
        }
        if doc.blocks.len() != ids.len() {
            bail!("云端文档有 {} 个顶层块，解析出 {} 个，无法对齐", ids.len(), doc.blocks.len());
        }
        localize_media(&mut doc.blocks, media);
        Ok(CloudDoc { revision, title_id: title_id.unwrap_or_default(), ids, doc })
    }
}

/// All unique cloud image tokens referenced by `<img src="…">` in raw cloud XML.
pub fn image_tokens(xml: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut r = Reader::from_str(xml);
    loop {
        match r.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) if e.name().as_ref() == b"img" => {
                if let Some(v) = e.attributes().flatten().find(|a| a.key.as_ref() == b"src").and_then(|a| a.unescape_value().ok())
                    && !v.is_empty()
                    && !v.starts_with("http://")
                    && !v.starts_with("https://")
                    && !out.contains(&v.to_string())
                {
                    out.push(v.into_owned());
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Drops cloud `id` / `seq-marker` / `comment-ids` attributes (returning the
/// title id and each top-level block's id), maps `rgb()`/`rgba()` colors back
/// to DocxXML palette names, and trims the trailing newline Feishu appends to
/// captions.
fn clean_cloud_xml(xml: &str) -> Result<(String, Option<String>, Vec<Option<String>>)> {
    let mut r = Reader::from_str(xml);
    r.config_mut().trim_text(false);
    let mut w = Writer::new(Vec::new());
    let mut stack: Vec<String> = Vec::new();
    let (mut title_id, mut ids) = (None, Vec::new());
    loop {
        let ev = r.read_event().map_err(|e| anyhow!("云端 XML 解析失败：{e}"))?;
        match ev {
            Event::Start(ref e) | Event::Empty(ref e) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).into_owned();
                let (el, id) = clean_element(e, &name, stack.last().map(String::as_str))?;
                let parent = stack.last().map(String::as_str);
                let top = stack.is_empty() && name != "ul" && name != "ol";
                let item = stack.len() == 1 && matches!(parent, Some("ul" | "ol")) && name == "li";
                if stack.is_empty() && name == "title" {
                    title_id = id;
                } else if top || item {
                    ids.push(id);
                }
                if matches!(ev, Event::Start(_)) {
                    stack.push(name);
                    w.write_event(Event::Start(el))?;
                } else {
                    w.write_event(Event::Empty(el))?;
                }
            }
            Event::End(e) => {
                stack.pop();
                w.write_event(Event::End(e))?;
            }
            Event::Eof => break,
            other => w.write_event(other)?,
        }
    }
    Ok((String::from_utf8(w.into_inner())?, title_id, ids))
}

fn clean_element(e: &BytesStart, name: &str, parent: Option<&str>) -> Result<(BytesStart<'static>, Option<String>)> {
    let mut el = BytesStart::new(name.to_string());
    let mut id = None;
    for a in e.attributes() {
        let a = a?;
        let key = String::from_utf8_lossy(a.key.as_ref()).into_owned();
        let mut val = a.unescape_value()?.into_owned();
        match key.as_str() {
            "id" => {
                id = Some(val);
                continue;
            }
            // Feishu's computed numbering and comment anchor metadata: not local content.
            "seq-marker" | "comment-ids" => continue,
            "caption" => val = val.trim_end_matches('\n').to_string(),
            "text-color" | "background-color" | "border-color" => {
                if let Some(n) = color_name(name, parent, &key, &val) {
                    val = n;
                }
            }
            _ => {}
        }
        el.push_attribute((key.as_str(), val.as_str()));
    }
    Ok((el, id))
}

fn parse_rgb(v: &str) -> Option<palette::Rgba> {
    let inner = v.strip_prefix("rgba(").or_else(|| v.strip_prefix("rgb("))?.strip_suffix(')')?;
    let n: Vec<&str> = inner.split(',').map(str::trim).collect();
    let c = |i: usize| n.get(i)?.parse::<u8>().ok();
    let a = match n.get(3) { Some(a) => a.parse().ok()?, None => 1. };
    Some(palette::rgba(c(0)?, c(1)?, c(2)?, a))
}

fn color_name(tag: &str, _parent: Option<&str>, attr: &str, v: &str) -> Option<String> {
    let got = parse_rgb(v)?;
    let names: Vec<&str> = palette::HUES.iter().copied()
        .chain(palette::HUES.iter().map(|h| match *h {
            "gray" => "light-gray",
            "red" => "light-red",
            "orange" => "light-orange",
            "yellow" => "light-yellow",
            "green" => "light-green",
            "blue" => "light-blue",
            _ => "light-purple",
        }))
        .chain(["medium-gray"])
        .collect();
    let tables: &[fn(&str) -> Option<palette::Rgba>] = match (tag, attr) {
        (_, "text-color") => &[palette::text_color],
        ("callout", "background-color") => &[palette::callout_bg, palette::block_bg],
        ("callout", "border-color") => &[palette::callout_border_swatch, palette::callout_border],
        ("span", "background-color") => &[palette::bg_color],
        (_, "background-color") => &[palette::block_bg, palette::bg_color],
        _ => return None,
    };
    let close = |c: palette::Rgba| c.r.abs_diff(got.r) <= 3 && c.g.abs_diff(got.g) <= 3 && c.b.abs_diff(got.b) <= 3 && (c.a - got.a).abs() <= 0.03;
    tables.iter().find_map(|t| names.iter().find(|n| t(n).is_some_and(close)).map(|n| n.to_string()))
}

// ---------------------------------------------------------------- media & folding

pub(crate) fn walk_mut(blocks: &mut [Block], f: &mut dyn FnMut(&mut Block)) {
    for b in blocks {
        f(b);
        if let BlockKind::Table(t) = &mut b.kind {
            for row in &mut t.rows {
                for cell in row {
                    walk_mut(&mut cell.blocks, f);
                }
            }
        }
        walk_mut(&mut b.children, f);
    }
}

/// Replace cloud image tokens that already have local files with `path` references.
pub fn localize_media(blocks: &mut [Block], media: &MediaMap) {
    walk_mut(blocks, &mut |b| {
        if let BlockKind::Image { src_kind, src, .. } = &mut b.kind
            && src_kind == "src"
            && let Some(p) = media.get(src.as_str())
        {
            *src_kind = "path".into();
            *src = p.clone();
        }
    });
}

/// Rewrite local image paths for sending to Feishu: known images go back to
/// their cloud token; brand-new local files become `path="@./…"` so `lark-cli`
/// uploads and binds them relative to the library folder.
pub fn cloudify_media(blocks: &mut [Block], media: &MediaMap) {
    walk_mut(blocks, &mut |b| {
        if let BlockKind::Image { src_kind, src, .. } = &mut b.kind
            && src_kind == "path"
        {
            let norm = src.replace('\\', "/");
            if let Some((t, _)) = media.iter().find(|(_, p)| **p == norm) {
                *src_kind = "src".into();
                *src = t.clone();
            } else if !norm.starts_with("@./") {
                *src = format!("@./{norm}");
            }
        }
    });
}

/// Normalize blocks for sending to the cloud: cloud image refs and no local fold state.
pub fn for_cloud(blocks: &mut [Block], media: &MediaMap) {
    cloudify_media(blocks, media);
    walk_mut(blocks, &mut |b| b.folded = false);
}

/// Full cloud XML payload for creating a brand-new Feishu document.
pub fn cloud_xml(doc: &Document, media: &MediaMap) -> String {
    let mut sent = doc.blocks.clone();
    for_cloud(&mut sent, media);
    format!("<title>{}</title>{}", docxml::serialize_inline(&doc.title), docxml::serialize_blocks_compact(&sent))
}

/// Local image paths referenced by `doc` that have no cloud token yet.
pub fn new_local_images(doc: &Document, media: &MediaMap) -> Vec<String> {
    let mut out = Vec::new();
    for raw in doc.asset_sources() {
        let s = raw.replace('\\', "/");
        if s.contains('/') && !s.contains(':') && !media.values().any(|p| p == &s) && !out.contains(&s) {
            out.push(s);
        }
    }
    out
}

/// One comparison key per top-level block (folding stripped, compact XML).
pub fn keys(blocks: &[Block]) -> Vec<String> {
    blocks.iter().map(block_key).collect()
}

pub fn block_key(b: &Block) -> String {
    let mut one = [b.clone()];
    walk_mut(&mut one, &mut |x| x.folded = false);
    docxml::serialize_blocks_compact(&one)
}

/// Return a clone of `doc` with every block's `folded` flag cleared (for merge/diff).
pub fn strip_fold(doc: &Document) -> Document {
    let mut out = doc.clone();
    walk_mut(&mut out.blocks, &mut |b| b.folded = false);
    out
}

/// Carry `folded = true` states from `from` onto matching blocks in `to` (by normalized key).
pub fn preserve_folds(from: &[Block], to: &mut [Block]) {
    let mut folded_keys: Vec<String> = Vec::new();
    fn collect(blocks: &[Block], out: &mut Vec<String>) {
        for b in blocks {
            if b.folded {
                out.push(block_key(b));
            }
            collect(&b.children, out);
        }
    }
    collect(from, &mut folded_keys);
    if folded_keys.is_empty() {
        return;
    }
    walk_mut(to, &mut |b| {
        if folded_keys.contains(&block_key(b)) {
            b.folded = true;
        }
    });
}

/// Whether a local document has nothing typed in it yet.
pub fn is_blank(doc: &Document) -> bool {
    doc.title.text.trim().is_empty()
        && doc.blocks.iter().all(|b| b.children.is_empty() && b.text.text.trim().is_empty() && matches!(b.kind, BlockKind::Text))
}
