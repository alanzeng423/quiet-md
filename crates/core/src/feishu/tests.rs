use super::*;
use crate::docxml;
use crate::model::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const CLOUD_FIXTURE: &str = include_str!("../../tests/fixtures/feishu-cloud.xml");

fn doc(blocks: &[&str]) -> Document {
    docxml::parse(&format!("<title>t</title>{}", blocks.concat())).unwrap()
}

fn texts(d: &Document) -> Vec<String> {
    d.blocks.iter().map(|b| b.text.text.clone()).collect()
}

fn cloud_doc(blocks: &[&str]) -> CloudDoc {
    let d = doc(blocks);
    CloudDoc {
        revision: 1,
        title_id: "doc".into(),
        ids: (0..d.blocks.len()).map(|i| Some(format!("b{i}"))).collect(),
        doc: d,
    }
}

/// Apply [`PushOp`]s to an in-memory block list the same way Feishu does.
fn apply_ops_local(remote: &CloudDoc, ops: &[PushOp]) -> Vec<String> {
    let mut v: Vec<(String, String)> = remote.ids.iter().flatten().cloned().zip(keys(&remote.doc.blocks)).collect();
    let mut fresh = 0usize;
    let mut parse_new = |xml: &str| -> Vec<(String, String)> {
        let d = docxml::parse(&format!("<title></title>{xml}")).unwrap();
        keys(&d.blocks).into_iter().map(|k| { fresh += 1; (format!("new{fresh}"), k) }).collect()
    };
    let pos = |v: &[(String, String)], id: &str| v.iter().position(|(i, _)| i == id).unwrap_or_else(|| panic!("no block {id} in {v:?}"));
    for op in ops {
        match op {
            PushOp::Title { .. } => {}
            PushOp::Replace { block_id, xml } => {
                let at = pos(&v, block_id);
                v.splice(at..=at, parse_new(xml));
            }
            PushOp::Delete { block_ids } => {
                for id in block_ids {
                    let at = pos(&v, id);
                    v.remove(at);
                }
            }
            PushOp::InsertAfter { anchor, xml } => {
                let at = if anchor == "0" { 0 } else { pos(&v, anchor) + 1 };
                v.splice(at..at, parse_new(xml));
            }
        }
    }
    v.into_iter().map(|(_, k)| k).collect()
}

// ---------------------------------------------------------------- link tests

#[test]
fn parses_feishu_and_lark_links() {
    let d = parse_link("https://example.feishu.cn/docx/DoxcnExampleDocumentToken1?from=from_copylink").unwrap();
    assert_eq!(d, PastedLink::Docx { token: "DoxcnExampleDocumentToken1".into(), url: "https://example.feishu.cn/docx/DoxcnExampleDocumentToken1?from=from_copylink".into() });

    let lark = parse_link("https://my.larkoffice.com/wiki/WikcnExampleWikiNodeToken1#heading").unwrap();
    assert_eq!(lark, PastedLink::Wiki { node_token: "WikcnExampleWikiNodeToken1".into(), url: "https://my.larkoffice.com/wiki/WikcnExampleWikiNodeToken1#heading".into() });

    let bare = parse_link("  DoxcnExampleDocumentToken1  ").unwrap();
    assert_eq!(bare, PastedLink::Docx { token: "DoxcnExampleDocumentToken1".into(), url: String::new() });

    assert!(parse_link("https://x.feishu.cn/sheets/ShtcnExampleDocumentToken1").is_err());
    assert!(parse_link("https://x.feishu.cn/docs/doccnExampleLegacyToken01").is_err());
    assert!(parse_link("https://x.feishu.cn/wiki/short").is_err());
    assert!(parse_link("not-a-link").is_err());
    assert!(docx_token_of("https://x.feishu.cn/wiki/WikcnExampleWikiNodeToken1").is_err());
}

// ---------------------------------------------------------------- codec tests

