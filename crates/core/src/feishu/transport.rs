//! Transport abstraction for Feishu / Lark cloud document operations.
//!
//! Keeping network / CLI calls behind [`FeishuTransport`] lets the sync engine
//! be tested deterministically with an in-memory fake, and allows swapping in a
//! direct OpenAPI (`CurlHttp` + OAuth Device Flow) backend without changing
//! merge, planning or UI code.

use super::link::wiki_kind;
use super::planner::PushOp;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

/// Which Feishu / Lark identity to pass to `lark-cli`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Identity {
    /// Let `lark-cli` choose its configured default (`user` if logged in, else `bot`).
    #[default]
    Auto,
    /// Force `--as user` (recommended for personal cloud docs and wikis).
    User,
    /// Force `--as bot` (enterprise self-built app bot identity).
    Bot,
}

impl Identity {
    pub fn from_pref(s: &str) -> Self {
        match s {
            "user" => Identity::User,
            "bot" => Identity::Bot,
            _ => Identity::Auto,
        }
    }

    pub fn as_pref(self) -> &'static str {
        match self {
            Identity::Auto => "auto",
            Identity::User => "user",
            Identity::Bot => "bot",
        }
    }
}

/// Resolved target behind a `/wiki/` node token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WikiTarget {
    pub obj_token: String,
    pub title: String,
}

/// Result of a lightweight revision probe (no full body transferred).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocProbe {
    pub revision: u64,
    pub title: Option<String>,
}

/// Raw cloud fetch response before DocxXML cleaning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawCloudDoc {
    pub document_id: String,
    pub revision: u64,
    pub xml: String,
}

/// Result of creating a new cloud document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CreatedDoc {
    pub token: String,
    pub url: String,
}

/// Status of the local Feishu / Lark connection.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthInfo {
    pub installed: bool,
    pub brand: String,
    pub user_ready: bool,
    pub bot_ready: bool,
    pub user_name: String,
    pub app_id: String,
    pub detail: String,
}

impl AuthInfo {
    pub fn ready_for(&self, id: Identity) -> bool {
        if !self.installed {
            return false;
        }
        match id {
            Identity::User => self.user_ready,
            Identity::Bot => self.bot_ready,
            Identity::Auto => self.user_ready || self.bot_ready,
        }
    }
}

/// Typed failure reasons so the sync engine and UI can react properly
/// (retry on `RevisionConflict`, back off on `RateLimited`, prompt login on
/// `NotAuthenticated`, show offline state on `Offline`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    NotInstalled(String),
    NotAuthenticated(String),
    RevisionConflict(String),
    RateLimited(String),
    PermissionDenied(String),
    NotFound(String),
    Offline(String),
    Other(String),
}

