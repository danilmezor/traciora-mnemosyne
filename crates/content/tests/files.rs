//! Files: PPTX extraction and the image rule.
use base64::Engine;
use mnemosyne_content::{
    api::Request,
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    extraction::{self, ExtractionRecord, ExtractionStatus},
    Change, Service,
};
use std::{io::Write, path::PathBuf};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("mnemosyne-files-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn zip_of(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    for (name, body) in parts {
        zip.start_file(*name, zip::write::FileOptions::default())
            .unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    zip.finish().unwrap().into_inner()
}

/// Like `zip_of`, but every entry is deflated at the fastest level and then
/// declares an uncompressed size of 0 in both its local and central headers.
fn zip_declaring_zero(parts: &[(&str, &str)]) -> Vec<u8> {
    let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let options = zip::write::FileOptions::default().compression_level(Some(1));
    for (name, body) in parts {
        zip.start_file(*name, options).unwrap();
        zip.write_all(body.as_bytes()).unwrap();
    }
    let mut bytes = zip.finish().unwrap().into_inner();
    let u16_at = |b: &[u8], i: usize| u16::from_le_bytes([b[i], b[i + 1]]) as usize;
    let u32_at = |b: &[u8], i: usize| u32::from_le_bytes(b[i..i + 4].try_into().unwrap()) as usize;
    let eocd = bytes.len() - 22;
    assert_eq!(&bytes[eocd..eocd + 4], b"PK\x05\x06");
    let mut at = u32_at(&bytes, eocd + 16);
    for _ in 0..u16_at(&bytes, eocd + 10) {
        assert_eq!(&bytes[at..at + 4], b"PK\x01\x02");
        let local = u32_at(&bytes, at + 42);
        bytes[at + 24..at + 28].fill(0);
        bytes[local + 22..local + 26].fill(0);
        at += 46 + u16_at(&bytes, at + 28) + u16_at(&bytes, at + 30) + u16_at(&bytes, at + 32);
    }
    bytes
}

/// Runs the bounded extraction worker entry in-process on `bytes`.
fn run_worker(name: &str, format: &str, bytes: &[u8]) -> ExtractionRecord {
    let dir = scratch(name);
    let input = dir.join("input");
    let output = dir.join("output.json");
    std::fs::write(&input, bytes).unwrap();
    let args: Vec<String> = vec![
        "mnemosyne".into(),
        "--internal-extract-worker".into(),
        format.into(),
        input.to_string_lossy().into(),
        output.to_string_lossy().into(),
    ];
    assert!(extraction::worker_main(&args));
    let record = serde_json::from_slice(&std::fs::read(&output).unwrap()).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    record
}

const NS: &str = r#"xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" xmlns:p="http://schemas.openxmlformats.org/presentationml/2006/main""#;