#[test]
fn cloud_fixture_parses_aligns_and_normalizes() {
    let media = MediaMap::from([("imgTOKEN1".into(), ".assets/blue.png".into())]);
    let c = CloudDoc::parse(CLOUD_FIXTURE, 4, &media).unwrap();
    assert_eq!(c.title_id, "docTOKEN");
    assert_eq!(c.ids.len(), c.doc.blocks.len());
    assert!(c.ids.iter().all(Option::is_some));
    assert_eq!(c.doc.blocks.iter().filter(|b| matches!(b.kind, BlockKind::Bullet)).count(), 3);
    let xml = docxml::serialize(&c.doc);
    assert!(!xml.contains("id=\""), "{xml}");
    assert!(!xml.contains("rgb"), "colors map back to names: {xml}");
    assert!(xml.contains("text-color=\"red\"") && xml.contains("background-color=\"light-yellow\""), "{xml}");
    assert!(xml.contains("<callout emoji=\"💡\" background-color=\"light-orange\" border-color=\"orange\">"), "{xml}");
    assert!(xml.contains("<th background-color=\"light-gray\""), "{xml}");
    assert!(xml.contains("caption=\"例\"") && xml.contains("caption=\"蓝图\""), "{xml}");
    assert!(xml.contains("<img path=\".assets/blue.png\""), "{xml}");
    assert!(!xml.contains("seq-marker"), "{xml}");
    assert_eq!(image_tokens(CLOUD_FIXTURE), vec!["imgTOKEN1".to_string()]);
}

#[test]
fn compact_round_trips_all_blocks_fixture() {
    let d = docxml::parse(include_str!("../../tests/fixtures/all-blocks.xml")).unwrap();
    let s = docxml::serialize_blocks_compact(&d.blocks);
    assert!(!s.contains('\n'));
    assert!(!s.contains(">  <"), "{s}");
    let back = docxml::parse(&format!("<title></title>{s}")).unwrap();
    assert_eq!(keys(&back.blocks), keys(&d.blocks));
}

// ---------------------------------------------------------------- merge & plan tests

#[test]
fn merge_preserves_disjoint_edits_folding_and_local_cover() {
    let base = doc(&["<h1>H</h1>", "<p>a</p>", "<p>b</p>", "<p>c</p>"]);
    let mut local = doc(&["<h1 folded=\"true\">H</h1>", "<p>a 本地</p>", "<p>b</p>", "<p>c</p>"]);
    local.icon = Some("📝".into());
    let remote = doc(&["<h1>H</h1>", "<p>a</p>", "<p>b</p>", "<p>c 云端</p>"]);

    let m = merge_docs(&base, &local, &remote);
    assert!(!m.has_conflicts());
    assert!(m.local_changed && m.remote_changed);
    assert_eq!(texts(&m.doc), ["H", "a 本地", "b", "c 云端"]);
    assert!(m.doc.blocks[0].folded, "local fold state survives merge");
    assert_eq!(m.doc.icon.as_deref(), Some("📝"));
}

#[test]
fn merge_detects_same_block_conflict_and_keeps_local() {
    let base = doc(&["<p>a</p>", "<p>b</p>", "<p>c</p>"]);
    let local = doc(&["<p>a</p>", "<p>本地</p>", "<p>c</p>"]);
    let remote = doc(&["<p>a</p>", "<p>云端</p>", "<p>c</p>"]);
    let m = merge_docs(&base, &local, &remote);
    assert!(m.has_conflicts());
    assert_eq!(texts(&m.doc), ["a", "本地", "c"]);
}

#[test]
fn folding_only_is_not_a_local_change() {
    let base = doc(&["<h1>h</h1>", "<p>a</p>"]);
    let local = doc(&["<h1 folded=\"true\">h</h1>", "<p>a</p>"]);
    let remote = doc(&["<h1>h</h1>", "<p>a</p>", "<p>b</p>"]);
    let m = merge_docs(&base, &local, &remote);
    assert!(!m.local_changed && m.remote_changed);
    assert!(m.doc.blocks[0].folded);
    assert_eq!(texts(&m.doc), ["h", "a", "b"]);
}