impl fmt::Display for TransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TransportError::NotInstalled(m) => write!(f, "{m}"),
            TransportError::NotAuthenticated(m) => write!(f, "飞书未登录或授权已过期：{m}"),
            TransportError::RevisionConflict(m) => write!(f, "云端文档已更新，需要重新合并：{m}"),
            TransportError::RateLimited(m) => write!(f, "飞书接口触发限流，稍后自动重试：{m}"),
            TransportError::PermissionDenied(m) => write!(f, "没有访问或编辑此飞书文档的权限：{m}"),
            TransportError::NotFound(m) => write!(f, "找不到这篇飞书文档：{m}"),
            TransportError::Offline(m) => write!(f, "网络连接失败：{m}"),
            TransportError::Other(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for TransportError {}

pub type TransportResult<T> = std::result::Result<T, TransportError>;

/// Backend operations needed by the Feishu bidirectional sync engine.
pub trait FeishuTransport: Send + Sync {
    fn resolve_wiki(&self, node_token: &str, work_dir: &Path) -> TransportResult<WikiTarget>;
    fn probe_revision(&self, doc_token: &str, work_dir: &Path) -> TransportResult<DocProbe>;
    fn fetch(&self, doc_token: &str, work_dir: &Path) -> TransportResult<RawCloudDoc>;
    fn download_media(&self, media_token: &str, output_rel: &str, work_dir: &Path) -> TransportResult<PathBuf>;
    /// Apply incremental block operations in order. When `expected_revision` is
    /// `Some(r)` with `r > 0`, the first op checks against `r` and returns
    /// [`TransportError::RevisionConflict`] if the cloud moved ahead.
    /// Returns the latest cloud `revision_id` reported by the server, if any.
    fn apply_ops(&self, doc_token: &str, expected_revision: Option<u64>, ops: &[PushOp], work_dir: &Path) -> TransportResult<Option<u64>>;
    fn create_doc(&self, title: Option<&str>, xml: &str, work_dir: &Path) -> TransportResult<CreatedDoc>;
    fn check_auth(&self, work_dir: &Path) -> AuthInfo;
}

// ---------------------------------------------------------------- lark-cli backend

#[cfg(not(target_family = "wasm"))]
#[derive(Clone, Debug)]
pub struct LarkCliTransport {
    pub identity: Identity,
    pub binary: Option<String>,
}

#[cfg(not(target_family = "wasm"))]
impl Default for LarkCliTransport {
    fn default() -> Self {
        Self { identity: Identity::Auto, binary: None }
    }
}

#[cfg(not(target_family = "wasm"))]
impl LarkCliTransport {
    pub fn new(identity: Identity) -> Self {
        Self { identity, binary: None }
    }

    fn bin_and_path(&self) -> (PathBuf, String) {
        let path = Self::extended_path();
        if let Some(explicit) = self.binary.clone()
            .or_else(|| std::env::var("QUIET_MD_LARK_CLI").ok().filter(|s| !s.trim().is_empty()))
        {
            return (PathBuf::from(explicit), path);
        }
        #[cfg(target_os = "windows")]
        {
            let mut dirs: Vec<PathBuf> = std::env::split_paths(&path).collect();
            if let Ok(local) = std::env::var("LOCALAPPDATA") {
                dirs.push(PathBuf::from(local).join("Yarn").join("Data").join("global"));
            }
            if let Some(found) = resolve_windows_bin_in_dirs(&dirs) {
                return (found, path);
            }
        }
        (PathBuf::from("lark-cli"), path)
    }

    fn extended_path() -> String {
        let mut path = std::env::var("PATH").unwrap_or_default();
        let sep = if cfg!(windows) { ';' } else { ':' };
        let mut extras: Vec<String> = if cfg!(windows) {
            let mut v = Vec::new();
            if let Ok(appdata) = std::env::var("APPDATA") {
                v.push(PathBuf::from(appdata).join("npm").to_string_lossy().into_owned());
            }
            if let Ok(local) = std::env::var("LOCALAPPDATA") {
                let lb = PathBuf::from(local);
                v.push(lb.join("pnpm").to_string_lossy().into_owned());
                v.push(lb.join("Yarn").join("bin").to_string_lossy().into_owned());
            }
            if let Ok(pf) = std::env::var("ProgramFiles") {
                v.push(PathBuf::from(pf).join("nodejs").to_string_lossy().into_owned());
            }
            if let Some(home) = dirs::home_dir() {
                v.push(home.join("scoop").join("shims").to_string_lossy().into_owned());
                v.push(home.join(".volta").join("bin").to_string_lossy().into_owned());
            }
            v
        } else {
            vec!["/opt/homebrew/bin".into(), "/usr/local/bin".into()]
        };
        if let Some(home) = dirs::home_dir() {
            extras.push(home.join(".local").join("bin").to_string_lossy().into_owned());
            extras.push(home.join(".cargo").join("bin").to_string_lossy().into_owned());
        }
        for extra in extras {
            let exists = if cfg!(windows) {
                path.split(sep).any(|p| p.eq_ignore_ascii_case(&extra))
            } else {
                path.split(sep).any(|p| p == extra)
            };
            if !exists {
                if !path.is_empty() {
                    path.push(sep);
                }
                path.push_str(&extra);
            }
        }
        path
    }

    fn run(&self, work_dir: &Path, args: &[&str], stdin_data: Option<&[u8]>, with_identity: bool) -> TransportResult<serde_json::Value> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let (bin, ext_path) = self.bin_and_path();
        let mut cmd = Command::new(&bin);
        #[cfg(target_os = "windows")]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        cmd.args(args);
        if with_identity {
            match self.identity {
                Identity::User => { cmd.args(["--as", "user"]); }
                Identity::Bot => { cmd.args(["--as", "bot"]); }
                Identity::Auto => {}
            }
        }
        if work_dir.exists() {
            cmd.current_dir(work_dir);
        }
        cmd.env("PATH", ext_path);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        if stdin_data.is_some() {
            cmd.stdin(Stdio::piped());
        } else {
            cmd.stdin(Stdio::null());
        }

        let mut child = cmd.spawn().map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                TransportError::NotInstalled("未找到 lark-cli，请先安装并登录：npm i -g @larksuite/cli && lark-cli auth login".into())
            } else {
                TransportError::Other(format!("无法运行 lark-cli（{e}）"))
            }
        })?;

        if let Some(bytes) = stdin_data
            && let Some(mut stdin) = child.stdin.take()
        {
            let _ = stdin.write_all(bytes);
        }

        let out = child.wait_with_output().map_err(|e| TransportError::Other(format!("执行 lark-cli 失败：{e}")))?;
        let reply: Option<serde_json::Value> = serde_json::from_slice(&out.stdout).ok();
        match reply {
            Some(v) if out.status.success() && v["ok"].as_bool() != Some(false) => Ok(v),
            _ => Err(classify_cli_error(args, &out.stdout, &out.stderr, out.status.code())),
        }
    }
}

