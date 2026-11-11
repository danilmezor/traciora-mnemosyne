//! Source-backed create handler; templates use the shared application getter.
use crate::search::Params;
use anyhow::{bail, Result};
use mnemosyne_core::{
    basename, extension, has_hidden_ancestor, normalize_path, strip_extension,
    vault_search::js_whitespace,
};
use mnemosyne_engine::{Engine, FileTarget, OpenOptions};
use std::io::Write;

fn value<'a>(p: &'a Params, key: &str) -> Option<&'a str> {
    p.get(key).map(|v| v.as_deref().unwrap_or("true"))
}
fn truthy(p: &Params, key: &str) -> bool {
    value(p, key).is_some_and(|s| !s.is_empty())
}
fn normalized(path: &str) -> String {
    normalize_path(&path.replace('\\', "/"))
}
fn valid_path(path: &str) -> Result<()> {
    if path.contains(':') {
        bail!("File name cannot contain any of the following characters: \\\u{a0}/\u{a0}:")
    }
    Ok(())
}
fn indexed_entry(engine: &Engine, path: &str) -> bool {
    !has_hidden_ancestor(path)
        && (engine.vault.has_file(path) || engine.vault.root.join(path).is_dir())
}
// getOrCreateFolder uses the indexed, case-insensitive folder destination;
// preserve its stored spelling in the created file's returned path.
fn parent_case(engine: &Engine, parent: &str) -> Option<String> {
    if parent.is_empty() || parent == "/" {
        return Some(String::new());
    }
    let mut actual = String::new();
    for component in parent.split('/') {
        let directory = engine.vault.root.join(&actual);
        let entry = std::fs::read_dir(directory).ok()?.flatten().find(|entry| {
            let name = entry.file_name().to_string_lossy().into_owned();
            !name.starts_with('.')
                && entry.path().is_dir()
                && name.to_lowercase() == component.to_lowercase()
        })?;
        if !actual.is_empty() {
            actual.push('/');
        }
        actual.push_str(&entry.file_name().to_string_lossy());
    }
    Some(actual)
}
fn canonical_parent(engine: &Engine, path: &str) -> String {
    let parent = mnemosyne_core::parent(path);
    match parent_case(engine, parent) {
        Some(actual) if !actual.is_empty() => format!("{actual}/{}", basename(path)),
        Some(_) => basename(path).to_owned(),
        None => path.to_owned(),
    }
}
fn ensure_parent(engine: &Engine, path: &str) -> Result<String> {
    let parent = mnemosyne_core::parent(path);
    if parent_case(engine, parent).is_some() {
        return Ok(canonical_parent(engine, path));
    }
    let directory = engine.vault.root.join(parent);
    if directory.exists() {
        bail!("Folder already exists.");
    }
    std::fs::create_dir_all(&directory)?;
    if has_hidden_ancestor(parent) {
        return Err(crate::error::ScriptException {
            kind: "TypeError",
            message: "Cannot read properties of null (reading 'getParentPrefix')".into(),
        }
        .into());
    }
    Ok(path.to_owned())
}
fn occupied_insensitive(engine: &Engine, path: &str) -> bool {
    let name = basename(path);
    let parent = mnemosyne_core::parent(path);
    std::fs::read_dir(engine.vault.root.join(parent))
        .ok()
        .is_some_and(|entries| {
            entries.flatten().any(|entry| {
                let other = entry.file_name().to_string_lossy().into_owned();
                !other.starts_with('.') && other.to_lowercase() == name.to_lowercase()
            })
        })
}

/// Returns the native handler body; application dispatch adds pipe framing.
pub fn execute(engine: &mut Engine, p: &Params) -> Result<String> {
    let name = value(p, "name").filter(|s| !s.is_empty());
    let path = value(p, "path").filter(|s| !s.is_empty());
    if name.is_some_and(|s| s.contains('/')) {
        bail!("name cannot contain \"/\". Use path for a full file path.")
    }
    let raw = match (path, name) {
        (Some(path), Some(name)) => format!("{}/{name}", path.trim_end_matches('/')),
        (Some(path), _) => path.to_owned(),
        (_, Some(name)) => name.to_owned(),
        _ => "Untitled".into(),
    };
    let (stem, requested_ext) = match raw.rfind('.').filter(|pos| *pos > 0) {
        Some(pos) => (raw[..pos].to_owned(), raw[pos + 1..].to_owned()),
        None => (raw, "md".into()),
    };
    let content = if let Some(template) = value(p, "template").filter(|s| !s.is_empty()) {
        engine.render_named_template(template, basename(&stem))?
    } else {
        value(p, "content")
            .unwrap_or("")
            .replace("\\n", "\n")
            .replace("\\t", "\t")
    };
    let overwrite_path = normalized(&format!("{stem}.{requested_ext}"));
    valid_path(&overwrite_path)?;
    let existing = truthy(p, "overwrite") && indexed_entry(engine, &overwrite_path);
    let created_path = if existing && engine.vault.has_file(&overwrite_path) {
        if engine.vault.get(&overwrite_path).is_some() {
            engine.vault.modify(&overwrite_path, &content)?
        } else {
            std::fs::write(engine.vault.root.join(&overwrite_path), &content)?
        }
        engine.file_modified(&overwrite_path);
        overwrite_path
    } else {
        // createNewFile infers a falsy requested extension from the stem before
        // checking the retained creator registry.
        let inferred_ext = if requested_ext.is_empty() {
            let inferred = extension(&stem);
            if inferred.is_empty() {
                "md".into()
            } else {
                inferred
            }
        } else {
            requested_ext
        };
        let ext = if engine.can_create_file_with_ext(&inferred_ext) {
            inferred_ext.as_str()
        } else {
            "md"
        };
        let trimmed = stem.trim_matches(js_whitespace);
        let stripped = if extension(trimmed) == ext {
            strip_extension(trimmed)
        } else {
            trimmed.to_owned()
        };
        let stem = normalized(&stripped);
        let mut candidate = canonical_parent(engine, &normalized(&format!("{stem}.{ext}")));
        let mut suffix = 1;
        while occupied_insensitive(engine, &candidate) {
            candidate = canonical_parent(engine, &normalized(&format!("{stem} {suffix}.{ext}")));
            suffix += 1;
        }
        valid_path(&candidate)?;
        let candidate = ensure_parent(engine, &candidate)?;
        let destination = engine.vault.root.join(&candidate);
        let mut file = match std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&destination)
        {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                bail!("File already exists.")
            }
            Err(error) => return Err(error.into()),
        };
        file.write_all(content.as_bytes())?;
        drop(file);
        engine.refresh_vault()?;
        if !has_hidden_ancestor(&candidate) {
            engine.file_created(&candidate)
        }
        candidate
    };
    if !engine.vault.has_file(&created_path) {
        return Err(crate::error::ScriptException {
            kind: "TypeError",
            message: "Cannot read properties of null (reading 'path')".into(),
        }
        .into());
    }
    if truthy(p, "open") {
        engine.open_file(
            FileTarget::path(&created_path),
            OpenOptions {
                new_tab: truthy(p, "newtab"),
            },
        )?;
    }
    engine.persist_session()?;
    Ok(format!(
        "{}: {created_path}",
        if existing { "Overwrote" } else { "Created" }
    ))
}
