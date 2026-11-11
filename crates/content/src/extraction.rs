//! Disposable, versioned local document extraction. Authority callers must hold
//! their lock and authorize the current source before calling this module.
use crate::{err, io, storage, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{Read, Write},
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

pub const VERSION: &str = "m4-v1-lopdf0.45.0-zip0.6.6-xml0.38.4";
const INPUT_LIMIT: u64 = 16 * 1024 * 1024;
const XML_LIMIT: u64 = 8 * 1024 * 1024;
const TEXT_LIMIT: usize = 2 * 1024 * 1024;
const OUTPUT_LIMIT: u64 = 8 * 1024 * 1024;
const SEGMENT_LIMIT: usize = 10_000;
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ExtractionStatus {
    Pending,
    Succeeded,
    Partial,
    Unsupported,
    Failed,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct SourceLocation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub page: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paragraph: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slide: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionSegment {
    pub text: String,
    pub location: SourceLocation,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExtractionRecord {
    pub source_blob: String,
    pub extractor: String,
    pub version: String,
    pub status: ExtractionStatus,
    pub segments: Vec<ExtractionSegment>,
    pub limits: Vec<String>,
}
pub fn format(path: &str) -> Option<&'static str> {
    match path.rsplit_once('.')?.1.to_ascii_lowercase().as_str() {
        "pdf" => Some("pdf"),
        "docx" => Some("docx"),
        "pptx" => Some("pptx"),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "svg" => Some("image"),
        _ => None,
    }
}
pub fn pending(blob: &str, media_type: &str) -> ExtractionRecord {
    ExtractionRecord {
        source_blob: blob.into(),
        extractor: format!("mnemosyne-local-{media_type}"),
        version: VERSION.into(),
        status: ExtractionStatus::Pending,
        segments: vec![],
        limits: vec![],
    }
}
fn cache(root: &Path) -> Result<Connection> {
    let dir = root.join("extraction-v1");
    fs::create_dir_all(&dir).map_err(io)?;
    let path = dir.join("cache.sqlite3");
    let initialize = || -> Result<Connection> {
        let c = Connection::open(&path).map_err(io)?;
        c.execute_batch("PRAGMA synchronous=FULL; CREATE TABLE IF NOT EXISTS extraction(blob TEXT NOT NULL, format TEXT NOT NULL, version TEXT NOT NULL, data TEXT NOT NULL, checksum TEXT NOT NULL, PRIMARY KEY(blob,format,version));").map_err(io)?;
        c.prepare("SELECT checksum FROM extraction LIMIT 1")
            .map_err(io)?;
        Ok(c)
    };
    let c = match initialize() {
        Ok(c) => c,
        Err(_) => {
            let _ = fs::remove_file(&path);
            let _ = fs::remove_file(path.with_extension("sqlite3-journal"));
            initialize()?
        }
    };
    Ok(c)
}
pub(crate) fn cached(
    root: &Path,
    blob: &str,
    media_type: &str,
) -> Result<Option<ExtractionRecord>> {
    // The image rule needs no extraction: every reader (status, text, search,
    // stat) answers it immediately, without the blob or a cache row.
    if media_type == "image" {
        let mut record = pending(blob, media_type);
        fail(&mut record, ExtractionStatus::Unsupported, "image");
        return Ok(Some(record));
    }
    let c = cache(root)?;
    let data: std::result::Result<Option<(String, String)>, _> = c
        .query_row(
            "SELECT data,checksum FROM extraction WHERE blob=?1 AND format=?2 AND version=?3",
            params![blob, media_type, VERSION],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional();
    let data = match data {
        Ok(data) => data,
        Err(_) => {
            // A damaged row/page can escape schema initialization. This cache
            // has no authority data; remove it and let the caller regenerate.
            drop(c);
            fs::remove_file(root.join("extraction-v1/cache.sqlite3")).map_err(io)?;
            return Ok(None);
        }
    };
    // Invalid derived records are discarded and regenerated, never returned.
    Ok(data
        .filter(|(d, sum)| {
            d.len() as u64 <= OUTPUT_LIMIT && format!("{:x}", Sha256::digest(d.as_bytes())) == *sum
        })
        .and_then(|(d, _)| serde_json::from_str::<ExtractionRecord>(&d).ok())
        .filter(|r| {
            r.source_blob == blob
                && r.version == VERSION
                && r.extractor == format!("mnemosyne-local-{media_type}")
                && r.status != ExtractionStatus::Pending
        }))
}
pub(crate) fn extract(
    root: &Path,
    blob: &str,
    media_type: &str,
    retry: bool,
) -> Result<ExtractionRecord> {
    if !retry {
        if let Some(record) = cached(root, blob, media_type)? {
            return Ok(record);
        }
    }
    let mut record = pending(blob, media_type);
    // Images are recognized but have no local text extractor (OCR deferred):
    // answer without reading the blob or spawning a worker.
    let bytes = if media_type == "image" {
        vec![]
    } else {
        storage::read_blob(root, blob)?
    };
    if media_type == "image" {
        fail(&mut record, ExtractionStatus::Unsupported, "image");
    } else if bytes.len() as u64 > INPUT_LIMIT {
        fail(&mut record, ExtractionStatus::Unsupported, "input_limit");
    } else if !matches!(media_type, "pdf" | "docx" | "pptx") {
        fail(
            &mut record,
            ExtractionStatus::Unsupported,
            "unsupported_format",
        );
    } else {
        let output = root
            .join("extraction-v1")
            .join(format!("{blob}-{media_type}.next"));
        fs::create_dir_all(output.parent().unwrap()).map_err(io)?;
        let _ = fs::remove_file(&output);
        let worker = std::env::var_os("MNEMOSYNE_EXTRACTION_WORKER")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::current_exe().ok())
            .ok_or_else(|| err("extraction_worker_unavailable"))?;
        let mut command = Command::new(worker);
        command
            .arg("--internal-extract-worker")
            .arg(media_type)
            .arg(root.join("blobs").join(blob))
            .arg(&output)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    // Linux enforces address space; macOS does not reliably enforce
                    // RLIMIT_AS. CPU and wall limits remain separate safeguards.
                    #[cfg(target_os = "linux")]
                    {
                        let limit = libc::rlimit {
                            rlim_cur: 512 * 1024 * 1024,
                            rlim_max: 512 * 1024 * 1024,
                        };
                        if libc::setrlimit(libc::RLIMIT_AS, &limit) != 0 {
                            return Err(std::io::Error::last_os_error());
                        }
                    }
                    let cpu = libc::rlimit {
                        rlim_cur: 8,
                        rlim_max: 8,
                    };
                    if libc::setrlimit(libc::RLIMIT_CPU, &cpu) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    let file = libc::rlimit {
                        rlim_cur: OUTPUT_LIMIT,
                        rlim_max: OUTPUT_LIMIT,
                    };
                    if libc::setrlimit(libc::RLIMIT_FSIZE, &file) != 0 {
                        return Err(std::io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        match command.spawn() {
            Err(_) => fail(&mut record, ExtractionStatus::Failed, "worker_unavailable"),
            Ok(mut child) => {
                let start = Instant::now();
                loop {
                    if output
                        .metadata()
                        .map(|m| m.len() > OUTPUT_LIMIT)
                        .unwrap_or(false)
                    {
                        let _ = child.kill();
                        let _ = child.wait();
                        fail(&mut record, ExtractionStatus::Failed, "worker_output_limit");
                        break;
                    }
                    match child.try_wait() {
                        Ok(Some(status)) => {
                            if status.success()
                                && output
                                    .metadata()
                                    .map(|m| m.len() <= OUTPUT_LIMIT)
                                    .unwrap_or(false)
                            {
                                if let Ok(data) = fs::read(&output) {
                                    if let Ok(mut result) =
                                        serde_json::from_slice::<ExtractionRecord>(&data)
                                    {
                                        result.source_blob = blob.into();
                                        record = result;
                                        break;
                                    }
                                }
                            }
                            fail(&mut record, ExtractionStatus::Failed, "worker_failed");
                            break;
                        }
                        Err(_) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            fail(&mut record, ExtractionStatus::Failed, "worker_failed");
                            break;
                        }
                        Ok(None) if start.elapsed() >= Duration::from_secs(10) => {
                            let _ = child.kill();
                            let _ = child.wait();
                            fail(&mut record, ExtractionStatus::Failed, "worker_timeout");
                            break;
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(10)),
                    }
                }
            }
        }
        let _ = fs::remove_file(output);
    }
    let c = cache(root)?;
    let data = serde_json::to_string(&record).map_err(io)?;
    failpoint("before_cache_commit");
    c.execute("INSERT OR REPLACE INTO extraction(blob,format,version,data,checksum) VALUES (?1,?2,?3,?4,?5)",params![blob,media_type,VERSION,data,format!("{:x}",Sha256::digest(data.as_bytes()))]).map_err(io)?;
    failpoint("after_cache_commit");
    Ok(record)
}
fn failpoint(name: &str) {
    if std::env::var("MNEMOSYNE_EXTRACTION_FAILPOINT").as_deref() == Ok(name) {
        std::process::exit(86);
    }
}
fn fail(r: &mut ExtractionRecord, status: ExtractionStatus, reason: &str) {
    r.status = status;
    r.segments.clear();
    r.limits.push(reason.into());
}
fn add(r: &mut ExtractionRecord, text: String, location: SourceLocation) -> bool {
    if text.trim().is_empty() {
        return true;
    }
    let used: usize = r.segments.iter().map(|s| s.text.len()).sum();
    if r.segments.len() >= SEGMENT_LIMIT || used.saturating_add(text.len()) > TEXT_LIMIT {
        r.limits.push("text_or_segment_limit".into());
        return false;
    }
    r.segments.push(ExtractionSegment { text, location });
    true
}
fn finish(r: &mut ExtractionRecord) {
    r.status = if r.segments.is_empty() {
        r.limits.push("image_only_or_empty_ocr_deferred".into());
        ExtractionStatus::Unsupported
    } else if r.limits.is_empty() {
        ExtractionStatus::Succeeded
    } else {
        ExtractionStatus::Partial
    };
}
fn pdf(bytes: &[u8], r: &mut ExtractionRecord) {
    let Ok(doc) = lopdf::Document::load_mem_with_options(
        bytes,
        lopdf::LoadOptions::with_max_decompressed_size(XML_LIMIT as usize),
    ) else {
        fail(r, ExtractionStatus::Failed, "malformed_pdf");
        return;
    };
    if doc.is_encrypted() || doc.encryption_state.is_some() {
        fail(r, ExtractionStatus::Unsupported, "encrypted_pdf");
        return;
    }
    let pages = doc.get_pages();
    if pages.len() > 1000 {
        fail(r, ExtractionStatus::Unsupported, "page_limit");
        return;
    }
    let mut page_errors = false;
    for page in pages.keys() {
        match doc.extract_text_with_limit(&[*page], XML_LIMIT as usize) {
            Ok(text) if !text.trim().is_empty() => {
                if !add(
                    r,
                    text,
                    SourceLocation {
                        page: Some(*page),
                        paragraph: None,
                        section: None,
                        slide: None,
                    },
                ) {
                    break;
                }
            }
            Ok(_) => r
                .limits
                .push(format!("page_{page}_image_only_or_empty_ocr_deferred")),
            Err(_) => {
                page_errors = true;
                r.limits.push(format!("page_{page}_extraction_failed"));
            }
        }
    }
    if r.segments.is_empty() && page_errors {
        fail(r, ExtractionStatus::Failed, "all_text_extraction_failed");
    } else {
        finish(r);
    }
}
fn docx(bytes: &[u8], r: &mut ExtractionRecord) {
    let Ok(mut archive) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else {
        fail(r, ExtractionStatus::Failed, "malformed_docx_zip");
        return;
    };
    if archive.len() > 2048 {
        fail(r, ExtractionStatus::Unsupported, "zip_entry_limit");
        return;
    }
    let mut total = 0u64;
    for i in 0..archive.len() {
        let Ok(entry) = archive.by_index(i) else {
            fail(
                r,
                ExtractionStatus::Unsupported,
                "encrypted_or_invalid_zip_entry",
            );
            return;
        };
        total = total.saturating_add(entry.size());
        if total > 32 * 1024 * 1024 {
            fail(r, ExtractionStatus::Unsupported, "zip_expanded_limit");
            return;
        }
        let name = entry.name();
        if (name.starts_with("word/header")
            || name.starts_with("word/footer")
            || name == "word/footnotes.xml"
            || name == "word/endnotes.xml"
            || name == "word/comments.xml")
            && name.ends_with(".xml")
        {
            r.limits.push("ancillary_parts_omitted".into());
        }
    }
    let Ok(entry) = archive.by_name("word/document.xml") else {
        fail(r, ExtractionStatus::Failed, "missing_document_xml");
        return;
    };
    if entry.size() > XML_LIMIT {
        fail(r, ExtractionStatus::Unsupported, "document_xml_limit");
        return;
    }
    let xml = match read_part(entry, &mut (32 * 1024 * 1024)) {
        Ok(xml) => xml,
        Err((status, reason)) => {
            fail(r, status, reason);
            return;
        }
    };
    if std::str::from_utf8(&xml).is_err() {
        fail(r, ExtractionStatus::Unsupported, "non_utf8_document_xml");
        return;
    }
    let mut reader = quick_xml::Reader::from_reader(xml.as_slice());
    let mut paragraph = 0;
    let mut section = 1u32;
    let mut text = String::new();
    let mut in_text = false;
    let mut in_paragraph = false;
    let mut depth = 0usize;
    let mut root = false;
    let mut section_break = false;
    let mut omitted_depth = None;
    loop {
        use quick_xml::events::Event;
        let event = reader.read_event();
        if let Some(omitted) = omitted_depth {
            match &event {
                Ok(Event::Start(_)) => {
                    depth += 1;
                    if depth > 128 {
                        fail(r, ExtractionStatus::Failed, "xml_depth_limit");
                        return;
                    }
                }
                Ok(Event::End(_)) => {
                    if depth == omitted {
                        omitted_depth = None;
                    }
                    depth = depth.saturating_sub(1);
                }
                Ok(Event::Eof) | Err(_) => {
                    fail(r, ExtractionStatus::Failed, "malformed_document_xml");
                    return;
                }
                Ok(Event::DocType(_)) => {
                    fail(r, ExtractionStatus::Unsupported, "xml_doctype_unsupported");
                    return;
                }
                _ => {}
            }
            continue;
        }
        match event {
            Ok(Event::Start(e)) => {
                depth += 1;
                if depth > 128 {
                    fail(r, ExtractionStatus::Failed, "xml_depth_limit");
                    return;
                }
                match e.local_name().as_ref() {
                    b"document" => root = true,
                    b"p" => {
                        paragraph += 1;
                        in_paragraph = true;
                        text.clear();
                    }
                    b"t" => in_text = true,
                    b"sectPr" => section_break = true,
                    b"drawing" | b"object" | b"txbxContent" => {
                        r.limits.push("embedded_content_omitted".into());
                        omitted_depth = Some(depth);
                    }
                    _ => {}
                }
            }
            Ok(Event::Empty(e)) => match e.local_name().as_ref() {
                b"p" => paragraph += 1,
                b"tab" if in_paragraph => text.push('\t'),
                b"br" | b"cr" if in_paragraph => text.push('\n'),
                b"sectPr" => section_break = true,
                _ => {}
            },
            Ok(Event::Text(t)) if in_text => {
                let Ok(decoded) = t.decode() else {
                    fail(r, ExtractionStatus::Failed, "invalid_xml_text");
                    return;
                };
                let Ok(unescaped) = quick_xml::escape::unescape(&decoded) else {
                    fail(r, ExtractionStatus::Failed, "invalid_xml_entity");
                    return;
                };
                text.push_str(&unescaped);
            }
            Ok(Event::CData(e)) if in_text => {
                let Ok(decoded) = e.decode() else {
                    fail(r, ExtractionStatus::Failed, "invalid_xml_text");
                    return;
                };
                text.push_str(&decoded);
            }
            Ok(Event::GeneralRef(e)) if in_text => {
                let Ok(value) = e.decode() else {
                    fail(r, ExtractionStatus::Failed, "invalid_xml_entity");
                    return;
                };
                let entity = format!("&{value};");
                let Ok(value) = quick_xml::escape::unescape(&entity) else {
                    fail(r, ExtractionStatus::Failed, "invalid_xml_entity");
                    return;
                };
                text.push_str(&value);
            }
            Ok(Event::End(e)) => {
                depth = depth.saturating_sub(1);
                match e.local_name().as_ref() {
                    b"t" => in_text = false,
                    b"p" => {
                        in_paragraph = false;
                        if !add(
                            r,
                            std::mem::take(&mut text),
                            SourceLocation {
                                page: None,
                                paragraph: Some(paragraph),
                                section: Some(section.to_string()),
                                slide: None,
                            },
                        ) {
                            break;
                        }
                        if section_break {
                            section += 1;
                            section_break = false;
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::DocType(_)) => {
                fail(r, ExtractionStatus::Unsupported, "xml_doctype_unsupported");
                return;
            }
            Ok(Event::Eof) => {
                if depth != 0 || !root {
                    fail(r, ExtractionStatus::Failed, "malformed_document_xml");
                    return;
                }
                break;
            }
            Err(_) => {
                fail(r, ExtractionStatus::Failed, "malformed_document_xml");
                return;
            }
            _ => {}
        }
    }
    r.limits.sort();
    r.limits.dedup();
    finish(r);
}
/// Appends the decoded text of a character-data event to `out`. Shared by the
/// OOXML parsers; `Err` carries the failure reason.
fn xml_text(
    event: &quick_xml::events::Event,
    out: &mut String,
) -> std::result::Result<(), &'static str> {
    use quick_xml::events::Event;
    match event {
        Event::Text(t) => {
            let decoded = t.decode().map_err(|_| "invalid_xml_text")?;
            let unescaped =
                quick_xml::escape::unescape(&decoded).map_err(|_| "invalid_xml_entity")?;
            out.push_str(&unescaped);
        }
        Event::CData(e) => out.push_str(&e.decode().map_err(|_| "invalid_xml_text")?),
        Event::GeneralRef(e) => {
            let value = e.decode().map_err(|_| "invalid_xml_entity")?;
            let value = quick_xml::escape::unescape(&format!("&{value};"))
                .map_err(|_| "invalid_xml_entity")?
                .into_owned();
            out.push_str(&value);
        }
        _ => {}
    }
    Ok(())
}
/// Reads one ZIP part, counting the bytes actually inflated (not the declared
/// size) against both the 8 MiB per-part bound and the archive's remaining
/// 32 MiB expansion budget. Errors carry DOCX's whole-file answers.
fn read_part(
    entry: impl Read,
    budget: &mut u64,
) -> std::result::Result<Vec<u8>, (ExtractionStatus, &'static str)> {
    let cap = XML_LIMIT.min(*budget);
    let mut xml = Vec::new();
    if entry.take(cap + 1).read_to_end(&mut xml).is_err() {
        return Err((ExtractionStatus::Failed, "document_xml_read_failed"));
    }
    if xml.len() as u64 > cap {
        let reason = if cap == XML_LIMIT {
            "document_xml_limit"
        } else {
            "zip_expanded_limit"
        };
        return Err((ExtractionStatus::Unsupported, reason));
    }
    *budget -= xml.len() as u64;
    Ok(xml)
}
/// PPTX: one segment per slide in presentation order (`sldIdLst`, else numeric
/// `ppt/slides/slideN.xml` order), located by `{slide: N, section: <title
/// placeholder text or "">}`. Same bounds and whole-file answers as DOCX.
mod pptx {
    use super::*;
    use quick_xml::events::{BytesStart, Event};
    use std::collections::BTreeMap;

    enum Fault {
        /// A bound or policy breach: answers for the whole file, as DOCX does.
        File(ExtractionStatus, &'static str),
        /// A corrupt part: degrades to a `slide_N_<reason>` omission.
        Part(&'static str),
    }

    struct Slide {
        title: Vec<String>,
        paragraphs: Vec<String>,
        pictures: bool,
        embedded: bool,
    }

    pub(super) fn extract(bytes: &[u8], r: &mut ExtractionRecord) {
        let Ok(mut archive) = zip::ZipArchive::new(std::io::Cursor::new(bytes)) else {
            fail(r, ExtractionStatus::Failed, "malformed_pptx_zip");
            return;
        };
        if archive.len() > 2048 {
            fail(r, ExtractionStatus::Unsupported, "zip_entry_limit");
            return;
        }
        let mut declared = 0u64;
        // Lower-cased part name -> archive index (OOXML names are case-insensitive;
        // the first of duplicate names wins).
        let mut parts = BTreeMap::new();
        for i in 0..archive.len() {
            let Ok(entry) = archive.by_index(i) else {
                fail(
                    r,
                    ExtractionStatus::Unsupported,
                    "encrypted_or_invalid_zip_entry",
                );
                return;
            };
            declared = declared.saturating_add(entry.size());
            if declared > 32 * 1024 * 1024 {
                fail(r, ExtractionStatus::Unsupported, "zip_expanded_limit");
                return;
            }
            let name = entry.name().to_ascii_lowercase();
            if name.starts_with("ppt/notesslides/") && name.ends_with(".xml") {
                r.limits.push("notes_omitted".into());
            }
            parts.entry(name).or_insert(i);
        }
        let mut budget = 32 * 1024 * 1024u64;
        let mut read = |archive: &mut zip::ZipArchive<_>, index: usize| {
            let entry = archive.by_index(index).map_err(|_| {
                (
                    ExtractionStatus::Unsupported,
                    "encrypted_or_invalid_zip_entry",
                )
            })?;
            read_part(entry, &mut budget)
        };
        // Slide number (from the part name) -> archive index, numeric order.
        let slides: BTreeMap<u32, usize> = parts
            .iter()
            .filter_map(|(name, i)| Some((slide_number(name)?, *i)))
            .collect();
        let Some(&presentation) = parts.get("ppt/presentation.xml") else {
            fail(r, ExtractionStatus::Failed, "missing_presentation_xml");
            return;
        };
        let order = match read(&mut archive, presentation)
            .map_err(|(s, reason)| Fault::File(s, reason))
            .and_then(|xml| slide_ids(&xml))
        {
            Ok(ids) if !ids.is_empty() => {
                let targets = match parts.get("ppt/_rels/presentation.xml.rels") {
                    None => Ok(BTreeMap::new()),
                    Some(&rels) => read(&mut archive, rels)
                        .map_err(|(s, reason)| Fault::File(s, reason))
                        .and_then(|xml| relationships(&xml)),
                };
                match targets {
                    Ok(targets) => ids
                        .iter()
                        .enumerate()
                        .filter_map(|(position, id)| {
                            let n = slide_number(targets.get(id)?)?;
                            Some((position as u32 + 1, n))
                        })
                        .collect::<Vec<_>>(),
                    Err(fault) => return whole(r, fault),
                }
            }
            Ok(_) => vec![],
            Err(fault) => return whole(r, fault),
        };
        // Presentation order when `sldIdLst` resolves; numeric file order otherwise.
        let order: Vec<(u32, u32)> = if order.is_empty() {
            slides.keys().map(|n| (*n, *n)).collect()
        } else {
            if slides.keys().any(|n| !order.iter().any(|(_, m)| m == n)) {
                r.limits.push("unreferenced_slides_omitted".into());
            }
            let mut seen = std::collections::BTreeSet::new();
            order.into_iter().filter(|(_, n)| seen.insert(*n)).collect()
        };
        let mut slide_errors = false;
        for (position, n) in order {
            let Some(&index) = slides.get(&n) else {
                continue;
            };
            let xml = match read(&mut archive, index) {
                Ok(xml) => xml,
                Err((status, reason)) => {
                    fail(r, status, reason);
                    return;
                }
            };
            let slide = match parse(&xml) {
                Ok(slide) => slide,
                Err(Fault::File(status, reason)) => {
                    fail(r, status, reason);
                    return;
                }
                Err(Fault::Part(reason)) => {
                    slide_errors = true;
                    r.limits.push(format!("slide_{position}_{reason}"));
                    continue;
                }
            };
            if slide.embedded {
                r.limits.push("embedded_content_omitted".into());
            }
            let text = slide.paragraphs.join("\n");
            if text.trim().is_empty() {
                if slide.pictures {
                    r.limits
                        .push(format!("slide_{position}_image_only_ocr_deferred"));
                }
                continue;
            }
            if !add(
                r,
                text,
                SourceLocation {
                    page: None,
                    paragraph: None,
                    section: Some(slide.title.join(" ")),
                    slide: Some(position),
                },
            ) {
                break;
            }
        }
        r.limits.sort();
        r.limits.dedup();
        if r.segments.is_empty() && slide_errors {
            fail(r, ExtractionStatus::Failed, "all_slide_extraction_failed");
        } else {
            finish(r);
        }
    }

    fn whole(r: &mut ExtractionRecord, fault: Fault) {
        match fault {
            Fault::File(status, reason) => fail(r, status, reason),
            Fault::Part(_) => fail(r, ExtractionStatus::Failed, "malformed_document_xml"),
        }
    }

    /// `N` for a lower-cased `ppt/slides/slideN.xml` part name.
    fn slide_number(name: &str) -> Option<u32> {
        name.strip_prefix("ppt/slides/slide")?
            .strip_suffix(".xml")
            .filter(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))?
            .parse()
            .ok()
    }

    fn attribute(e: &BytesStart, prefixed: bool, local: &[u8]) -> Option<String> {
        e.attributes().flatten().find_map(|a| {
            (a.key.prefix().is_some() == prefixed && a.key.local_name().as_ref() == local)
                .then(|| String::from_utf8_lossy(&a.value).into_owned())
        })
    }

    /// Walks one bounded part with DOCX's whole-file answers for non-UTF-8,
    /// DOCTYPE and nesting depth. `visit` sees each event with its depth.
    fn walk(
        xml: &[u8],
        mut visit: impl FnMut(&Event, usize) -> std::result::Result<(), Fault>,
    ) -> std::result::Result<(), Fault> {
        if std::str::from_utf8(xml).is_err() {
            return Err(Fault::File(
                ExtractionStatus::Unsupported,
                "non_utf8_document_xml",
            ));
        }
        let mut reader = quick_xml::Reader::from_reader(xml);
        let mut depth = 0usize;
        let mut root = false;
        loop {
            let event = reader
                .read_event()
                .map_err(|_| Fault::Part("malformed_xml"))?;
            match &event {
                Event::Start(_) => {
                    depth += 1;
                    if depth > 128 {
                        return Err(Fault::File(ExtractionStatus::Failed, "xml_depth_limit"));
                    }
                    root = true;
                }
                Event::Empty(_) => root = true,
                Event::DocType(_) => {
                    return Err(Fault::File(
                        ExtractionStatus::Unsupported,
                        "xml_doctype_unsupported",
                    ))
                }
                Event::Eof => {
                    return if depth != 0 || !root {
                        Err(Fault::Part("malformed_xml"))
                    } else {
                        Ok(())
                    };
                }
                _ => {}
            }
            visit(&event, depth)?;
            if matches!(event, Event::End(_)) {
                depth = depth.saturating_sub(1);
            }
        }
    }

    /// The relationship ids of `p:sldIdLst`, in presentation order.
    fn slide_ids(xml: &[u8]) -> std::result::Result<Vec<String>, Fault> {
        let mut ids = vec![];
        walk(xml, |event, _| {
            if let Event::Start(e) | Event::Empty(e) = event {
                if e.local_name().as_ref() == b"sldId" {
                    ids.extend(attribute(e, true, b"id"));
                }
            }
            Ok(())
        })?;
        Ok(ids)
    }

    /// Internal relationship id -> lower-cased target part name, resolved
    /// against `ppt/`.
    fn relationships(xml: &[u8]) -> std::result::Result<BTreeMap<String, String>, Fault> {
        let mut targets = BTreeMap::new();
        walk(xml, |event, _| {
            if let Event::Start(e) | Event::Empty(e) = event {
                if e.local_name().as_ref() == b"Relationship"
                    && attribute(e, false, b"TargetMode").as_deref() != Some("External")
                {
                    if let (Some(id), Some(target)) =
                        (attribute(e, false, b"Id"), attribute(e, false, b"Target"))
                    {
                        let mut path: Vec<&str> = if target.starts_with('/') {
                            vec![]
                        } else {
                            vec!["ppt"]
                        };
                        for segment in target.split('/') {
                            match segment {
                                "" | "." => {}
                                ".." => {
                                    path.pop();
                                }
                                s => path.push(s),
                            }
                        }
                        targets.insert(id, path.join("/").to_ascii_lowercase());
                    }
                }
            }
            Ok(())
        })?;
        Ok(targets)
    }

    /// Parses one bounded slide part. Runs (`a:r`) inside a paragraph (`a:p`)
    /// concatenate verbatim; `a:br` becomes a newline. Table cells and grouped
    /// shapes contribute their paragraphs; chart, SmartArt and OLE frames are
    /// flagged as omitted embedded content.
    fn parse(xml: &[u8]) -> std::result::Result<Slide, Fault> {
        let mut slide = Slide {
            title: vec![],
            paragraphs: vec![],
            pictures: false,
            embedded: false,
        };
        let mut root = false;
        let mut text = String::new();
        let mut in_paragraph = false;
        let mut in_text = false;
        // Depth of the enclosing `p:sp`, and whether its placeholder is a title.
        let mut shape: Option<usize> = None;
        let mut title_shape = false;
        walk(xml, |event, depth| {
            match event {
                Event::Start(e) | Event::Empty(e) => {
                    let start = matches!(event, Event::Start(_));
                    match e.local_name().as_ref() {
                        b"sld" if depth == 1 && start => root = true,
                        b"sp" if start => {
                            shape = Some(depth);
                            title_shape = false;
                        }
                        b"pic" => slide.pictures = true,
                        b"p" if start => {
                            in_paragraph = true;
                            text.clear();
                        }
                        b"t" if start && in_paragraph => in_text = true,
                        b"br" if !start && in_paragraph => text.push('\n'),
                        b"ph" if shape.is_some() => {
                            if matches!(
                                attribute(e, false, b"type").as_deref(),
                                Some("title" | "ctrTitle")
                            ) {
                                title_shape = true;
                            }
                        }
                        b"graphicData" => {
                            let uri = attribute(e, false, b"uri").unwrap_or_default();
                            if ["/chart", "/chartex", "/diagram", "/ole"]
                                .iter()
                                .any(|kind| uri.ends_with(kind))
                            {
                                slide.embedded = true;
                            }
                        }
                        b"oleObj" => slide.embedded = true,
                        _ => {}
                    }
                }
                Event::Text(_) | Event::CData(_) | Event::GeneralRef(_) if in_text => {
                    xml_text(event, &mut text).map_err(Fault::Part)?;
                }
                Event::End(e) => match e.local_name().as_ref() {
                    b"t" => in_text = false,
                    b"p" if in_paragraph => {
                        in_paragraph = false;
                        let paragraph = std::mem::take(&mut text);
                        if !paragraph.trim().is_empty() {
                            if title_shape {
                                slide.title.push(paragraph.trim().to_owned());
                            }
                            slide.paragraphs.push(paragraph);
                        }
                    }
                    b"sp" if shape == Some(depth) => {
                        shape = None;
                        title_shape = false;
                    }
                    _ => {}
                },
                _ => {}
            }
            Ok(())
        })?;
        if !root {
            return Err(Fault::Part("malformed_xml"));
        }
        Ok(slide)
    }
}
/// Internal CLI subprocess entry. No authority, credentials, network or writes to
/// originals: reads one bounded blob and writes one disposable extraction result.
pub fn worker_main(args: &[String]) -> bool {
    if args.get(1).map(String::as_str) != Some("--internal-extract-worker") {
        return false;
    }
    if args.len() != 5 {
        std::process::exit(2);
    }
    let mut r = pending("", &args[2]);
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let file = fs::File::open(&args[3]).map_err(io)?;
        let mut bytes = Vec::new();
        file.take(INPUT_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(io)?;
        if bytes.len() as u64 > INPUT_LIMIT {
            fail(&mut r, ExtractionStatus::Unsupported, "input_limit");
        } else {
            match args[2].as_str() {
                "pdf" => pdf(&bytes, &mut r),
                "docx" => docx(&bytes, &mut r),
                "pptx" => pptx::extract(&bytes, &mut r),
                _ => fail(&mut r, ExtractionStatus::Unsupported, "unsupported_format"),
            }
        }
        Ok::<(), crate::ContentError>(())
    }));
    if !matches!(result, Ok(Ok(()))) {
        r.segments.clear();
        fail(&mut r, ExtractionStatus::Failed, "parser_failed");
    }
    let Ok(bytes) = serde_json::to_vec(&r) else {
        std::process::exit(2);
    };
    if bytes.len() as u64 > OUTPUT_LIMIT {
        std::process::exit(2);
    }
    let Ok(mut out) = fs::File::create(&args[4]) else {
        std::process::exit(2);
    };
    if out.write_all(&bytes).is_err() {
        std::process::exit(2);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(name: &str) -> Vec<u8> {
        fs::read(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/fixtures/extraction")
                .join(name),
        )
        .unwrap()
    }
    #[test]
    fn bounded_pdf_pages_multilingual_and_nontext_outcomes() {
        let mut r = pending("blob", "pdf");
        pdf(&fixture("mixed.pdf"), &mut r);
        assert_eq!(r.status, ExtractionStatus::Succeeded, "{r:?}");
        assert!(r.segments.iter().any(|s| s.text.contains("поиск")), "{r:?}");
        assert!(r.segments.iter().all(|s| s.location.page.is_some()));
        let mut r = pending("blob", "pdf");
        pdf(&fixture("partial.pdf"), &mut r);
        assert_eq!(r.status, ExtractionStatus::Partial, "{r:?}");
        let mut r = pending("blob", "pdf");
        pdf(&fixture("image-only.pdf"), &mut r);
        assert_eq!(r.status, ExtractionStatus::Unsupported);
        let mut r = pending("blob", "pdf");
        pdf(&fixture("encrypted.pdf"), &mut r);
        assert_eq!(r.status, ExtractionStatus::Unsupported, "{r:?}");
        let mut r = pending("blob", "pdf");
        pdf(&fixture("malformed.pdf"), &mut r);
        assert_eq!(r.status, ExtractionStatus::Failed);
    }
    #[test]
    fn docx_structure_and_bounded_failures() {
        let mut r = pending("blob", "docx");
        docx(&fixture("structured.docx"), &mut r);
        assert_eq!(r.status, ExtractionStatus::Succeeded, "{r:?}");
        assert!(r.segments.len() > 1, "{r:?}");
        assert!(r
            .segments
            .iter()
            .all(|s| s.location.paragraph.is_some() && s.location.section.is_some()));
        for (name, status) in [
            ("malformed.docx", ExtractionStatus::Failed),
            ("malformed-xml.docx", ExtractionStatus::Failed),
            ("doctype.docx", ExtractionStatus::Unsupported),
            ("bounded-expansion.docx", ExtractionStatus::Unsupported),
            ("empty.docx", ExtractionStatus::Unsupported),
        ] {
            let mut r = pending("blob", "docx");
            docx(&fixture(name), &mut r);
            assert_eq!(r.status, status, "{name}: {r:?}");
        }
    }
    #[test]
    fn encrypted_empty_password_is_unsupported_after_automatic_decryption() {
        let mut doc = lopdf::Document::load_mem(&fixture("mixed.pdf")).unwrap();
        doc.trailer.set(
            "ID",
            lopdf::Object::Array(vec![
                lopdf::Object::string_literal("synthetic-fixture-id"),
                lopdf::Object::string_literal("synthetic-fixture-id"),
            ]),
        );
        let state = lopdf::EncryptionState::try_from(lopdf::EncryptionVersion::V1 {
            document: &doc,
            owner_password: "synthetic-owner",
            user_password: "",
            permissions: lopdf::Permissions::all(),
        })
        .unwrap();
        doc.encrypt(&state).unwrap();
        let mut bytes = Vec::new();
        doc.save_to(&mut bytes).unwrap();
        let loaded = lopdf::Document::load_mem(&bytes).unwrap();
        assert!(loaded.encryption_state.is_some());
        let mut record = pending("blob", "pdf");
        pdf(&bytes, &mut record);
        assert_eq!(record.status, ExtractionStatus::Unsupported);
        assert_eq!(record.limits, vec!["encrypted_pdf"]);
    }
    #[test]
    fn docx_cdata_entities_empty_paragraphs_and_sections_remain_resolvable() {
        let xml = r#"<w:document xmlns:w="word"><w:body><w:p/><w:p><w:pPr><w:sectPr/></w:pPr><w:r><w:t><![CDATA[CDATA <text>]]>&amp; entity</w:t></w:r></w:p><w:p><w:r><w:t>next section</w:t></w:r></w:p></w:body></w:document>"#;
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("word/document.xml", zip::write::FileOptions::default())
            .unwrap();
        zip.write_all(xml.as_bytes()).unwrap();
        let bytes = zip.finish().unwrap().into_inner();
        let mut r = pending("blob", "docx");
        docx(&bytes, &mut r);
        assert_eq!(r.status, ExtractionStatus::Succeeded, "{r:?}");
        assert_eq!(r.segments[0].text, "CDATA <text>& entity");
        assert_eq!(r.segments[0].location.paragraph, Some(2));
        assert_eq!(r.segments[0].location.section.as_deref(), Some("1"));
        assert_eq!(r.segments[1].location.paragraph, Some(3));
        assert_eq!(r.segments[1].location.section.as_deref(), Some("2"));
    }
    #[test]
    fn pptx_image_only_and_malformed_slides_are_explicit() {
        let pptx = |parts: &[(&str, &str)]| {
            let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            for (name, body) in parts {
                zip.start_file(*name, zip::write::FileOptions::default())
                    .unwrap();
                zip.write_all(body.as_bytes()).unwrap();
            }
            let mut r = pending("blob", "pptx");
            pptx::extract(&zip.finish().unwrap().into_inner(), &mut r);
            r
        };
        let text = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree><p:sp><p:txBody><a:p><a:r><a:t>one</a:t></a:r><a:br/><a:r><a:t>two</a:t></a:r></a:p></p:txBody></p:sp></p:spTree></p:cSld></p:sld>"#;
        let picture = r#"<p:sld xmlns:a="a" xmlns:p="p"><p:cSld><p:spTree><p:pic><p:nvPicPr><p:cNvPr id="4" name="Picture"/></p:nvPicPr></p:pic></p:spTree></p:cSld></p:sld>"#;
        let r = pptx(&[
            ("ppt/presentation.xml", "<p:presentation/>"),
            ("ppt/slides/slide1.xml", text),
            ("ppt/slides/slide2.xml", picture),
            ("ppt/slides/slide3.xml", "<p:sld><p:cSld></p:sld>"),
        ]);
        assert_eq!(r.status, ExtractionStatus::Partial, "{r:?}");
        assert_eq!(r.segments.len(), 1);
        assert_eq!(r.segments[0].text, "one\ntwo");
        assert_eq!(
            r.limits,
            vec!["slide_2_image_only_ocr_deferred", "slide_3_malformed_xml"]
        );
        // DOCTYPE and non-UTF-8 answer for the whole file with DOCX's reasons.
        let r = pptx(&[
            ("ppt/presentation.xml", "<p:presentation/>"),
            ("ppt/slides/slide1.xml", text),
            ("ppt/slides/slide2.xml", "<!DOCTYPE x><p:sld/>"),
        ]);
        assert_eq!(r.status, ExtractionStatus::Unsupported, "{r:?}");
        assert_eq!(r.limits, vec!["xml_doctype_unsupported"]);
        assert!(r.segments.is_empty());
        let mut zip = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        zip.start_file("ppt/presentation.xml", zip::write::FileOptions::default())
            .unwrap();
        zip.write_all(b"<p:presentation/>").unwrap();
        zip.start_file("ppt/slides/slide1.xml", zip::write::FileOptions::default())
            .unwrap();
        zip.write_all(b"<p:sld>\xff</p:sld>").unwrap();
        let mut r = pending("blob", "pptx");
        pptx::extract(&zip.finish().unwrap().into_inner(), &mut r);
        assert_eq!(r.status, ExtractionStatus::Unsupported, "{r:?}");
        assert_eq!(r.limits, vec!["non_utf8_document_xml"]);
        let r = pptx(&[
            ("ppt/presentation.xml", "<p:presentation/>"),
            ("ppt/slides/slide1.xml", "<p:sld><p:cSld>"),
        ]);
        assert_eq!(r.status, ExtractionStatus::Failed, "{r:?}");
        assert!(r.limits.contains(&"all_slide_extraction_failed".into()));
        let r = pptx(&[("ppt/slides/slide1.xml", text)]);
        assert_eq!(r.limits, vec!["missing_presentation_xml"]);
        // Bound breaches answer `unsupported` with the DOCX reason.
        let big = format!("<p:sld>{}</p:sld>", " ".repeat(XML_LIMIT as usize));
        let r = pptx(&[
            ("ppt/presentation.xml", "<p:presentation/>"),
            ("ppt/slides/slide1.xml", text),
            ("ppt/slides/slide2.xml", &big),
        ]);
        assert_eq!(r.status, ExtractionStatus::Unsupported, "{r:?}");
        assert_eq!(r.limits, vec!["document_xml_limit"]);
        assert!(r.segments.is_empty());
    }
    #[test]
    fn pdf_page_decompression_failures_are_failed_or_partial() {
        let mut doc = lopdf::Document::load_mem(&fixture("mixed.pdf")).unwrap();
        let pages = doc.get_pages();
        for (index, page) in pages.values().enumerate() {
            for id in doc.get_page_contents(*page) {
                let stream = doc.get_object_mut(id).unwrap().as_stream_mut().unwrap();
                stream.content = vec![b' '; 9 * 1024 * 1024];
                stream.dict.remove(b"Filter");
                stream.compress().unwrap();
            }
            let mut bytes = Vec::new();
            doc.save_to(&mut bytes).unwrap();
            let mut r = pending("blob", "pdf");
            pdf(&bytes, &mut r);
            assert_eq!(
                r.status,
                if index == 0 {
                    ExtractionStatus::Partial
                } else {
                    ExtractionStatus::Failed
                },
                "{r:?}"
            );
        }
    }
}