/// Resolve `lark-cli` on Windows across `dirs`, preferring the native Go
/// `lark-cli.exe` (direct or inside `@larksuite/cli/bin`) over npm's `.cmd`
/// batch shim to avoid `cmd.exe` + Node startup overhead and argument-quoting
/// pitfalls. Falls back to `lark-cli.cmd` if the native binary is absent (e.g.
/// mid self-update recovery).
#[cfg(all(not(target_family = "wasm"), any(target_os = "windows", test)))]
pub(crate) fn resolve_windows_bin_in_dirs(dirs: &[PathBuf]) -> Option<PathBuf> {
    let mut cmd_fallback: Option<PathBuf> = None;
    for dir in dirs {
        if dir.as_os_str().is_empty() {
            continue;
        }
        let exe = dir.join("lark-cli.exe");
        if exe.is_file() {
            return Some(exe);
        }
        let npm_native = dir.join("node_modules").join("@larksuite").join("cli").join("bin").join("lark-cli.exe");
        if npm_native.is_file() {
            return Some(npm_native);
        }
        let cmd = dir.join("lark-cli.cmd");
        if cmd.is_file() {
            if let Ok(bytes) = std::fs::read(&cmd)
                && let Some(native) = extract_native_exe_from_cmd(dir, &String::from_utf8_lossy(&bytes))
            {
                return Some(native);
            }
            if cmd_fallback.is_none() {
                cmd_fallback = Some(cmd);
            }
        }
    }
    cmd_fallback
}