fn shape(placeholder: Option<&str>, paragraphs: &[&[&str]]) -> String {
    let ph = placeholder
        .map(|t| format!(r#"<p:nvPr><p:ph type="{t}"/></p:nvPr>"#))
        .unwrap_or_else(|| "<p:nvPr/>".into());
    let body: String = paragraphs
        .iter()
        .map(|runs| {
            let runs: String = runs
                .iter()
                .map(|t| format!(r#"<a:r><a:rPr lang="en-US"/><a:t>{t}</a:t></a:r>"#))
                .collect();
            format!("<a:p>{runs}</a:p>")
        })
        .collect();
    format!(
        r#"<p:sp><p:nvSpPr><p:cNvPr id="2" name="Shape"/><p:cNvSpPr/>{ph}</p:nvSpPr><p:spPr/><p:txBody><a:bodyPr/><a:lstStyle/>{body}</p:txBody></p:sp>"#
    )
}

fn slide(shapes: &[String]) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><p:sld {NS}><p:cSld><p:spTree><p:nvGrpSpPr><p:cNvPr id="1" name=""/><p:cNvGrpSpPr/><p:nvPr/></p:nvGrpSpPr><p:grpSpPr/>{}</p:spTree></p:cSld></p:sld>"#,
        shapes.concat()
    )
}

#[test]
fn pptx_extracts_slide_text_with_locations() {
    assert_eq!(extraction::format("decks/Quarterly.PPTX"), Some("pptx"));
    let presentation = format!(r#"<p:presentation {NS}/>"#);
    let first = slide(&[
        shape(Some("ctrTitle"), &[&["Quarterly ", "Re", "view"]]),
        shape(Some("subTitle"), &[&["Prepared by ops"]]),
    ]);
    let second = slide(&[
        shape(Some("title"), &[&["Road", "map"]]),
        // Review Focus 1: runs split inside a word join with no inserted space.
        shape(
            None,
            &[&["Ship ", "extrac", "tion"], &["Fix &amp; ", "verify"]],
        ),
    ]);
    // Untitled slide: section is "".
    let tenth = slide(&[shape(None, &[&["Appendix ", "notes"]])]);
    let notes = format!(
        r#"<p:notes {NS}><p:cSld><p:spTree>{}</p:spTree></p:cSld></p:notes>"#,
        shape(None, &[&["speaker secret"]])
    );
    // Numeric order: slide10 is listed first in the archive but comes last.
    let bytes = zip_of(&[
        ("[Content_Types].xml", "<Types/>"),
        ("ppt/presentation.xml", &presentation),
        ("ppt/slides/slide10.xml", &tenth),
        ("ppt/slides/slide2.xml", &second),
        ("ppt/slides/slide1.xml", &first),
        ("ppt/slides/_rels/slide1.xml.rels", "<Relationships/>"),
        ("ppt/notesSlides/notesSlide1.xml", &notes),
    ]);
    let r = run_worker("pptx", "pptx", &bytes);
    assert_eq!(r.extractor, "mnemosyne-local-pptx");
    assert_eq!(r.status, ExtractionStatus::Partial, "{r:?}");
    assert_eq!(r.limits, vec!["notes_omitted"], "{r:?}");
    let got: Vec<(&str, Option<u32>, Option<&str>)> = r
        .segments
        .iter()
        .map(|s| {
            (
                s.text.as_str(),
                s.location.slide,
                s.location.section.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        got,
        vec![
            (
                "Quarterly Review\nPrepared by ops",
                Some(1),
                Some("Quarterly Review")
            ),
            (
                "Roadmap\nShip extraction\nFix & verify",
                Some(2),
                Some("Roadmap")
            ),
            ("Appendix notes", Some(10), Some("")),
        ]
    );
    assert!(r.segments.iter().all(|s| s.location.page.is_none()
        && s.location.paragraph.is_none()
        && !s.text.contains("speaker secret")));
    let json = serde_json::to_value(&r.segments[2].location).unwrap();
    assert_eq!(json, serde_json::json!({"slide": 10, "section": ""}));
}

#[test]
fn images_are_unsupported_with_reason() {
    let root = scratch("images");
    let service = Service::open(&root).unwrap();
    let owner = TrustedIdentity {
        tenant_id: "t".into(),
        actor_id: "owner".into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    };
    service
        .bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: [Permission::Read, Permission::Edit]
                .into_iter()
                .map(|permission| Grant {
                    tenant_id: "t".into(),
                    actor_id: "owner".into(),
                    permission,
                    scope: "".into(),
                })
                .collect(),
        })
        .unwrap();
    let names = ["a.png", "b.jpg", "c.JPEG", "d.gif", "e.webp", "f.svg"];
    service
        .edit(
            &owner,
            "r0",
            names
                .iter()
                .map(|n| Change::PutBinary {
                    path: format!("pics/{n}"),
                    bytes: format!("not really {n}").into_bytes(),
                })
                .collect(),
            "seed",
            None,
            None,
        )
        .unwrap();
    for n in names {
        let path = format!("pics/{n}");
        // Status answers the image rule before any extraction ran.
        let status = service.extraction_status(&owner, &path).unwrap();
        assert_eq!(
            status.extraction.status,
            ExtractionStatus::Unsupported,
            "{n}"
        );
        assert_eq!(status.extraction.limits, vec!["image"], "{n}");
        // The rule never reads the blob: it holds with the blob file gone.
        std::fs::remove_file(root.join("blobs").join(&status.source_blob)).unwrap();
        let result = service.extracted_text(&owner, &path).unwrap();
        assert_eq!(
            result.extraction.status,
            ExtractionStatus::Unsupported,
            "{n}"
        );
        assert_eq!(result.extraction.limits, vec!["image"], "{n}");
        assert!(result.extraction.segments.is_empty());
        let status = service.extraction_status(&owner, &path).unwrap();
        assert_eq!(status.extraction.status, ExtractionStatus::Unsupported);
        assert_eq!(status.extraction.limits, vec!["image"]);
    }
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn pptx_bounds_apply() {
    let presentation = format!(r#"<p:presentation {NS}/>"#);
    let body = slide(&[shape(Some("title"), &[&["Only"]])]);
    let mut parts: Vec<(String, String)> = vec![
        ("ppt/presentation.xml".into(), presentation),
        ("ppt/slides/slide1.xml".into(), body),
    ];
    while parts.len() < 2_049 {
        parts.push((
            format!("ppt/media/filler{}.xml", parts.len()),
            "<x/>".into(),
        ));
    }
    let refs: Vec<(&str, &str)> = parts
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_str()))
        .collect();
    // Bound breaches answer `unsupported` with the DOCX reason.
    let r = run_worker("bounds", "pptx", &zip_of(&refs));
    assert_eq!(r.status, ExtractionStatus::Unsupported, "{r:?}");
    assert_eq!(r.limits, vec!["zip_entry_limit"]);
    assert!(r.segments.is_empty());
    // 2,048 entries stay within bounds.
    let r = run_worker("bounds-ok", "pptx", &zip_of(&refs[..2_048]));
    assert_eq!(r.status, ExtractionStatus::Succeeded, "{r:?}");
    assert_eq!(r.segments[0].location.slide, Some(1));
}

#[test]
fn pptx_bounds_count_inflated_bytes() {
    let presentation = format!(r#"<p:presentation {NS}/>"#);
    // Five parts of 7.5 MiB each: every part is under 8 MiB, but together they
    // inflate past 32 MiB while declaring 0 bytes.
    let filler = format!("<p:sld {NS}>{}</p:sld>", " ".repeat(15 * 512 * 1024));
    let mut parts = vec![("ppt/presentation.xml".to_string(), presentation.clone())];
    for n in 1..=5 {
        parts.push((format!("ppt/slides/slide{n}.xml"), filler.clone()));
    }
    let refs: Vec<(&str, &str)> = parts
        .iter()
        .map(|(n, b)| (n.as_str(), b.as_str()))
        .collect();
    let bytes = zip_declaring_zero(&refs);
    let started = std::time::Instant::now();
    let r = run_worker("inflated", "pptx", &bytes);
    assert!(
        started.elapsed() < std::time::Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(r.status, ExtractionStatus::Unsupported, "{r:?}");
    assert_eq!(r.limits, vec!["zip_expanded_limit"]);
    assert!(r.segments.is_empty());
    // One part declared small but really over 8 MiB: whole-file DOCX answer.
    let big = format!("<p:sld {NS}>{}</p:sld>", " ".repeat(9 * 1024 * 1024));
    let r = run_worker(
        "inflated-part",
        "pptx",
        &zip_declaring_zero(&[
            ("ppt/presentation.xml", &presentation),
            ("ppt/slides/slide1.xml", &big),
        ]),
    );
    assert_eq!(r.status, ExtractionStatus::Unsupported, "{r:?}");
    assert_eq!(r.limits, vec!["document_xml_limit"]);
    // The shared reader applies to DOCX's main part too.
    let r = run_worker(
        "inflated-docx",
        "docx",
        &zip_declaring_zero(&[("word/document.xml", &big)]),
    );
    assert_eq!(r.status, ExtractionStatus::Unsupported, "{r:?}");
    assert_eq!(r.limits, vec!["document_xml_limit"]);
    // Depth breaches answer for the whole file exactly as DOCX.
    let deep = format!(
        "<p:sld {NS}>{}{}</p:sld>",
        "<a:x>".repeat(130),
        "</a:x>".repeat(130)
    );
    let good = slide(&[shape(Some("title"), &[&["Fine"]])]);
    let r = run_worker(
        "deep",
        "pptx",
        &zip_of(&[
            ("ppt/presentation.xml", &presentation),
            ("ppt/slides/slide1.xml", &good),
            ("ppt/slides/slide2.xml", &deep),
        ]),
    );
    assert_eq!(r.status, ExtractionStatus::Failed, "{r:?}");
    assert_eq!(r.limits, vec!["xml_depth_limit"]);
    assert!(r.segments.is_empty());
}

#[test]
fn pptx_follows_presentation_order_and_flags_embedded_content() {
    let presentation = format!(
        r#"<p:presentation {NS} xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><p:sldIdLst><p:sldId id="256" r:id="rId7"/><p:sldId id="257" r:id="rId2"/><p:sldId id="258" r:id="rId3"/></p:sldIdLst></p:presentation>"#
    );
    let rels = r#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId2" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide1.xml"/><Relationship Id="rId3" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="/ppt/slides/slide2.xml"/><Relationship Id="rId7" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/slide" Target="slides/slide3.xml"/></Relationships>"#;
    let chart = r#"<p:graphicFrame><p:nvGraphicFramePr><p:cNvPr id="5" name="Chart"/></p:nvGraphicFramePr><a:graphic><a:graphicData uri="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:chart xmlns:c="c" r:id="rId9" xmlns:r="r"/></a:graphicData></a:graphic></p:graphicFrame>"#;
    let bytes = zip_of(&[
        ("ppt/presentation.xml", &presentation),
        ("ppt/_rels/presentation.xml.rels", rels),
        (
            "ppt/slides/slide1.xml",
            &slide(&[shape(Some("title"), &[&["Second"]]), chart.to_string()]),
        ),
        (
            "ppt/slides/slide2.xml",
            &slide(&[shape(Some("title"), &[&["Third"]])]),
        ),
        (
            "ppt/slides/slide3.xml",
            &slide(&[shape(Some("title"), &[&["First"]])]),
        ),
    ]);
    let r = run_worker("order", "pptx", &bytes);
    let got: Vec<(&str, Option<u32>)> = r
        .segments
        .iter()
        .map(|s| (s.text.as_str(), s.location.slide))
        .collect();
    assert_eq!(
        got,
        vec![("First", Some(1)), ("Second", Some(2)), ("Third", Some(3))]
    );
    assert_eq!(r.status, ExtractionStatus::Partial, "{r:?}");
    assert_eq!(r.limits, vec!["embedded_content_omitted"]);
}

fn owner_service(name: &str) -> (PathBuf, Service, TrustedIdentity) {
    let root = scratch(name);
    let service = Service::open(&root).unwrap();
    let owner = TrustedIdentity {
        tenant_id: "t".into(),
        actor_id: "owner".into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    };
    service
        .bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: [Permission::Read, Permission::Edit]
                .into_iter()
                .map(|permission| Grant {
                    tenant_id: "t".into(),
                    actor_id: "owner".into(),
                    permission,
                    scope: "".into(),
                })
                .collect(),
        })
        .unwrap();
    (root, service, owner)
}

fn read_request(version: u32, encoding: Option<&str>, original: bool) -> Request {
    let mut op =
        serde_json::json!({"type": "read_binary", "path": "bin/a.dat", "original": original});
    if let Some(e) = encoding {
        op["encoding"] = e.into();
    }
    serde_json::from_value(serde_json::json!({"version": version, "operation": op})).unwrap()
}

#[test]
fn read_binary_base64_round_trips() {
    // Review Focus 2: base64 of 16 MiB is 4 * ceil(16 MiB / 3) = 22_369_624
    // bytes (~21.3 MiB), far below the 256 MiB response frame limit, even with
    // JSON overhead; the array form would be several times larger.
    const SIXTEEN_MIB: usize = 16 * 1024 * 1024;
    let encoded_len = 4 * SIXTEEN_MIB.div_ceil(3);
    assert!(mnemosyne_content::transport::MAX_RESPONSE_BYTES > encoded_len + 1024 * 1024);
    let (root, service, owner) = owner_service("base64");
    let bytes: Vec<u8> = (0..SIXTEEN_MIB as u64)
        .map(|i| (i.wrapping_mul(2654435761) >> 7) as u8)
        .collect();
    service
        .edit(
            &owner,
            "r0",
            vec![Change::PutBinary {
                path: "bin/a.dat".into(),
                bytes: bytes.clone(),
            }],
            "seed",
            None,
            None,
        )
        .unwrap();
    let b64 = base64::engine::general_purpose::STANDARD;
    for original in [false, true] {
        let r = service
            .read_binary(&owner, "bin/a.dat", None, original, Some("base64"))
            .unwrap();
        assert!(r.bytes.is_empty());
        let text = r.bytes_base64.as_deref().unwrap();
        assert_eq!(text.len(), encoded_len);
        assert_eq!(b64.decode(text).unwrap(), bytes, "original={original}");
        let json = serde_json::to_vec(&r).unwrap();
        assert!(json.len() < mnemosyne_content::transport::MAX_RESPONSE_BYTES);
    }
    // The default array path is unchanged and omits the new field.
    let r = service
        .read_binary(&owner, "bin/a.dat", None, false, None)
        .unwrap();
    assert_eq!(r.bytes, bytes);
    assert!(r.bytes_base64.is_none());
    assert!(!serde_json::to_string(&r).unwrap().contains("bytes_base64"));
    // Through the v2 API.
    let resp = service.execute(&owner, read_request(2, Some("base64"), true));
    assert!(resp.error.is_none(), "{:?}", resp.error);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn encoding_is_validated() {
    let (root, service, owner) = owner_service("encoding");
    service
        .edit(
            &owner,
            "r0",
            vec![Change::PutBinary {
                path: "bin/a.dat".into(),
                bytes: vec![1, 2, 3],
            }],
            "seed",
            None,
            None,
        )
        .unwrap();
    for bad in ["hex", "BASE64", ""] {
        let e = service
            .read_binary(&owner, "bin/a.dat", None, false, Some(bad))
            .unwrap_err();
        assert_eq!(e.code, "invalid_request", "{bad}");
        let resp = service.execute(&owner, read_request(2, Some(bad), false));
        assert_eq!(resp.error.unwrap().code, "invalid_request", "{bad}");
    }
    let r = service
        .read_binary(&owner, "bin/a.dat", None, false, Some("array"))
        .unwrap();
    assert_eq!(r.bytes, vec![1, 2, 3]);
    assert!(r.bytes_base64.is_none());
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn read_binary_v1_refuses_encoding() {
    let (root, service, owner) = owner_service("v1encoding");
    let resp = service.execute(&owner, read_request(1, Some("base64"), false));
    assert_eq!(resp.error.unwrap().code, "unsupported_version");
    std::fs::remove_dir_all(root).unwrap();
}

// ---- rebase_proposal ----

use mnemosyne_content::{
    api::ApiResult,
    auth::{Member, TenantPolicy},
    proposals::ProposalListRequest,
    Origin, Proposal, ProposalState,
};

struct Ranked {
    root: PathBuf,
    s: Service,
    admin: TrustedIdentity,
    middle: TrustedIdentity,
    senior: TrustedIdentity,
}
impl Drop for Ranked {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn person(actor: &str) -> TrustedIdentity {
    TrustedIdentity {
        tenant_id: "t1".into(),
        actor_id: actor.into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    }
}
fn put(path: &str, content: &str) -> Change {
    Change::Put {
        path: path.into(),
        content: content.into(),
    }
}
fn code<T: std::fmt::Debug>(r: Result<T, mnemosyne_content::ContentError>) -> String {
    r.unwrap_err().code
}
const DOC: &str = "Intro\n\nAlpha\n\nBeta\n\nOutro\n";

const MEMBERS: [(&str, &str); 5] = [
    ("root", "administrator"),
    ("junior-j", "junior"),
    ("middle-m", "middle"),
    ("middle-n", "middle"),
    ("senior-s", "senior"),
];
/// Tenant "t1" on the default ladder with `members` (human) and root's Admin grant plus
/// `grants`.
fn ranked_policy(members: &[(&str, &str)], grants: Vec<Grant>) -> Policy {
    let mut all = vec![Grant {
        tenant_id: "t1".into(),
        actor_id: "root".into(),
        permission: Permission::Admin,
        scope: "".into(),
    }];
    all.extend(grants);
    Policy {
        grants: all,
        tenants: std::collections::BTreeMap::from([(
            "t1".to_string(),
            TenantPolicy {
                members: members
                    .iter()
                    .map(|(id, role)| {
                        (
                            id.to_string(),
                            Member {
                                role: role.to_string(),
                                kind: ActorKind::Human,
                            },
                        )
                    })
                    .collect(),
                ..TenantPolicy::default()
            },
        )]),
    }
}
fn grant(actor: &str, permission: Permission, scope: &str) -> Grant {
    Grant {
        tenant_id: "t1".into(),
        actor_id: actor.into(),
        permission,
        scope: scope.into(),
    }
}

/// Tenant "t1" on the default ladder: an administrator seeds `doc.md` and `other.md`.
fn ranked(name: &str) -> Ranked {
    ranked_with(
        name,
        vec![],
        &[("doc.md", DOC), ("other.md", "One\n\nTwo\n")],
    )
}
fn ranked_with(name: &str, grants: Vec<Grant>, seed: &[(&str, &str)]) -> Ranked {
    let root = scratch(name);
    let s = Service::open(&root).unwrap();
    let admin = person("root");
    s.bootstrap_policy(ranked_policy(&MEMBERS, grants)).unwrap();
    s.edit(
        &admin,
        "r0",
        seed.iter().map(|(p, c)| put(p, c)).collect(),
        "seed",
        None,
        None,
    )
    .unwrap();
    Ranked {
        root,
        s,
        admin,
        middle: person("middle-m"),
        senior: person("senior-s"),
    }
}
impl Ranked {
    fn head(&self) -> String {
        self.s.head(&self.admin).unwrap()
    }
    /// The middle proposes `changes` at today's head.
    fn propose(&self, changes: Vec<Change>, key: &str) -> Proposal {
        self.s
            .propose(&self.middle, &self.head(), changes, key, None, None)
            .unwrap()
    }
    /// The senior publishes `changes` straight to head.
    fn edit(&self, changes: Vec<Change>, key: &str) {
        self.s
            .edit(&self.senior, &self.head(), changes, key, None, None)
            .unwrap();
    }
    fn read(&self, path: &str) -> String {
        self.s.read(&self.admin, path, None).unwrap().content
    }
    fn count(&self) -> usize {
        self.s
            .list_proposals(&self.admin, &ProposalListRequest::default())
            .unwrap()
            .proposals
            .len()
    }
}

#[test]
fn rebase_keeps_fitting_paragraphs_and_drops_conflicts() {
    let fx = ranked("rebase-fits");
    let origin = Origin {
        source: "assistant".into(),
        model: Some("m".into()),
        chat_id: Some("c".into()),
    };
    let old =
        fx.s.propose(
            &fx.middle,
            &fx.head(),
            vec![
                put(
                    "doc.md",
                    "Intro\n\nAlpha by middle\n\nBeta by middle\n\nOutro\n",
                ),
                put("other.md", "One\n\nTwo by middle\n"),
            ],
            "p1",
            Some("tidy the doc"),
            Some(origin.clone()),
        )
        .unwrap();
    // Head changes Beta (same paragraph: conflict) and other.md's Two (the whole file conflicts).
    fx.edit(
        vec![
            put("doc.md", "Intro\n\nAlpha\n\nBeta by senior\n\nOutro\n"),
            put("other.md", "One\n\nTwo by senior\n"),
        ],
        "e1",
    );
    let head = fx.head();
    let new =
        fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "rb1")
            .unwrap();
    assert_eq!(new.proposer, fx.middle);
    assert_eq!(new.base_revision, head);
    assert_eq!(new.reason.as_deref(), Some("tidy the doc"));
    assert_eq!(new.origin, Some(origin));
    assert_eq!(new.supersedes.as_deref(), Some(old.id.as_str()));
    assert_eq!(new.state, ProposalState::Pending);
    assert_eq!(
        new.changes,
        vec![put(
            "doc.md",
            "Intro\n\nAlpha by middle\n\nBeta by senior\n\nOutro\n"
        )]
    );

    let was = fx.s.review(&fx.senior, &old.id).unwrap();
    assert_eq!(was.state, ProposalState::Superseded);
    let d = was.decisions.last().unwrap();
    assert_eq!(d.actor, fx.senior);
    assert!(!d.approved);
    assert_eq!(
        d.reason.as_deref(),
        Some(format!("Superseded by {}", new.id).as_str())
    );
    // The superseded proposal still renders; nobody can decide it any more.
    fx.s.render(&fx.senior, None, Some(&old.id), None, 0)
        .unwrap();
    assert!(fx
        .s
        .deciders(&fx.senior, &old.id)
        .unwrap()
        .deciders
        .is_empty());
    assert_eq!(
        code(fx.s.approve(&fx.senior, &old.id, &old.digest, "a0", None)),
        "invalid_review_state"
    );
    assert_eq!(
        code(fx.s.apply(&fx.senior, &old.id, &old.digest, "ap0")),
        "invalid_review_state"
    );

    // The rebased proposal is the middle's: the senior may approve it and apply it.
    let rendered =
        fx.s.render(&fx.senior, None, Some(&new.id), None, 0)
            .unwrap();
    assert!(!rendered.stale);
    fx.s.approve(&fx.senior, &new.id, &new.digest, "a1", None)
        .unwrap();
    fx.s.apply(&fx.senior, &new.id, &new.digest, "ap1").unwrap();
    assert_eq!(
        fx.read("doc.md"),
        "Intro\n\nAlpha by middle\n\nBeta by senior\n\nOutro\n"
    );
    assert_eq!(fx.read("other.md"), "One\n\nTwo by senior\n");
}

#[test]
fn rebase_follows_renames() {
    let fx = ranked("rebase-rename");
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n")],
        "p1",
    );
    fx.edit(
        vec![Change::Rename {
            path: "doc.md".into(),
            to: "moved.md".into(),
        }],
        "mv",
    );
    fx.edit(
        vec![put(
            "moved.md",
            "Intro\n\nAlpha\n\nBeta\n\nOutro by senior\n",
        )],
        "e1",
    );
    let new =
        fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "rb1")
            .unwrap();
    assert_eq!(
        new.changes,
        vec![put(
            "moved.md",
            "Intro\n\nAlpha by middle\n\nBeta\n\nOutro by senior\n"
        )]
    );
    assert_eq!(new.diff.len(), 1);
    assert_eq!(new.diff[0].path, "moved.md");
    assert_eq!(new.diff[0].previous_path, None);
}

#[test]
fn rebase_nothing_fits() {
    let fx = ranked("rebase-nothing");
    // The same paragraph changed at head.
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha\n\nBeta by middle\n\nOutro\n")],
        "p1",
    );
    // A file deleted at head.
    let gone = fx.propose(vec![put("other.md", "One\n\nTwo by middle\n")], "p2");
    fx.edit(
        vec![
            put("doc.md", "Intro\n\nAlpha\n\nBeta by senior\n\nOutro\n"),
            Change::Delete {
                path: "other.md".into(),
            },
        ],
        "e1",
    );
    let before = fx.count();
    for p in [&old, &gone] {
        assert_eq!(
            code(fx.s.rebase_proposal(&fx.senior, &p.id, &p.digest, "fitting", &p.id)),
            "nothing_fits"
        );
        assert_eq!(
            fx.s.review(&fx.senior, &p.id).unwrap().state,
            ProposalState::Pending
        );
    }
    assert_eq!(fx.count(), before);
}

#[test]
fn rebase_unsupported_for_non_put_changes() {
    let fx = ranked("rebase-unsupported");
    fx.edit(
        vec![Change::PutBinary {
            path: "bin.dat".into(),
            bytes: vec![1, 2, 3],
        }],
        "bin",
    );
    let renamed = fx.propose(
        vec![
            put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n"),
            Change::Rename {
                path: "other.md".into(),
                to: "else.md".into(),
            },
        ],
        "p1",
    );
    let binary = fx.propose(vec![put("bin.dat", "now text")], "p2");
    fx.edit(
        vec![put("doc.md", "Intro\n\nAlpha\n\nBeta\n\nOutro by senior\n")],
        "e1",
    );
    for p in [&renamed, &binary] {
        assert_eq!(
            code(fx.s.rebase_proposal(&fx.senior, &p.id, &p.digest, "fitting", &p.id)),
            "rebase_unsupported"
        );
        assert_eq!(
            fx.s.review(&fx.senior, &p.id).unwrap().state,
            ProposalState::Pending
        );
    }
}

#[test]
fn rebase_is_idempotent_and_supersedes() {
    let fx = ranked("rebase-idempotent");
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n")],
        "p1",
    );
    fx.edit(
        vec![put("doc.md", "Intro\n\nAlpha\n\nBeta\n\nOutro by senior\n")],
        "e1",
    );
    assert_eq!(
        code(fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "all", "k0")),
        "invalid_request"
    );
    assert_eq!(
        code(fx.s.rebase_proposal(&fx.senior, &old.id, "nope", "fitting", "k0")),
        "proposal_mismatch"
    );
    // Through the v2 API; v1 does not know the operation.
    let request = |version: u32| -> Request {
        serde_json::from_value(serde_json::json!({
            "version": version,
            "idempotency_key": "k1",
            "operation": {
                "type": "rebase_proposal",
                "proposal_id": old.id,
                "digest": old.digest,
                "keep": "fitting",
            },
        }))
        .unwrap()
    };
    assert_eq!(
        fx.s.execute(&fx.senior, request(1)).error.unwrap().code,
        "unsupported_version"
    );
    let first = match fx.s.execute(&fx.senior, request(2)).result.unwrap() {
        ApiResult::Proposal(p) => p,
        other => panic!("{other:?}"),
    };
    let count = fx.count();
    let again =
        fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "k1")
            .unwrap();
    assert_eq!(again.id, first.id);
    assert_eq!(again.digest, first.digest);
    assert_eq!(fx.count(), count);
    assert_eq!(
        code(fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "k2")),
        "invalid_review_state"
    );
    let listed =
        fx.s.list_proposals(
            &fx.admin,
            &ProposalListRequest {
                states: vec![ProposalState::Superseded],
                ..Default::default()
            },
        )
        .unwrap();
    assert_eq!(
        listed.proposals.iter().map(|p| &p.id).collect::<Vec<_>>(),
        vec![&old.id]
    );
    let was = fx.s.review(&fx.senior, &old.id).unwrap();
    assert_eq!(was.decisions.len(), 1);
}

