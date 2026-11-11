//! Shipped task/tasks handlers. Cache identifies list tasks; single-task updates
//! deliberately use the narrower source-line grammar of the reference CLI.
use crate::search::Params;
use anyhow::{bail, Result};
use mnemosyne_core::vault_search::{ecmascript_number, js_whitespace};
use mnemosyne_engine::{EditorPosition, Engine, EngineError, FileTarget};
use std::cmp::Ordering;

fn value<'a>(p: &'a Params, key: &str) -> Option<&'a str> {
    p.get(key).map(|v| v.as_deref().unwrap_or("true"))
}
fn truthy(p: &Params, key: &str) -> bool {
    value(p, key).is_some_and(|v| !v.is_empty())
}
fn target<'a>(p: &'a Params) -> FileTarget<'a> {
    FileTarget {
        path: value(p, "path"),
        file: value(p, "file"),
    }
}
fn trim(s: &str) -> &str {
    s.trim_matches(js_whitespace)
}
fn saved(engine: &Engine, path: &str) -> Result<String> {
    let text = engine.read_saved(FileTarget::path(path))?;
    Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_owned())
}

/// Returns handler text without the native pipe transport's final newline.
pub fn execute(engine: &mut Engine, params: &Params, single: bool) -> Result<String> {
    if single {
        task(engine, params)
    } else {
        tasks(engine, params)
    }
}

fn daily(engine: &mut Engine) -> Result<String> {
    Ok(engine.daily_note_path()?)
}

fn tasks(engine: &mut Engine, p: &Params) -> Result<String> {
    let paths: Vec<String> = if truthy(p, "daily") {
        vec![daily(engine)?]
    } else if ["active", "file", "path"].iter().any(|key| truthy(p, key)) {
        vec![engine.resolve_target(target(p), true)?]
    } else {
        engine
            .ordered_note_paths()
            .into_iter()
            .map(str::to_owned)
            .collect()
    };
    let mut rows: Vec<(String, usize, String, String)> = Vec::new();
    for path in paths {
        let Some(cache) = engine.file_cache(&path) else {
            continue;
        };
        let items: Vec<_> = cache
            .list_items
            .iter()
            .filter_map(|item| {
                item.task
                    .as_ref()
                    .map(|status| (item.position.start.line + 1, status.clone()))
            })
            .filter(|(_, status)| {
                if let Some(wanted) = value(p, "status") {
                    status == wanted
                } else if truthy(p, "done") {
                    status != " "
                } else if truthy(p, "todo") {
                    status == " "
                } else {
                    true
                }
            })
            .collect();
        if items.is_empty() {
            continue;
        }
        let source = saved(engine, &path)?;
        let lines: Vec<_> = source.split('\n').collect();
        for (line, status) in items {
            rows.push((
                path.clone(),
                line,
                status,
                lines
                    .get(line - 1)
                    .map(|s| trim(s))
                    .unwrap_or("")
                    .to_owned(),
            ));
        }
    }
    if truthy(p, "total") {
        return Ok(rows.len().to_string());
    }
    if rows.is_empty() {
        return Ok("No tasks found.".into());
    }
    rows.sort_by(|a, b| natural_path_cmp(&a.0, &b.0).then(a.1.cmp(&b.1)));
    if let Some(format) = value(p, "format").filter(|s| !s.is_empty()) {
        if format == "json" {
            let strings: Vec<_> = rows.iter().map(|(path,line,status,text)| {
                format!("  {{\n    \"status\": {},\n    \"text\": {},\n    \"file\": {},\n    \"line\": {}\n  }}", json(status),json(text),json(path),json(&line.to_string()))
            }).collect();
            return Ok(format!("[\n{}\n]", strings.join(",\n")));
        }
        let delimiter = if format == "csv" { ',' } else { '\t' };
        return Ok(rows
            .iter()
            .map(|(path, line, status, text)| {
                [status.as_str(), text, path, &line.to_string()]
                    .map(|s| quote_field(s, delimiter))
                    .join(&delimiter.to_string())
            })
            .collect::<Vec<_>>()
            .join("\n"));
    }
    if truthy(p, "verbose") {
        let mut out = Vec::new();
        let mut previous = "";
        for (path, line, _, text) in &rows {
            if path != previous {
                if !out.is_empty() {
                    out.push(String::new())
                }
                out.push(path.clone());
                previous = path;
            }
            out.push(format!("{line}\t{text}"));
        }
        return Ok(out.join("\n"));
    }
    Ok(rows
        .into_iter()
        .map(|row| row.3)
        .collect::<Vec<_>>()
        .join("\n"))
}
fn json(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}
fn quote_field(s: &str, delimiter: char) -> String {
    if s.contains(['"', '\r', '\n']) || s.contains(delimiter) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.into()
    }
}