#[cfg(all(not(target_family = "wasm"), any(target_os = "windows", test)))]
fn extract_native_exe_from_cmd(shim_dir: &Path, content: &str) -> Option<PathBuf> {
    for tok in cmd_tokens(content) {
        let lower = tok.to_ascii_lowercase();
        let suffix_len = if lower.ends_with("scripts\\run.js") || lower.ends_with("scripts/run.js") {
            "scripts/run.js".len()
        } else {
            continue;
        };
        let pkg_raw = tok[..tok.len() - suffix_len].trim_end_matches(['\\', '/']);
        let pkg_lower = pkg_raw.to_ascii_lowercase();
        let pkg_dir = if let Some(rest) = pkg_lower.strip_prefix("%dp0%").map(|_| &pkg_raw[5..])
            .or_else(|| pkg_lower.strip_prefix("%~dp0").map(|_| &pkg_raw[5..]))
        {
            join_portable(shim_dir, rest)
        } else if pkg_raw.is_empty() {
            continue;
        } else {
            PathBuf::from(pkg_raw)
        };
        let candidate = pkg_dir.join("bin").join("lark-cli.exe");
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

#[cfg(all(not(target_family = "wasm"), any(target_os = "windows", test)))]
fn join_portable(base: &Path, rest: &str) -> PathBuf {
    let mut p = base.to_path_buf();
    for seg in rest.split(['\\', '/']) {
        match seg {
            "" | "." => {}
            ".." => { p.pop(); }
            s => p.push(s),
        }
    }
    p
}

#[cfg(all(not(target_family = "wasm"), any(target_os = "windows", test)))]
fn cmd_tokens(content: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = content.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() { i += 1; }
        if i >= bytes.len() { break; }
        if bytes[i] == b'"' {
            i += 1;
            let start = i;
            while i < bytes.len() && bytes[i] != b'"' { i += 1; }
            out.push(&content[start..i]);
            if i < bytes.len() { i += 1; }
        } else {
            let start = i;
            while i < bytes.len() && !bytes[i].is_ascii_whitespace() && bytes[i] != b'"' { i += 1; }
            out.push(&content[start..i]);
        }
    }
    out
}

#[cfg(not(target_family = "wasm"))]
fn classify_cli_error(args: &[&str], stdout: &[u8], stderr: &[u8], exit: Option<i32>) -> TransportError {
    let parsed = [stdout, stderr].into_iter().find_map(|b| serde_json::from_slice::<serde_json::Value>(b).ok());
    let code = parsed.as_ref().and_then(|v| {
        ["/error/code", "/code"].iter().find_map(|p| v.pointer(p).and_then(|x| x.as_i64()))
    });
    let err_type = parsed.as_ref().and_then(|v| v.pointer("/error/type").and_then(|x| x.as_str())).unwrap_or("");
    let subtype = parsed.as_ref().and_then(|v| v.pointer("/error/subtype").and_then(|x| x.as_str())).unwrap_or("");
    let raw_msg = parsed.as_ref()
        .and_then(|v| ["/error/message", "/error/msg", "/msg", "/message"].iter().find_map(|p| v.pointer(p)?.as_str().map(str::to_string)))
        .or_else(|| String::from_utf8_lossy(stderr).lines().rev().find(|l| !l.trim().is_empty()).map(str::to_string))
        .unwrap_or_else(|| format!("退出码 {:?}", exit));

    let lower = raw_msg.to_ascii_lowercase();
    let label = format!("lark-cli {}：{raw_msg}", args.iter().take(2).copied().collect::<Vec<_>>().join(" "));

    if matches!(code, Some(99991400 | 429)) || lower.contains("rate limit") || lower.contains("too many requests") || lower.contains("frequency limit") {
        return TransportError::RateLimited(raw_msg);
    }
    if matches!(code, Some(1770064 | 1061045)) || lower.contains("revision") && (lower.contains("conflict") || lower.contains("mismatch") || lower.contains("stale") || lower.contains("outdated")) {
        return TransportError::RevisionConflict(raw_msg);
    }
    if err_type == "authentication"
        || matches!(subtype, "token_missing" | "token_expired" | "not_logged_in" | "missing_identity")
        || lower.contains("auth login")
        || lower.contains("no token")
        || lower.contains("not logged in")
        || lower.contains("token expired")
        || lower.contains("invalid access token")
    {
        return TransportError::NotAuthenticated(raw_msg);
    }
    if err_type == "authorization" || matches!(code, Some(1770032 | 99991672 | 403)) || lower.contains("permission") || lower.contains("forbidden") || lower.contains("scope") || lower.contains("no access") {
        return TransportError::PermissionDenied(raw_msg);
    }
    if matches!(code, Some(1770002 | 404)) || lower.contains("not found") || lower.contains("does not exist") {
        return TransportError::NotFound(raw_msg);
    }
    if err_type == "network" || lower.contains("timeout") || lower.contains("no such host") || lower.contains("connection refused") || lower.contains("network is unreachable") || lower.contains("dial tcp") || lower.contains("eof") {
        return TransportError::Offline(raw_msg);
    }
    TransportError::Other(label)
}

#[cfg(not(target_family = "wasm"))]
fn find_json_str(v: &serde_json::Value, key: &str) -> Option<String> {
    match v {
        serde_json::Value::Object(m) => m.get(key)
            .and_then(|x| x.as_str().map(str::to_string))
            .or_else(|| m.values().find_map(|x| find_json_str(x, key))),
        serde_json::Value::Array(a) => a.iter().find_map(|x| find_json_str(x, key)),
        _ => None,
    }
}

#[cfg(not(target_family = "wasm"))]
fn find_json_u64(v: &serde_json::Value, key: &str) -> Option<u64> {
    match v {
        serde_json::Value::Object(m) => m.get(key)
            .and_then(|x| x.as_u64().or_else(|| x.as_str()?.parse().ok()))
            .or_else(|| m.values().find_map(|x| find_json_u64(x, key))),
        serde_json::Value::Array(a) => a.iter().find_map(|x| find_json_u64(x, key)),
        _ => None,
    }
}

#[cfg(not(target_family = "wasm"))]
impl FeishuTransport for LarkCliTransport {
    fn resolve_wiki(&self, node_token: &str, work_dir: &Path) -> TransportResult<WikiTarget> {
        let get = |n: &str| -> TransportResult<serde_json::Value> {
            let v = self.run(work_dir, &["wiki", "+node-get", "--node-token", n], None, true)?;
            Ok(v["data"].clone())
        };
        let mut d = get(node_token)?;
        // Follow nested node object if wrapped under `data.node`.
        if d.get("node").is_some_and(|n| n.is_object()) {
            d = d["node"].clone();
        }
        if d["node_type"].as_str() == Some("shortcut") {
            let origin = d["origin_node_token"].as_str().filter(|s| !s.is_empty())
                .ok_or_else(|| TransportError::Other("知识库快捷方式没有指向原文档".into()))?;
            d = get(origin)?;
            if d.get("node").is_some_and(|n| n.is_object()) {
                d = d["node"].clone();
            }
        }
        let kind = d["obj_type"].as_str().unwrap_or_default();
        if kind != "docx" {
            return Err(TransportError::Other(format!("知识库里的这篇文档是{}（{kind}），目前只支持新版文档（docx）", wiki_kind(kind))));
        }
        let obj_token = d["obj_token"].as_str().filter(|s| !s.is_empty())
            .ok_or_else(|| TransportError::Other("知识库没有返回文档的标识".into()))?
            .to_string();
        let title = d["title"].as_str().unwrap_or_default().to_string();
        Ok(WikiTarget { obj_token, title })
    }

    fn probe_revision(&self, doc_token: &str, work_dir: &Path) -> TransportResult<DocProbe> {
        let path = format!("/open-apis/docx/v1/documents/{doc_token}");
        match self.run(work_dir, &["api", "GET", &path], None, true) {
            Ok(v) => {
                let doc = &v["data"]["document"];
                let revision = doc["revision_id"].as_u64()
                    .or_else(|| find_json_u64(&v["data"], "revision_id"))
                    .unwrap_or(0);
                let title = doc["title"].as_str().map(str::to_string);
                Ok(DocProbe { revision, title })
            }
            // If raw api call is blocked or lacks a different scope, fall back to a lightweight outline fetch.
            Err(TransportError::PermissionDenied(_)) | Err(TransportError::Other(_)) => {
                let v = self.run(work_dir, &["docs", "+fetch", "--doc", doc_token, "--doc-format", "xml", "--scope", "outline", "--max-depth", "1"], None, true)?;
                let revision = find_json_u64(&v["data"], "revision_id").unwrap_or(0);
                Ok(DocProbe { revision, title: None })
            }
            Err(e) => Err(e),
        }
    }

    fn fetch(&self, doc_token: &str, work_dir: &Path) -> TransportResult<RawCloudDoc> {
        let v = self.run(work_dir, &["docs", "+fetch", "--doc", doc_token, "--detail", "with-ids", "--doc-format", "xml"], None, true)?;
        let d = &v["data"]["document"];
        let xml = d["content"].as_str()
            .ok_or_else(|| TransportError::Other("飞书返回的内容里没有文档正文".into()))?
            .to_string();
        let revision = d["revision_id"].as_u64().or_else(|| find_json_u64(&v["data"], "revision_id")).unwrap_or(0);
        let document_id = d["document_id"].as_str().unwrap_or(doc_token).to_string();
        Ok(RawCloudDoc { document_id, revision, xml })
    }

    fn download_media(&self, media_token: &str, output_rel: &str, work_dir: &Path) -> TransportResult<PathBuf> {
        let v = self.run(work_dir, &["docs", "+media-download", "--token", media_token, "--output", output_rel, "--overwrite"], None, true)?;
        let saved = v["data"]["saved_path"].as_str().map(|s| {
            let p = PathBuf::from(s);
            if p.is_absolute() { p } else { work_dir.join(p) }
        }).unwrap_or_else(|| work_dir.join(output_rel));
        Ok(saved)
    }

    fn apply_ops(&self, doc_token: &str, expected_revision: Option<u64>, ops: &[PushOp], work_dir: &Path) -> TransportResult<Option<u64>> {
        let mut rev = expected_revision.filter(|&r| r > 0);
        let mut latest_rev = None;
        for op in ops {
            let rev_str = rev.map(|r| r.to_string());
            let mut args: Vec<&str> = vec!["docs", "+update", "--doc", doc_token, "--doc-format", "xml", "--command"];
            let (stdin_payload, joined_ids): (Option<&[u8]>, String) = match op {
                PushOp::Title { id, xml } => {
                    args.extend(["block_replace", "--block-id", id.as_str(), "--content", "-"]);
                    (Some(xml.as_bytes()), String::new())
                }
                PushOp::Replace { block_id, xml } => {
                    args.extend(["block_replace", "--block-id", block_id.as_str(), "--content", "-"]);
                    (Some(xml.as_bytes()), String::new())
                }
                PushOp::InsertAfter { anchor, xml } => {
                    args.extend(["block_insert_after", "--block-id", anchor.as_str(), "--content", "-"]);
                    (Some(xml.as_bytes()), String::new())
                }
                PushOp::Delete { block_ids } => {
                    let joined = block_ids.join(",");
                    args.push("block_delete");
                    (None, joined)
                }
            };
            if !joined_ids.is_empty() {
                args.extend(["--block-id", joined_ids.as_str()]);
            }
            if let Some(r) = rev_str.as_deref() {
                args.extend(["--revision-id", r]);
            }
            let reply = self.run(work_dir, &args, stdin_payload, true)?;
            // Chain the server's new revision_id when present; otherwise let subsequent ops in this batch apply against latest.
            let next_rev = find_json_u64(&reply["data"], "revision_id");
            if let Some(nr) = next_rev {
                latest_rev = Some(nr);
            }
            rev = next_rev;
        }
        Ok(latest_rev)
    }

    fn create_doc(&self, title: Option<&str>, xml: &str, work_dir: &Path) -> TransportResult<CreatedDoc> {
        let clean_title = title.map(|t| t.replace(['\r', '\n'], " ")).map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let mut args: Vec<&str> = vec!["docs", "+create", "--doc-format", "xml", "--content", "-"];
        if let Some(t) = clean_title.as_deref() {
            args.extend(["--title", t]);
        }
        let v = self.run(work_dir, &args, Some(xml.as_bytes()), true)?;
        let url = find_json_str(&v["data"], "url").unwrap_or_default();
        let token = find_json_str(&v["data"], "document_id")
            .or_else(|| super::link::docx_token_of(&url).ok())
            .ok_or_else(|| TransportError::Other("飞书没有返回新文档的链接".into()))?;
        Ok(CreatedDoc { token, url })
    }

    fn check_auth(&self, work_dir: &Path) -> AuthInfo {
        match self.run(work_dir, &["auth", "status"], None, false) {
            Ok(v) => AuthInfo {
                installed: true,
                brand: v["brand"].as_str().unwrap_or("feishu").to_string(),
                user_ready: v["identities"]["user"]["available"].as_bool().unwrap_or(false),
                bot_ready: v["identities"]["bot"]["available"].as_bool().unwrap_or(false),
                user_name: v["identities"]["user"]["userName"].as_str().unwrap_or_default().to_string(),
                app_id: v["appId"].as_str().unwrap_or_default().to_string(),
                detail: v["note"].as_str().unwrap_or_default().to_string(),
            },
            Err(TransportError::NotInstalled(msg)) => AuthInfo { installed: false, detail: msg, ..Default::default() },
            Err(e) => AuthInfo { installed: true, detail: e.to_string(), ..Default::default() },
        }
    }
}