#[test]
fn plan_turns_remote_into_target_without_touching_unchanged_blocks() {
    let remote = cloud_doc(&["<p>a</p>", "<ul><li>x</li><li>y</li></ul>", "<p>c</p>", "<p>d</p>"]);
    let target = doc(&["<p>0</p>", "<p>a</p>", "<ul><li>x</li><li>Y</li></ul>", "<p>d</p>", "<p>e</p>"]);
    let ops = plan_push(&remote, &target, &MediaMap::new()).unwrap();
    assert_eq!(apply_ops_local(&remote, &ops), keys(&target.blocks));
    assert!(ops.iter().all(|o| !matches!(o, PushOp::Replace { xml, .. } | PushOp::InsertAfter { xml, .. } if xml.contains("<p>a</p>") || xml.contains("<p>d</p>"))));
    assert!(plan_push(&remote, &remote.doc, &MediaMap::new()).unwrap().is_empty());
}

#[test]
fn plan_handles_multi_block_replacement_and_media() {
    let remote = cloud_doc(&["<p>keep</p>", "<p>r1</p>", "<p>r2</p>", "<p>r3</p>", "<p>end</p>"]);
    let mut target = doc(&["<p>keep</p>", "<img path=\".assets/old.png\"/>", "<img path=\".assets/new.png\"/>", "<p>end</p>"]);
    target.title = RichText::plain("新标题");
    let media = MediaMap::from([("tokOLD".into(), ".assets/old.png".into())]);
    let ops = plan_push(&remote, &target, &media).unwrap();
    assert_eq!(ops[0], PushOp::Title { id: "doc".into(), xml: "<title>新标题</title>".into() });
    // Verify delete of trailing r2, r3 followed by replace of r1.
    assert!(matches!(&ops[1], PushOp::Delete { block_ids } if block_ids == &["b2", "b3"]));
    assert!(matches!(&ops[2], PushOp::Replace { block_id, xml } if block_id == "b1" && xml.contains("<img src=\"tokOLD\"") && xml.contains("<img path=\"@./.assets/new.png\"")));
}

struct Rng(u64);
impl Rng {
    fn next(&mut self, n: usize) -> usize {
        self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % n.max(1)
    }
}

#[test]
fn randomized_plans_always_reach_target_exactly() {
    for seed in 0..300 {
        let mut r = Rng(seed);
        let n = r.next(14);
        let base: Vec<String> = (0..n).map(|i| if r.next(4) == 0 { format!("<ul><li>i{i}</li></ul>") } else { format!("<p>p{i}</p>") }).collect();
        let (mut local, mut remote) = (base.clone(), base.clone());
        for (side, tag) in [(&mut local, "L"), (&mut remote, "R")] {
            for _ in 0..1 + r.next(5) {
                let i = r.next(side.len() + 1);
                match r.next(3) {
                    0 => side.insert(i, format!("<p>{tag}{}</p>", r.next(1000))),
                    1 if i < side.len() => { side.remove(i); }
                    _ if i < side.len() => side[i] = format!("<p>{tag}{}</p>", r.next(1000)),
                    _ => {}
                }
            }
        }
        fn refs(v: &[String]) -> Vec<&str> { v.iter().map(String::as_str).collect() }
        let (ld, rm) = (doc(&refs(&local)), cloud_doc(&refs(&remote)));
        let ops = plan_push(&rm, &ld, &MediaMap::new()).unwrap();
        assert_eq!(apply_ops_local(&rm, &ops), keys(&ld.blocks), "seed {seed}: {ops:?}");
    }
}

