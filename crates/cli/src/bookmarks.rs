//! Bookmark CLI operations over retained core-plugin data.
use crate::search::Params;
use mnemosyne_engine::Engine;
use serde_json::{json, Value};
fn value<'a>(params: &'a Params, name: &str) -> Option<&'a str> {
    params
        .get(name)
        .and_then(|v| v.as_deref())
        .filter(|v| !v.is_empty())
}
fn flag(params: &Params, name: &str) -> bool {
    params
        .get(name)
        .is_some_and(|v| v.as_deref().is_none_or(|v| !v.is_empty()))
}
fn text<'a>(item: &'a Value, key: &str) -> &'a str {
    item.get(key).and_then(Value::as_str).unwrap_or("")
}
fn title(engine: &Engine, item: &Value) -> String {
    let explicit = text(item, "title");
    if !explicit.is_empty() {
        return explicit.into();
    }
    match text(item, "type") {
        "file" | "folder" => {
            let path = text(item, "path");
            let name = mnemosyne_core::basename(path);
            let name = if mnemosyne_core::extension(name) == "md" {
                mnemosyne_core::strip_extension(name)
            } else {
                name.to_owned()
            };
            let subpath = text(item, "subpath");
            if text(item, "type") == "file" && !subpath.is_empty() {
                return format!("{name}{subpath}");
            }
            if engine.ordered_folder_paths().contains(&path) {
                return mnemosyne_core::basename(path).to_owned();
            }
            name.into()
        }
        "search" => text(item, "query").into(),
        _ => String::new(),
    }
}
pub fn list(engine: &Engine, params: &Params) -> anyhow::Result<String> {
    let items = engine.bookmarks.flattened();
    if flag(params, "total") {
        return Ok(items.len().to_string());
    }
    if items.is_empty() {
        return Ok("No bookmarks found.".into());
    }
    let verbose = flag(params, "verbose");
    let rows = items
        .into_iter()
        .map(|item| {
            let kind = text(item, "type");
            let title = title(engine, item);
            let value = match kind {
                "file" => format!("{}{}", text(item, "path"), text(item, "subpath")),
                "folder" => text(item, "path").into(),
                "search" => text(item, "query").into(),
                "url" => text(item, "url").into(),
                _ => String::new(),
            };
            if verbose {
                vec![kind.into(), value, title]
            } else {
                vec![if value.is_empty() {
                    format!("[{kind}] {title}")
                } else {
                    value
                }]
            }
        })
        .collect();
    Ok(crate::graph_commands::format_table(
        if verbose {
            &["type", "value", "title"]
        } else {
            &["value"]
        },
        rows,
        value(params, "format"),
    ))
}
pub fn add(engine: &mut Engine, params: &Params) -> anyhow::Result<String> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)?
        .as_millis();
    add_at(engine, params, i64::try_from(now)?)
}
/// Explicit clock injection for pinned differential fixtures; no product env override.
pub fn add_at(engine: &mut Engine, params: &Params, now: i64) -> anyhow::Result<String> {
    let mut item = if let Some(path) = value(params, "file") {
        if engine.ordered_folder_paths().contains(&path) {
            json!({"type":"folder","ctime":now,"path":path})
        } else if engine.vault.has_file(path) {
            let mut item = json!({"type":"file","ctime":now,"path":path});
            if let Some(subpath) = value(params, "subpath") {
                item["subpath"] = subpath.into();
            }
            item
        } else {
            anyhow::bail!("File \"{path}\" not found.");
        }
    } else if let Some(path) = value(params, "folder") {
        if !engine.ordered_folder_paths().contains(&path) {
            anyhow::bail!("Folder \"{path}\" not found.");
        }
        json!({"type":"folder","ctime":now,"path":path})
    } else if let Some(query) = value(params, "search") {
        json!({"type":"search","ctime":now,"query":query})
    } else if let Some(url) = value(params, "url") {
        json!({"type":"url","ctime":now,"url":url,"title":value(params,"title").unwrap_or("")})
    } else {
        let path=engine.active_file().ok_or_else(||anyhow::anyhow!("No active file. Specify what to bookmark with: file=<path>, folder=<path>, search=<query>, or url=<url>"))?;
        json!({"type":"file","ctime":now,"path":path})
    };
    if text(&item, "type") != "url" {
        if let Some(title) = value(params, "title") {
            item["title"] = title.into();
        }
    }
    let output = match text(&item, "type") {
        "file" => format!(
            "Bookmarked: {}{}",
            text(&item, "path"),
            text(&item, "subpath")
        ),
        "folder" => format!("Bookmarked folder: {}", text(&item, "path")),
        "search" => format!("Bookmarked search: {}", text(&item, "query")),
        "url" => format!("Bookmarked URL: {}", text(&item, "url")),
        _ => "Bookmark added.".into(),
    };
    engine.bookmarks.add(item);
    Ok(output)
}