// Matches the captured ASCII/numeric filenames. Full locale-sensitive
// Intl.Collator equivalence remains part of the shared ordering work.
fn natural_path_cmp(a: &str, b: &str) -> Ordering {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    let mut a = a.chars().peekable();
    let mut b = b.chars().peekable();
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut x = String::new();
                let mut y = String::new();
                while a.peek().is_some_and(|c| c.is_ascii_digit()) {
                    x.push(a.next().unwrap())
                }
                while b.peek().is_some_and(|c| c.is_ascii_digit()) {
                    y.push(b.next().unwrap())
                }
                let x = x.trim_start_matches('0');
                let y = y.trim_start_matches('0');
                let cmp = x.len().cmp(&y.len()).then(x.cmp(y));
                if cmp != Ordering::Equal {
                    return cmp;
                }
            }
            (Some(x), Some(y)) => {
                a.next();
                b.next();
                let cmp = x.cmp(&y);
                if cmp != Ordering::Equal {
                    return cmp;
                }
            }
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
        }
    }
}
fn line_number(s: &str) -> Option<f64> {
    let s = s.trim_start_matches(js_whitespace);
    if s.starts_with('-') {
        return None;
    }
    let s = s.strip_prefix('+').unwrap_or(s);
    let digits: String = s.chars().take_while(char::is_ascii_digit).collect();
    digits.parse::<f64>().ok().filter(|n| *n >= 1.0)
}

// The regex's status and suffix use JS dot (no line terminators), and its
// marker accepts '-'/'*'/decimal-dot but excludes '+' and decimal-parenthesis.
fn marker(line: &str) -> Option<(usize, char, usize)> {
    let mut pos = 0;
    while let Some(c) = line[pos..].chars().next().filter(|c| js_whitespace(*c)) {
        pos += c.len_utf8()
    }
    match line.as_bytes().get(pos)? {
        b'-' | b'*' => pos += 1,
        b'0'..=b'9' => {
            while line.as_bytes().get(pos).is_some_and(u8::is_ascii_digit) {
                pos += 1
            }
            if line.as_bytes().get(pos) != Some(&b'.') {
                return None;
            }
            pos += 1;
        }
        _ => return None,
    }
    while let Some(c) = line[pos..].chars().next().filter(|c| js_whitespace(*c)) {
        pos += c.len_utf8()
    }
    if line.as_bytes().get(pos) != Some(&b'[') {
        return None;
    }
    pos += 1;
    let status = line[pos..].chars().next()?;
    if status.len_utf16() != 1 || matches!(status, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
        return None;
    }
    let end = pos + status.len_utf8();
    if line.as_bytes().get(end) != Some(&b']')
        || line[end..].contains(['\n', '\r', '\u{2028}', '\u{2029}'])
    {
        return None;
    }
    Some((pos, status, end))
}

fn task(engine: &mut Engine, p: &Params) -> Result<String> {
    let (path, line) = if let Some(reference) = value(p, "ref").filter(|s| !s.is_empty()) {
        let Some((name, number)) = reference.rsplit_once(':') else {
            bail!("Invalid ref format. Expected path:line (e.g., \"Note.md:5\")")
        };
        let Some(line) = line_number(number) else {
            bail!("Invalid line number in ref.")
        };
        // getAbstractFileByPath wins even when it resolves a folder. Only an
        // absent exact entry permits the metadata name-resolution fallback.
        let path = match engine.resolve_target(FileTarget::path(name), false) {
            Ok(path) => path,
            Err(EngineError::Folder(_)) => bail!("File \"{name}\" not found."),
            Err(_) => engine
                .resolve_target(FileTarget::file(name), false)
                .map_err(|_| anyhow::anyhow!("File \"{name}\" not found."))?,
        };
        (path, line)
    } else {
        let Some(number) = value(p, "line").filter(|s| !s.is_empty()) else {
            bail!("Missing required parameter: line\nUsage: task ref=<path:line> or task daily line=<n> or task file=<name> line=<n>")
        };
        let Some(line) = line_number(number) else {
            bail!("Invalid line number.")
        };
        let path = if truthy(p, "daily") {
            daily(engine)?
        } else {
            engine.resolve_target(target(p), false)?
        };
        (path, line)
    };
    let text = saved(engine, &path)?;
    let mut lines: Vec<_> = text.split('\n').map(str::to_owned).collect();
    if line > lines.len() as f64 {
        bail!(
            "Line {} is out of range (file has {} lines).",
            ecmascript_number(line),
            lines.len()
        )
    }
    let line = line as usize;
    let old_line = &lines[line - 1];
    let Some((start, old, end)) = marker(old_line) else {
        bail!("Line {line} is not a task.")
    };
    let new = if let Some(status) = value(p, "status") {
        if status.encode_utf16().count() != 1 {
            bail!("Status must be a single character.")
        }
        Some(status.to_owned())
    } else if truthy(p, "done") {
        Some("x".into())
    } else if truthy(p, "todo") {
        Some(" ".into())
    } else if truthy(p, "toggle") {
        Some(if old == ' ' { "x" } else { " " }.into())
    } else {
        None
    };
    let Some(new) = new else {
        return Ok(format!(
            "file\t{path}\nline\t{line}\nstatus\t{old}\ntext\t{}",
            trim(old_line)
        ));
    };
    if new == old.to_string() {
        return Ok(format!("Task already has status [{old}]."));
    }
    let column = old_line[..start].encode_utf16().count();
    let replaced = format!("{}{}{}", &old_line[..start], new, &old_line[end..]);
    let output = format!("Updated: [{old}] → [{new}]\n{}", trim(&replaced));
    lines[line - 1] = replaced;
    let updated = lines.join("\n");
    if engine.vault.get(&path).is_some() {
        engine.vault.modify(&path, &updated)?
    } else {
        std::fs::write(engine.vault.root.join(&path), &updated)?
    }
    engine.file_modified(&path);
    engine.patch_editor_task_status(
        &path,
        EditorPosition {
            line: (line - 1) as i64,
            ch: column as i64,
        },
        &new,
    )?;
    Ok(output)
}