#[test]
fn rebase_requires_a_decider_or_the_proposer() {
    let fx = ranked("rebase-who");
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n")],
        "p1",
    );
    fx.edit(
        vec![put("doc.md", "Intro\n\nAlpha\n\nBeta\n\nOutro by senior\n")],
        "e1",
    );
    // Both can read doc.md; neither proposed it nor may decide it.
    for who in [person("junior-j"), person("middle-n")] {
        assert_eq!(
            code(fx.s.rebase_proposal(&who, &old.id, &old.digest, "fitting", "x")),
            "forbidden"
        );
    }
    assert_eq!(
        fx.s.review(&fx.admin, &old.id).unwrap().state,
        ProposalState::Pending
    );
    // The proposer may rebase its own proposal.
    let new =
        fx.s.rebase_proposal(&fx.middle, &old.id, &old.digest, "fitting", "own")
            .unwrap();
    assert_eq!(new.proposer, fx.middle);
    assert_eq!(new.supersedes.as_deref(), Some(old.id.as_str()));

    // A proposal on a path the caller cannot read answers as if it did not exist.
    fx.s.edit(
        &fx.admin,
        &fx.head(),
        vec![
            Change::CreateFolder {
                path: "senior-only".into(),
            },
            put("senior-only/pay.md", "Pay\n"),
        ],
        "seed-hidden",
        None,
        None,
    )
    .unwrap();
    fx.s.edit(
        &fx.admin,
        &fx.head(),
        vec![Change::SetMeta {
            path: "senior-only".into(),
            seen_from: Some("senior".into()),
            colour: None,
        }],
        "hide",
        None,
        None,
    )
    .unwrap();
    let hidden =
        fx.s.propose(
            &fx.senior,
            &fx.head(),
            vec![put("senior-only/pay.md", "Pay more\n")],
            "p2",
            None,
            None,
        )
        .unwrap();
    fx.edit(vec![put("doc.md", "Changed\n")], "e2");
    assert_eq!(
        code(fx.s.rebase_proposal(&fx.middle, &hidden.id, &hidden.digest, "fitting", "h")),
        "not_found"
    );
}

