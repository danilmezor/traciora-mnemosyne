//! Saved-file append/prepend operations. The desktop adapter processes raw
//! UTF-8 text, so BOMs and CRLFs remain part of the transform input.
use crate::search::Params;
use mnemosyne_engine::{Engine, FileTarget};

fn value<'a>(params: &'a Params, key: &str) -> Option<&'a str> {
    params.get(key).and_then(|v| v.as_deref())
}

pub fn execute(engine: &mut Engine, params: &Params, prepend: bool) -> anyhow::Result<String> {
    let command = if prepend { "prepend" } else { "append" };
    let usage = format!("Missing required parameter: content\nUsage: {command} [file=<name>] [path=<path>] content=<text> [inline]");
    // Generic required validation precedes the handler. Empty-but-present
    // content passes that validation and is rejected after target resolution.
    if !params.contains_key("content") {
        anyhow::bail!("Missing required parameter: content=<text>\nUsage: {command} [file=<name>] [path=<path>] content=<text> [inline]");
    }
    let path = engine.resolve_target(
        FileTarget {
            file: value(params, "file"),
            path: value(params, "path"),
        },
        true,
    )?;
    let content = value(params, "content").unwrap_or("true");
    if content.is_empty() {
        anyhow::bail!("{usage}");
    }
    let content = content.replace("\\n", "\n").replace("\\t", "\t");
    let inline = params
        .get("inline")
        .is_some_and(|v| v.as_ref().is_none_or(|s| !s.is_empty()));
    let absolute = engine.vault.root.join(&path);
    let saved = String::from_utf8_lossy(&std::fs::read(&absolute)?).into_owned();
    let separator = if inline { "" } else { "\n" };
    let result = if prepend {
        let start = content_start(&saved);
        format!("{}{content}{separator}{}", &saved[..start], &saved[start..])
    } else {
        format!("{saved}{separator}{content}")
    };
    // An unchanged transform does not touch the saved file or its timestamps.
    if result != saved {
        std::fs::write(&absolute, &result)?;
        engine.saved_content_modified(&path, &result)?;
        engine.refresh_vault()?;
        engine.file_modified(&path);
    }
    Ok(format!(
        "{} to: {path}",
        if prepend { "Prepended" } else { "Appended" }
    ))
}

fn content_start(source: &str) -> usize {
    let start = if source.starts_with("---\r\n") {
        5
    } else if source.starts_with("---\n") {
        4
    } else {
        return 0;
    };
    let mut cursor = start;
    while let Some(found) = source[cursor..].find("---") {
        let found = cursor + found;
        let end = found + 3;
        if source.as_bytes().get(found.wrapping_sub(1)) == Some(&b'\n') {
            if end == source.len() {
                return end;
            }
            if source[end..].starts_with("\r\n") {
                return end + 2;
            }
            if source[end..].starts_with('\n') {
                return end + 1;
            }
        }
        cursor = end;
    }
    0
}