#[test]
fn blockers_only_flag_changed_blocks() {
    let callout = "<callout emoji=\"💡\"><blockquote><p>q</p></blockquote></callout>";
    let remote = cloud_doc(&["<p>a</p>", callout, "<p>c</p>"]);
    let target = doc(&["<p>a</p>", callout, "<p>C!</p>"]);
    let (_, sent) = plan_push_sent(&remote, &target, &MediaMap::new()).unwrap();
    assert!(blockers_in(&target.blocks, &sent).is_empty());
    assert_eq!(blockers(&target.blocks).len(), 1);

    let with_countdown = doc(&["<p>a</p>", "<countdown duration=\"60\" start-time=\"0\" notify=\"false\" color=\"red\" timing=\"false\"/>"]);
    let (_, sent) = plan_push_sent(&remote, &with_countdown, &MediaMap::new()).unwrap();
    assert!(blockers_in(&with_countdown.blocks, &sent).iter().any(|s| s.contains("倒计时")));
}

// ---------------------------------------------------------------- fake transport e2e tests

#[derive(Default)]
struct FakeDoc {
    title: String,
    blocks: Vec<(String, String)>, // (id, compact xml)
    revision: u64,
    next_id: u64,
}

impl FakeDoc {
    fn new(title: &str, blocks: &[&str]) -> Self {
        let mut f = FakeDoc { title: title.into(), blocks: vec![], revision: 1, next_id: 1 };
        for b in blocks {
            let id = format!("blk{}", f.next_id);
            f.next_id += 1;
            f.blocks.push((id, (*b).into()));
        }
        f
    }

    fn to_cloud_xml(&self, doc_id: &str) -> String {
        let mut s = format!("<title id=\"{doc_id}\">{}</title>", self.title);
        for (id, xml) in &self.blocks {
            // Inject id="..." into the opening tag (or first <li> inside <ul>/<ol>).
            if let Some(pos) = xml.find("<li") {
                s.push_str(&xml[..pos + 3]);
                s.push_str(&format!(" id=\"{id}\""));
                s.push_str(&xml[pos + 3..]);
            } else if let Some(pos) = xml.find(['>', ' ']) {
                s.push_str(&xml[..pos]);
                s.push_str(&format!(" id=\"{id}\""));
                s.push_str(&xml[pos..]);
            }
        }
        s
    }
}

struct FakeTransport {
    docs: Mutex<HashMap<String, FakeDoc>>,
    wikis: Mutex<HashMap<String, String>>,
    fail_first_revision: Mutex<bool>,
}

impl FakeTransport {
    fn new() -> Self {
        Self { docs: Default::default(), wikis: Default::default(), fail_first_revision: Mutex::new(false) }
    }

    fn insert_doc(&self, token: &str, title: &str, blocks: &[&str]) {
        self.docs.lock().unwrap().insert(token.into(), FakeDoc::new(title, blocks));
    }

    fn edit_remote(&self, token: &str, f: impl FnOnce(&mut FakeDoc)) {
        let mut g = self.docs.lock().unwrap();
        let d = g.get_mut(token).unwrap();
        f(d);
        d.revision += 1;
    }
}

impl FeishuTransport for FakeTransport {
    fn resolve_wiki(&self, node_token: &str, _work_dir: &Path) -> transport::TransportResult<WikiTarget> {
        let g = self.wikis.lock().unwrap();
        let tok = g.get(node_token).cloned().ok_or_else(|| TransportError::NotFound(node_token.into()))?;
        Ok(WikiTarget { obj_token: tok, title: String::new() })
    }

    fn probe_revision(&self, doc_token: &str, _work_dir: &Path) -> transport::TransportResult<DocProbe> {
        let g = self.docs.lock().unwrap();
        let d = g.get(doc_token).ok_or_else(|| TransportError::NotFound(doc_token.into()))?;
        Ok(DocProbe { revision: d.revision, title: Some(d.title.clone()) })
    }