#[test]
fn rebase_not_stale() {
    let fx = ranked("rebase-fresh");
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n")],
        "p1",
    );
    assert_eq!(
        code(fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "rb")),
        "not_stale"
    );
    assert_eq!(
        fx.s.review(&fx.senior, &old.id).unwrap().state,
        ProposalState::Pending
    );
}

#[test]
fn rebase_not_stale_when_only_other_files_changed() {
    let fx = ranked("rebase-other");
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n")],
        "p1",
    );
    fx.edit(vec![put("other.md", "One\n\nTwo by senior\n")], "e1");
    assert_eq!(
        code(fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "rb")),
        "not_stale"
    );
    // Once doc.md itself changes the proposal is stale and rebases.
    fx.edit(
        vec![put("doc.md", "Intro\n\nAlpha\n\nBeta\n\nOutro by senior\n")],
        "e2",
    );
    let new =
        fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "rb2")
            .unwrap();
    assert_eq!(new.base_revision, fx.head());
}

/// Rebases the middle's `proposed` text of `path` after the senior wrote `today`.
fn rebased(name: &str, seed: &str, proposed: &str, today: &str) -> String {
    let fx = ranked_with(name, vec![], &[("doc.md", seed)]);
    let old = fx.propose(vec![put("doc.md", proposed)], "p1");
    fx.edit(vec![put("doc.md", today)], "e1");
    let new =
        fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "rb")
            .unwrap();
    match &new.changes[..] {
        [Change::Put { path, content }] if path == "doc.md" => content.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn rebase_keeps_head_bytes_outside_the_fitting_paragraphs() {
    // CRLF at head: only Alpha's bytes change, and they keep CRLF.
    let crlf = "Intro\r\n\r\nAlpha\r\nline 2\r\n\r\n\r\nBeta\r\nmore\r\n\r\nOutro\r\n";
    let today = crlf.replace("Outro", "Outro by senior");
    assert_eq!(
        rebased(
            "rebase-crlf",
            crlf,
            "Intro\n\nAlpha by middle\nline 2\n\nBeta\nmore\n\nOutro\n",
            &today,
        ),
        today.replace("Alpha\r\nline 2", "Alpha by middle\r\nline 2")
    );
    // Python spacing: leading blank line, double blank lines, trailing blank lines.
    let py =
        "\n# header\n\n\nimport os\n\n\ndef a():\n    return 1\n\n\ndef b():\n    return 2\n\n\n";
    let today = py.replace("return 2", "return 22");
    assert_eq!(
        rebased(
            "rebase-py",
            py,
            &py.replace("return 1", "return 11"),
            &today,
        ),
        today.replace("return 1\n", "return 11\n")
    );
}

#[test]
fn rebase_rechecks_the_proposer() {
    // Demoted to junior: may still read, may no longer propose.
    let fx = ranked("rebase-demoted");
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n")],
        "p1",
    );
    fx.edit(
        vec![put("doc.md", "Intro\n\nAlpha\n\nBeta\n\nOutro by senior\n")],
        "e1",
    );
    let mut members = MEMBERS;
    members[2] = ("middle-m", "junior");
    let digest = fx.s.policy_state(&fx.admin).unwrap().digest;
    fx.s.replace_policy(&fx.admin, ranked_policy(&members, vec![]), Some(&digest))
        .unwrap();
    assert_eq!(
        code(fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "rb")),
        "forbidden"
    );
    assert_eq!(
        fx.s.review(&fx.senior, &old.id).unwrap().state,
        ProposalState::Pending
    );

    // Hidden from the proposer at head: may no longer read it.
    let fx = ranked("rebase-hidden-from-proposer");
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n")],
        "p1",
    );
    fx.s.edit(
        &fx.admin,
        &fx.head(),
        vec![Change::SetMeta {
            path: "doc.md".into(),
            seen_from: Some("senior".into()),
            colour: None,
        }],
        "hide",
        None,
        None,
    )
    .unwrap();
    assert_eq!(
        code(fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "rb")),
        "forbidden"
    );
    assert_eq!(
        fx.s.review(&fx.senior, &old.id).unwrap().state,
        ProposalState::Pending
    );
}

