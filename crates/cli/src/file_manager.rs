//! CLI naming/target rules over the shared link-preserving vault rename.
use crate::search::Params;
use anyhow::{bail, Result};
use mnemosyne_core::{basename, extension, normalize_path, parent, VaultError};
use mnemosyne_engine::{file_operations::FileOperationId, Engine, EngineError, FileTarget};

fn value<'a>(p: &'a Params, key: &str) -> Option<&'a str> {
    p.get(key).map(|v| v.as_deref().unwrap_or("true"))
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
// The desktop adapter joins paths through Node's path.join before touching the
// filesystem. Vault names themselves preserve these components until its
// watcher reconciles the physical path.
fn adapter_relative(path: &str) -> String {
    let mut parts = Vec::new();
    for component in path.trim_start_matches('/').split('/') {
        match component {
            "" | "." => {}
            ".." if parts.last().is_some_and(|part| *part != "..") => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts.join("/")
}
fn adapter_absolute(engine: &Engine, path: &str) -> std::path::PathBuf {
    let joined = engine.vault.root.join(path.trim_start_matches('/'));
    let mut result = std::path::PathBuf::new();
    for component in joined.components() {
        match component {
            std::path::Component::ParentDir => {
                result.pop();
            }
            std::path::Component::CurDir => {}
            component => result.push(component.as_os_str()),
        }
    }
    result
}
fn canonical_parent(engine: &Engine, path: &str) -> String {
    let mut actual = String::new();
    let destination_parent = parent(path);
    if destination_parent.is_empty() || destination_parent == "/" {
        return basename(path).to_owned();
    }
    for component in destination_parent.split('/') {
        let Some(entry) = std::fs::read_dir(engine.vault.root.join(&actual))
            .ok()
            .and_then(|entries| {
                entries.flatten().find(|entry| {
                    let name = entry.file_name().to_string_lossy().into_owned();
                    !name.starts_with('.')
                        && entry.path().is_dir()
                        && name.to_lowercase() == component.to_lowercase()
                })
            })
        else {
            return path.to_owned();
        };
        if !actual.is_empty() {
            actual.push('/');
        }
        actual.push_str(&entry.file_name().to_string_lossy());
    }
    format!("{actual}/{}", basename(path))
}
fn rename_error(error: VaultError, engine: &Engine, old: &str, new: &str) -> anyhow::Error {
    match error {
        VaultError::Exists(_) => anyhow::anyhow!("Destination file already exists!"),
        VaultError::Io(_, error) => {
            let (code, message) = match error.raw_os_error() {
                Some(2) => ("ENOENT", "no such file or directory"),
                Some(20) => ("ENOTDIR", "not a directory"),
                Some(13) => ("EACCES", "permission denied"),
                Some(30) => ("EROFS", "read-only file system"),
                _ => return error.into(),
            };
            anyhow::anyhow!(
                "{code}: {message}, rename '{}' -> '{}'",
                adapter_absolute(engine, old).display(),
                adapter_absolute(engine, new).display()
            )
        }
        error => error.into(),
    }
}

/// `moving=false` executes rename; `moving=true` executes move.
pub struct PendingRename {
    pub id: FileOperationId,
    old: String,
    raw_destination: String,
    physical_destination: String,
    moving: bool,
}
impl PendingRename {
    pub fn complete(&self, engine: &Engine, result: Result<(), EngineError>) -> Result<String> {
        result.map_err(|error| match error {
            EngineError::Vault(error) => {
                rename_error(error, engine, &self.old, &self.physical_destination)
            }
            error => error.into(),
        })?;
        Ok(format!(
            "{}: {} -> {}",
            if self.moving { "Moved" } else { "Renamed" },
            self.old,
            self.raw_destination
        ))
    }
}

pub fn begin(engine: &mut Engine, p: &Params, moving: bool) -> Result<PendingRename> {
    let key = if moving { "to" } else { "name" };
    let Some(argument) = value(p, key) else {
        if moving {
            bail!("Missing required parameter: to=<path>\nUsage: move [file=<name>] [path=<path>] to=<path>")
        }
        bail!("Missing required parameter: name=<name>\nUsage: rename [file=<name>] [path=<path>] name=<name>")
    };
    if argument.is_empty() {
        if moving {
            bail!("Missing required parameter: to\nUsage: move to=<folder> or to=<path>")
        }
        bail!("Missing required parameter: name\nUsage: rename name=<new name>")
    }
    let old = engine.resolve_target(
        FileTarget {
            file: value(p, "file"),
            path: value(p, "path"),
        },
        true,
    )?;
    let raw_destination = if moving {
        let to = argument.trim_matches('/');
        if to.contains('.') {
            to.to_owned()
        } else if to.is_empty() {
            basename(&old).to_owned()
        } else {
            format!("{to}/{}", basename(&old))
        }
    } else {
        let mut name = argument.to_owned();
        let ext = extension(&old);
        if !name.contains('.') && !ext.is_empty() {
            name.push('.');
            name.push_str(&ext);
        }
        let directory = parent(&old).trim_matches('/');
        if directory.is_empty() {
            name
        } else {
            format!("{directory}/{name}")
        }
    };
    let normalized_destination = normalized(&raw_destination);
    valid_path(&normalized_destination)?;
    let destination = canonical_parent(engine, &normalized_destination);
    let traversing = destination
        .split('/')
        .any(|part| part == "." || part == "..");
    let physical_destination = if traversing {
        adapter_relative(&destination)
    } else {
        destination.clone()
    };
    // The adapter tests destination existence using joined physical paths,
    // but its same-file exception compares the uncollapsed vault names.
    if traversing
        && adapter_absolute(engine, &physical_destination).exists()
        && old.to_lowercase() != destination.to_lowercase()
    {
        bail!("Destination file already exists!");
    }
    let id = engine.begin_file_rename(&old, &destination, &physical_destination);
    Ok(PendingRename {
        id,
        old,
        raw_destination,
        physical_destination,
        moving,
    })
}

/// Blocking convenience for non-event-loop callers. Pending decisions remain
/// owned by the engine; retained/interactive callers use begin plus completion.
pub fn execute(engine: &mut Engine, p: &Params, moving: bool) -> Result<String> {
    let pending = begin(engine, p, moving)?;
    loop {
        let _ = engine.poll_metadata_lifecycle();
        engine.poll_file_operations();
        if let Some(result) = engine.take_file_operation_result(pending.id) {
            return pending.complete(engine, result);
        }
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}
