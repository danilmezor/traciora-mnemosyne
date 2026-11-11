//! Read-only vault/folder listings over the retained engine tree.
//! Stat refresh timing and cold child insertion order remain engine work.
use crate::search::Params;
use mnemosyne_engine::Engine;
fn value<'a>(params: &'a Params, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .and_then(|value| value.as_deref())
        .filter(|value| !value.is_empty())
}
fn flag(params: &Params, key: &str) -> bool {
    params
        .get(key)
        .is_some_and(|value| value.as_ref().is_none_or(|value| !value.is_empty()))
}
fn within(path: &str, folder: &str) -> bool {
    folder == "/"
        || path
            .strip_prefix(folder)
            .is_some_and(|suffix| suffix.starts_with('/'))
}
fn size(engine: &Engine, files: &[&str]) -> anyhow::Result<String> {
    let mut total = 0.0;
    for path in files {
        total += std::fs::metadata(engine.vault.root.join(path))?.len() as f64;
    }
    Ok(mnemosyne_core::yaml::javascript_string(
        &serde_yaml::to_value(total)?,
    ))
}
pub fn execute(engine: &Engine, params: &Params, command: &str) -> anyhow::Result<String> {
    let files = engine.ordered_file_paths();
    let folders = engine.ordered_folder_paths();
    let sort = |mut paths: Vec<&str>| -> anyhow::Result<String> {
        let collator = mnemosyne_core::collation::NaturalCollator::new(engine.collation_locale())?;
        paths.sort_by(|left, right| collator.compare(left, right));
        Ok(paths.join("\n"))
    };
    match command {
        "files" => {
            let prefix = value(params, "folder").map(|folder| {
                if folder.ends_with('/') {
                    folder.to_owned()
                } else {
                    format!("{folder}/")
                }
            });
            let extension = value(params, "ext")
                .map(|extension| extension.strip_prefix('.').unwrap_or(extension));
            let paths: Vec<_> = files
                .into_iter()
                .filter(|path| {
                    prefix
                        .as_deref()
                        .is_none_or(|prefix| path.starts_with(prefix))
                        && extension.is_none_or(|extension| {
                            mnemosyne_core::extension(mnemosyne_core::basename(path)) == extension
                        })
                })
                .collect();
            if flag(params, "total") {
                Ok(paths.len().to_string())
            } else {
                sort(paths)
            }
        }
        "folders" => {
            let folder = value(params, "folder").unwrap_or("/");
            if !folders.contains(&folder) {
                anyhow::bail!("Folder \"{folder}\" not found.");
            }
            let paths: Vec<_> = folders
                .into_iter()
                .filter(|path| *path == folder || within(path, folder))
                .collect();
            if flag(params, "total") {
                Ok(paths.len().to_string())
            } else {
                sort(paths)
            }
        }
        "folder" => {
            let Some(folder) = value(params, "path") else {
                anyhow::bail!("Missing required parameter: path\nUsage: folder path=<folder-path> [info=files|folders|size]");
            };
            if files.contains(&folder) {
                anyhow::bail!("\"{folder}\" is a file, not a folder.");
            }
            if !folders.contains(&folder) {
                anyhow::bail!("Folder \"{folder}\" not found.");
            }
            let paths: Vec<_> = files
                .into_iter()
                .filter(|path| within(path, folder))
                .collect();
            let folder_count = folders
                .into_iter()
                .filter(|path| *path != folder && within(path, folder))
                .count();
            match value(params, "info") {
                Some("files") => Ok(paths.len().to_string()),
                Some("folders") => Ok(folder_count.to_string()),
                Some("size") => size(engine, &paths),
                _ => Ok(format!(
                    "path\t{folder}\nfiles\t{}\nfolders\t{folder_count}\nsize\t{}",
                    paths.len(),
                    size(engine, &paths)?
                )),
            }
        }
        "vault" => {
            let name = engine
                .vault
                .root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            let path = engine.vault.root.to_string_lossy();
            match value(params, "info") {
                Some("name") => Ok(name.into_owned()),
                Some("path") => Ok(path.into_owned()),
                Some("files") => Ok(files.len().to_string()),
                Some("folders") => Ok(folders.len().saturating_sub(1).to_string()),
                Some("size") => size(engine, &files),
                _ => Ok(format!(
                    "name\t{name}\npath\t{path}\nfiles\t{}\nfolders\t{}\nsize\t{}",
                    files.len(),
                    folders.len().saturating_sub(1),
                    size(engine, &files)?
                )),
            }
        }
        _ => anyhow::bail!("unknown inventory command: {command}"),
    }
}