    fn fetch(&self, doc_token: &str, _work_dir: &Path) -> transport::TransportResult<RawCloudDoc> {
        let g = self.docs.lock().unwrap();
        let d = g.get(doc_token).ok_or_else(|| TransportError::NotFound(doc_token.into()))?;
        Ok(RawCloudDoc { document_id: doc_token.into(), revision: d.revision, xml: d.to_cloud_xml(doc_token) })
    }

    fn download_media(&self, media_token: &str, output_rel: &str, work_dir: &Path) -> transport::TransportResult<PathBuf> {
        let p = work_dir.join(format!("{output_rel}.png"));
        if let Some(parent) = p.parent() { let _ = std::fs::create_dir_all(parent); }
        std::fs::write(&p, format!("img:{media_token}")).unwrap();
        Ok(p)
    }

    fn apply_ops(&self, doc_token: &str, expected_revision: Option<u64>, ops: &[PushOp], _work_dir: &Path) -> transport::TransportResult<Option<u64>> {
        {
            let mut fail = self.fail_first_revision.lock().unwrap();
            if *fail {
                *fail = false;
                return Err(TransportError::RevisionConflict("simulated race".into()));
            }
        }
        let mut g = self.docs.lock().unwrap();
        let d = g.get_mut(doc_token).ok_or_else(|| TransportError::NotFound(doc_token.into()))?;
        if let Some(exp) = expected_revision && exp != d.revision {
            return Err(TransportError::RevisionConflict(format!("expected {exp}, got {}", d.revision)));
        }
        for op in ops {
            match op {
                PushOp::Title { xml, .. } => {
                    let parsed = docxml::parse(xml).unwrap();
                    d.title = parsed.title.text;
                }
                PushOp::Replace { block_id, xml } => {
                    let pos = d.blocks.iter().position(|(id, _)| id == block_id).unwrap();
                    let parsed = docxml::parse(&format!("<title></title>{xml}")).unwrap();
                    let items: Vec<_> = keys(&parsed.blocks).into_iter().map(|k| {
                        let id = format!("blk{}", d.next_id);
                        d.next_id += 1;
                        (id, k)
                    }).collect();
                    d.blocks.splice(pos..=pos, items);
                }
                PushOp::Delete { block_ids } => {
                    for bid in block_ids {
                        let pos = d.blocks.iter().position(|(id, _)| id == bid).unwrap();
                        d.blocks.remove(pos);
                    }
                }
                PushOp::InsertAfter { anchor, xml } => {
                    let at = if anchor == "0" { 0 } else { d.blocks.iter().position(|(id, _)| id == anchor).unwrap() + 1 };
                    let parsed = docxml::parse(&format!("<title></title>{xml}")).unwrap();
                    let items: Vec<_> = keys(&parsed.blocks).into_iter().map(|k| {
                        let id = format!("blk{}", d.next_id);
                        d.next_id += 1;
                        (id, k)
                    }).collect();
                    d.blocks.splice(at..at, items);
                }
            }
            d.revision += 1;
        }
        Ok(Some(d.revision))
    }

    fn create_doc(&self, title: Option<&str>, xml: &str, _work_dir: &Path) -> transport::TransportResult<CreatedDoc> {
        let token = "DoxcnCreatedDocument00001".to_string();
        let parsed = docxml::parse(xml).unwrap();
        let t = title.unwrap_or(&parsed.title.text);
        let ks = keys(&parsed.blocks);
        let refs: Vec<&str> = ks.iter().map(String::as_str).collect();
        self.docs.lock().unwrap().insert(token.clone(), FakeDoc::new(t, &refs));
        Ok(CreatedDoc { url: format!("https://feishu.cn/docx/{token}"), token })
    }

    fn check_auth(&self, _work_dir: &Path) -> AuthInfo {
        AuthInfo { installed: true, brand: "feishu".into(), user_ready: true, bot_ready: true, user_name: "tester".into(), app_id: "cli_test".into(), detail: String::new() }
    }
}