#[test]
fn rebase_of_an_approved_proposal_drops_the_approval() {
    let fx = ranked("rebase-approved");
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n")],
        "p1",
    );
    fx.s.approve(&fx.senior, &old.id, &old.digest, "a1", None)
        .unwrap();
    fx.edit(
        vec![put("doc.md", "Intro\n\nAlpha\n\nBeta\n\nOutro by senior\n")],
        "e1",
    );
    let new =
        fx.s.rebase_proposal(&fx.senior, &old.id, &old.digest, "fitting", "rb")
            .unwrap();
    assert!(new.approver.is_none());
    let was = fx.s.review(&fx.senior, &old.id).unwrap();
    assert_eq!(was.state, ProposalState::Superseded);
    assert!(was.approver.is_none());
    assert_eq!(was.decisions.len(), 2);
    let d = &was.decisions[1];
    assert!(d.at.is_some());
    assert_eq!(d.policy_digest, was.decisions[0].policy_digest);
}

#[test]
fn rebase_by_the_root_administrator_and_never_by_an_agent() {
    // `owner` is no member: only its root Admin grant makes it a decider.
    let grants = vec![
        grant("bot", Permission::Read, ""),
        grant("bot", Permission::Approve, ""),
        grant("owner", Permission::Admin, ""),
    ];
    let fx = ranked_with("rebase-root-agent", grants, &[("doc.md", DOC)]);
    let old = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n")],
        "p1",
    );
    let other = fx.propose(
        vec![put("doc.md", "Intro\n\nAlpha\n\nBeta by middle\n\nOutro\n")],
        "p2",
    );
    fx.edit(
        vec![put("doc.md", "Intro\n\nAlpha\n\nBeta\n\nOutro by senior\n")],
        "e1",
    );
    let bot = TrustedIdentity {
        kind: ActorKind::Agent,
        ..person("bot")
    };
    assert_eq!(
        code(fx.s.rebase_proposal(&bot, &old.id, &old.digest, "fitting", "b")),
        "forbidden"
    );
    let owner = person("owner");
    let new =
        fx.s.rebase_proposal(&owner, &old.id, &old.digest, "fitting", "o")
            .unwrap();
    assert_eq!(new.proposer, fx.middle);
    let new =
        fx.s.rebase_proposal(&fx.admin, &other.id, &other.digest, "fitting", "r")
            .unwrap();
    assert_eq!(new.proposer, fx.middle);
}

#[test]
fn rebase_read_checks_the_caller_on_heads_paths() {
    // An auditor decides docs/ through scoped grants; head moves the file out of docs/.
    let grants = [Permission::Read, Permission::Approve]
        .map(|p| grant("auditor", p, "docs"))
        .to_vec();
    let fx = ranked_with("rebase-scoped", grants, &[("docs/a.md", DOC)]);
    let old = fx.propose(
        vec![put(
            "docs/a.md",
            "Intro\n\nAlpha by middle\n\nBeta\n\nOutro\n",
        )],
        "p1",
    );
    let auditor = person("auditor");
    assert!(fx
        .s
        .deciders(&auditor, &old.id)
        .unwrap()
        .deciders
        .contains(&"auditor".to_string()));
    fx.edit(
        vec![Change::Rename {
            path: "docs/a.md".into(),
            to: "finance/a.md".into(),
        }],
        "mv",
    );
    assert_eq!(
        code(fx.s.rebase_proposal(&auditor, &old.id, &old.digest, "fitting", "rb")),
        "not_found"
    );
    assert_eq!(
        fx.s.review(&fx.admin, &old.id).unwrap().state,
        ProposalState::Pending
    );
}
