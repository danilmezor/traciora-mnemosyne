//! Native CLI deletion routing: permanent deletion or system trash with the
//! captured local fallback. Application UI trash preferences are not consulted.
use crate::search::Params;
use anyhow::Result;
use mnemosyne_engine::{Engine, FileTarget};
use std::path::Path;

fn value<'a>(params: &'a Params, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .map(|value| value.as_deref().unwrap_or("true"))
}

pub fn execute(engine: &mut Engine, params: &Params) -> Result<String> {
    execute_with_trash(engine, params, system_trash)
}

/// The adapter boundary supports the independently captured forced-false
/// fallback fixtures; these tests do not certify real platform trash routing.
pub fn execute_with_trash(
    engine: &mut Engine,
    params: &Params,
    adapter: impl FnOnce(&Path) -> bool,
) -> Result<String> {
    let path = engine.resolve_target(
        FileTarget {
            path: value(params, "path"),
            file: value(params, "file"),
        },
        true,
    )?;
    let permanent = value(params, "permanent").is_some_and(|value| !value.is_empty());
    if permanent {
        engine.vault.delete(&path)?;
    } else {
        engine.vault.trash_with_system(&path, adapter)?;
    }
    engine.file_deleted(&path);
    engine.persist_session()?;
    Ok(format!(
        "{}: {path}",
        if permanent {
            "Deleted permanently"
        } else {
            "Moved to trash"
        }
    ))
}

/// Complete one OS trash move, reporting false for local fallback on failure.
/// macOS uses the same Foundation API as the pinned Electron platform source.
#[cfg(target_os = "macos")]
pub fn system_trash(path: &Path) -> bool {
    objc2::rc::autoreleasepool(|_| {
        let Some(url) = objc2_foundation::NSURL::from_file_path(path) else {
            return false;
        };
        objc2_foundation::NSFileManager::defaultManager()
            .trashItemAtURL_resultingItemURL_error(&url, None)
            .is_ok()
    })
}

/// Normal trash integration on other platforms is still pending verification.
/// The existing local fallback remains available when no platform adapter exists.
#[cfg(not(target_os = "macos"))]
pub fn system_trash(_path: &Path) -> bool {
    false
}
