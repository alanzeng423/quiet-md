//! Parses Feishu / Lark cloud document and wiki URLs (or bare tokens).

use anyhow::{Result, bail};

/// What a pasted Feishu / Lark link points at (before network resolution).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PastedLink {
    /// A docx token (`/docx/…`, or a bare token).
    Docx { token: String, url: String },
    /// A wiki node token (`/wiki/…`), resolved to its underlying docx via API.
    Wiki { node_token: String, url: String },
}

impl PastedLink {
    pub fn url(&self) -> &str {
        match self {
            PastedLink::Docx { url, .. } | PastedLink::Wiki { url, .. } => url,
        }
    }
}

fn token_ok(t: &str) -> bool {
    t.len() >= 20 && t.chars().all(|c| c.is_ascii_alphanumeric())
}

fn tail_after(url: &str, key: &str) -> Option<String> {
    url.split(key)
        .nth(1)
        .map(|t| t.split(['?', '#', '/']).next().unwrap_or("").to_string())
}

/// Extract a Feishu / Lark document or wiki target from a pasted URL or bare token.
pub fn parse_link(input: &str) -> Result<PastedLink> {
    let raw = input.trim();
    if raw.is_empty() {
        bail!("请输入飞书文档或知识库链接");
    }
    let clean_url = if raw.starts_with("http://") || raw.starts_with("https://") {
        raw.to_string()
    } else {
        String::new()
    };
    if raw.contains("/wiki/") {
        return match tail_after(raw, "/wiki/") {
            Some(t) if token_ok(&t) => Ok(PastedLink::Wiki { node_token: t, url: clean_url }),
            _ => bail!("没有识别出飞书知识库链接"),
        };
    }
    if let Some(t) = tail_after(raw, "/docx/") {
        if !token_ok(&t) {
            bail!("没有识别出飞书文档链接");
        }
        return Ok(PastedLink::Docx { token: t, url: clean_url });
    }
    if raw.contains("/docs/") || raw.contains("/doc/") {
        bail!("只支持新版飞书文档（链接里带 /docx/ 或 /wiki/）");
    }
    if raw.contains("/sheets/") || raw.contains("/base/") || raw.contains("/mindnotes/") || raw.contains("/slides/") || raw.contains("/file/") {
        bail!("只支持飞书新版云文档（/docx/）或知识库文档（/wiki/）");
    }
    if !clean_url.is_empty() || raw.contains('/') {
        bail!("没有识别出飞书文档链接");
    }
    if !token_ok(raw) {
        bail!("没有识别出飞书文档链接");
    }
    Ok(PastedLink::Docx { token: raw.to_string(), url: String::new() })
}

/// Extract a docx token directly (rejecting `/wiki/` links).
pub fn docx_token_of(input: &str) -> Result<String> {
    match parse_link(input)? {
        PastedLink::Docx { token, .. } => Ok(token),
        PastedLink::Wiki { .. } => bail!("这里需要云空间里的文档链接"),
    }
}

/// Whether `text` looks like a Feishu / Lark doc or wiki URL (for quick paste hints).
pub fn looks_like_feishu_url(text: &str) -> bool {
    let s = text.trim();
    (s.starts_with("https://") || s.starts_with("http://"))
        && (s.contains("/docx/") || s.contains("/wiki/"))
        && (s.contains("feishu.cn") || s.contains("larkoffice.com") || s.contains("larksuite.com") || parse_link(s).is_ok())
}

/// Human-readable Chinese label for a non-docx wiki object type.
pub fn wiki_kind(t: &str) -> &'static str {
    match t {
        "sheet" => "表格",
        "bitable" => "多维表格",
        "mindnote" => "思维笔记",
        "doc" => "旧版文档",
        "file" => "文件",
        "slides" => "幻灯片",
        _ => "其他类型",
    }
}