#[test]
fn e2e_import_edit_push_pull_and_race_recovery() {
    let tmp = crate::vfs::temp_dir().join(format!("quiet-feishu-test-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&tmp).unwrap();
    let tr = FakeTransport::new();
    let tok = "DoxcnTestDocumentToken0001";
    tr.insert_doc(tok, "周报", &["<p>第一段</p>", "<p>第二段</p>", "<p>第三段</p>"]);
    tr.wikis.lock().unwrap().insert("WikcnTestWikiNodeToken0001".into(), tok.into());

    // 1. Import via wiki URL.
    let imported = import_from_link(&tr, &tmp, ".assets", "https://my.larkoffice.com/wiki/WikcnTestWikiNodeToken0001").unwrap();
    assert_eq!(imported.link.token, tok);
    assert_eq!(imported.link.wiki_node, "WikcnTestWikiNodeToken0001");
    assert_eq!(imported.cloud.doc.title.text, "周报");
    assert_eq!(texts(&imported.cloud.doc), ["第一段", "第二段", "第三段"]);

    // Probe detects no remote change yet.
    assert_eq!(check_remote_changed(&tr, &tmp, &imported.link).unwrap(), None);

    // 2. Local edits first paragraph, remote concurrently edits third paragraph + simulated revision race!
    let mut local = imported.cloud.doc.clone();
    local.blocks[0].text = RichText::plain("第一段（本地改）");
    tr.edit_remote(tok, |d| { d.blocks[2].1 = "<p>第三段（云端改）</p>".into(); });
    assert_eq!(check_remote_changed(&tr, &tmp, &imported.link).unwrap(), Some(2));

    *tr.fail_first_revision.lock().unwrap() = true;
    let pushed = push_doc(&tr, &tmp, ".assets", &imported.link, Some(&imported.cloud.doc), &local, false).unwrap();
    let PushOutcome::Pushed { ops_sent, after, merged_from_cloud, .. } = pushed else { panic!("expected Pushed, got {pushed:?}") };
    assert_eq!(ops_sent, 1);
    assert!(merged_from_cloud);
    assert_eq!(texts(&after.doc), ["第一段（本地改）", "第二段", "第三段（云端改）"]);

    // 3. Echo suppression: immediately pushing again does nothing!
    let mut link2 = imported.link.clone();
    link2.revision = after.revision;
    let noop = push_doc(&tr, &tmp, ".assets", &link2, Some(&after.doc), &after.doc, false).unwrap();
    assert!(matches!(noop, PushOutcome::NoChanges { .. }));

    // 4. Conflicting edit on same paragraph pauses push and returns cloud copy.
    let mut local_clash = after.doc.clone();
    local_clash.blocks[1].text = RichText::plain("第二段（本地冲突）");
    tr.edit_remote(tok, |d| { d.blocks[1].1 = "<p>第二段（云端冲突）</p>".into(); });
    let clash = push_doc(&tr, &tmp, ".assets", &link2, Some(&after.doc), &local_clash, false).unwrap();
    let PushOutcome::ConflictsPaused { merged_doc, cloud, conflict_count, .. } = clash else { panic!("expected ConflictsPaused, got {clash:?}") };
    assert_eq!(conflict_count, 1);
    assert_eq!(texts(&merged_doc), ["第一段（本地改）", "第二段（本地冲突）", "第三段（云端改）"]);
    assert_eq!(texts(&cloud.doc), ["第一段（本地改）", "第二段（云端冲突）", "第三段（云端改）"]);

    // 5. State directory round-trip.
    let sdir = SyncDir(tmp.join("state"));
    sdir.save_link("doc1", &link2).unwrap();
    sdir.save_base("doc1", &after.doc).unwrap();
    assert_eq!(sdir.load_link("doc1").unwrap().token, tok);
    assert_eq!(sdir.find_by_token(tok).as_deref(), Some("doc1"));
    assert_eq!(keys(&sdir.load_base("doc1").unwrap().blocks), keys(&after.doc.blocks));
    sdir.remove("doc1");
    assert!(sdir.load_link("doc1").is_none());

    let _ = std::fs::remove_dir_all(&tmp);
}

#[cfg(not(target_family = "wasm"))]
#[test]
fn windows_bin_resolution_prefers_native_exe_over_cmd_shim() {
    let tmp = crate::vfs::temp_dir().join(format!("quiet-feishu-winbin-{}", uuid::Uuid::new_v4()));

    // Case 1: npm global prefix with lark-cli.cmd + node_modules/@larksuite/cli/bin/lark-cli.exe.
    let npm_dir = tmp.join("AppData Roaming").join("npm");
    let npm_exe = npm_dir.join("node_modules/@larksuite/cli/bin/lark-cli.exe");
    std::fs::create_dir_all(npm_exe.parent().unwrap()).unwrap();
    std::fs::write(npm_dir.join("lark-cli.cmd"), "@ECHO off\r\n\"%_prog%\"  \"%dp0%\\node_modules\\@larksuite\\cli\\scripts\\run.js\" %*\r\n").unwrap();
    std::fs::write(&npm_exe, b"MZ").unwrap();
    assert_eq!(transport::resolve_windows_bin_in_dirs(std::slice::from_ref(&npm_dir)), Some(npm_exe));

    // Case 2: pnpm-style .cmd shim referencing %~dp0\global\5\... with spaces in directory path.
    let pnpm_dir = tmp.join("pnpm bin");
    let pnpm_exe = pnpm_dir.join("global/5/node_modules/@larksuite/cli/bin/lark-cli.exe");
    std::fs::create_dir_all(pnpm_exe.parent().unwrap()).unwrap();
    std::fs::write(
        pnpm_dir.join("lark-cli.cmd"),
        "@ECHO off\r\n\"node\" \"%~dp0\\global\\5\\node_modules\\@larksuite\\cli\\scripts\\run.js\" %*\r\n",
    ).unwrap();
    std::fs::write(&pnpm_exe, b"MZ").unwrap();
    assert_eq!(transport::resolve_windows_bin_in_dirs(std::slice::from_ref(&pnpm_dir)), Some(pnpm_exe));

    // Case 3: missing native exe (e.g. mid self-update .old) falls back cleanly to lark-cli.cmd.
    let fallback_dir = tmp.join("fallback");
    std::fs::create_dir_all(&fallback_dir).unwrap();
    let fallback_cmd = fallback_dir.join("lark-cli.cmd");
    std::fs::write(&fallback_cmd, "@ECHO off\r\n\"%dp0%\\node_modules\\@larksuite\\cli\\scripts\\run.js\" %*\r\n").unwrap();
    assert_eq!(transport::resolve_windows_bin_in_dirs(std::slice::from_ref(&fallback_dir)), Some(fallback_cmd));

    // Case 4: direct lark-cli.exe takes highest priority.
    let scoop_dir = tmp.join("scoop");
    std::fs::create_dir_all(&scoop_dir).unwrap();
    let direct_exe = scoop_dir.join("lark-cli.exe");
    std::fs::write(&direct_exe, b"MZ").unwrap();
    assert_eq!(transport::resolve_windows_bin_in_dirs(&[scoop_dir, npm_dir]), Some(direct_exe));

    let _ = std::fs::remove_dir_all(&tmp);
}

#[test]
fn pc_shortcut_binding_for_feishu_avoids_altgr_and_normalizes_win_paths() {
    let mut blocks = doc(&["<img path=\".assets\\shot.png\"/>"]).blocks;
    let media = MediaMap::new();
    codec::for_cloud(&mut blocks, &media);
    let xml = docxml::serialize_blocks_compact(&blocks);
    assert!(xml.contains("path=\"@./.assets/shot.png\""), "{xml}");
}
