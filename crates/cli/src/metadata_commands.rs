//! Tags, aliases and headings over retained saved metadata.
use crate::search::Params;
use anyhow::{bail, Result};
use indexmap::IndexMap;
use mnemosyne_core::{cache::CachedMetadata, collation::NaturalCollator};
use mnemosyne_engine::{Engine, FileTarget};
use serde_json::json;
fn value<'a>(params: &'a Params, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .map(|value| value.as_deref().unwrap_or("true"))
}
fn truthy(params: &Params, key: &str) -> bool {
    value(params, key).is_some_and(|value| !value.is_empty())
}
fn target<'a>(params: &'a Params) -> FileTarget<'a> {
    FileTarget {
        file: value(params, "file"),
        path: value(params, "path"),
    }
}
fn strings(cache: &CachedMetadata, property: &str) -> Vec<String> {
    cache.frontmatter_strings(property)
}

fn aliases(cache: &CachedMetadata) -> Vec<String> {
    strings(cache, "aliases")
        .into_iter()
        .filter(|alias| !alias.is_empty())
        .collect()
}
fn tags(cache: &CachedMetadata) -> Vec<String> {
    strings(cache, "tags")
        .into_iter()
        .filter(|tag| !tag.is_empty() && !tag.contains(' '))
        .map(|tag| {
            if tag.starts_with('#') {
                tag
            } else {
                format!("#{tag}")
            }
        })
        .chain(cache.tags.iter().map(|tag| tag.tag.clone()))
        .collect()
}
fn global_tags(engine: &Engine) -> IndexMap<String, usize> {
    let mut exact = IndexMap::<String, usize>::new();
    for source in engine
        .cached_file_paths()
        .into_iter()
        .filter(|path| !engine.is_user_ignored(path))
    {
        let Some(cache) = engine.file_cache(source) else {
            continue;
        };
        for mut tag in tags(cache) {
            loop {
                if tag.ends_with('/') {
                    tag.pop();
                }
                if !mnemosyne_core::is_valid_tag(&tag) {
                    break;
                }
                *exact.entry(tag.clone()).or_default() += 1;
                let Some((parent, _)) = tag.rsplit_once('/') else {
                    break;
                };
                tag = parent.to_owned();
            }
        }
    }
    let mut folded = IndexMap::<String, (String, usize, usize)>::new();
    for (tag, count) in exact {
        let key = tag.to_lowercase();
        if let Some((display, total, max)) = folded.get_mut(&key) {
            *total += count;
            if count > *max {
                *max = count;
                *display = tag;
            }
        } else {
            folded.insert(key, (tag, count, count));
        }
    }
    folded
        .into_values()
        .map(|(display, total, _)| (display, total))
        .collect()
}
pub fn execute(engine: &Engine, params: &Params, command: &str) -> Result<String> {
    let collator = NaturalCollator::new(engine.collation_locale())?;
    match command {
        "tags" => {
            let mut rows: Vec<(String, usize)> =
                if truthy(params, "active") || truthy(params, "file") || truthy(params, "path") {
                    let path = engine.resolve_target(target(params), true)?;
                    let mut counts = IndexMap::<String, usize>::new();
                    if let Some(cache) = engine.file_cache(&path) {
                        for tag in tags(cache) {
                            *counts.entry(tag).or_default() += 1;
                        }
                    }
                    counts.into_iter().collect()
                } else {
                    global_tags(engine).into_iter().collect()
                };
            if truthy(params, "total") {
                return Ok(rows.len().to_string());
            }
            if rows.is_empty() {
                return Ok("No tags found.".into());
            }
            if value(params, "sort") == Some("count") {
                rows.sort_by(|left, right| right.1.cmp(&left.1))
            } else {
                rows.sort_by(|left, right| collator.compare(&left.0, &right.0))
            }
            let counts = truthy(params, "counts");
            let header = if counts {
                vec!["tag", "count"]
            } else {
                vec!["tag"]
            };
            Ok(crate::graph_commands::format_table(
                &header,
                rows.into_iter()
                    .map(|(tag, count)| {
                        if counts {
                            vec![tag, count.to_string()]
                        } else {
                            vec![tag]
                        }
                    })
                    .collect(),
                value(params, "format"),
            ))
        }
        "tag" => {
            let Some(name) = value(params, "name").filter(|name| !name.is_empty()) else {
                bail!("Missing required parameter: name\nUsage: tag name=<tag>")
            };
            let name = if name.starts_with('#') {
                name.to_owned()
            } else {
                format!("#{name}")
            };
            let counts = global_tags(engine);
            let Some(count) = counts.get(&name) else {
                bail!("Tag \"{name}\" not found.")
            };
            if truthy(params, "total") {
                return Ok(count.to_string());
            }
            let folded = name.to_lowercase();
            let mut files: Vec<_> = engine
                .ordered_note_paths()
                .into_iter()
                .filter(|path| {
                    engine.file_cache(path).is_some_and(|cache| {
                        tags(cache).iter().any(|tag| tag.to_lowercase() == folded)
                    })
                })
                .collect();
            files.sort_by(|left, right| collator.compare(left, right));
            let paths = files.join("\n");
            Ok(if truthy(params, "verbose") {
                format!("{name}\t{count}\n{paths}")
            } else {
                paths
            })
        }
        "aliases" => {
            if truthy(params, "active") || truthy(params, "file") || truthy(params, "path") {
                let path = engine.resolve_target(target(params), true)?;
                let mut values = engine.file_cache(&path).map(aliases).unwrap_or_default();
                if truthy(params, "total") {
                    return Ok(values.len().to_string());
                }
                if values.is_empty() {
                    return Ok("No aliases found.".into());
                }
                values.sort_by(|left, right| collator.compare(left, right));
                return Ok(values.join("\n"));
            }
            let mut values = IndexMap::<String, Vec<&str>>::new();
            for path in engine.ordered_note_paths() {
                if let Some(cache) = engine.file_cache(path) {
                    for alias in aliases(cache) {
                        values.entry(alias).or_default().push(path);
                    }
                }
            }
            if truthy(params, "total") {
                return Ok(values.len().to_string());
            }
            if values.is_empty() {
                return Ok("No aliases found.".into());
            }
            let mut values: Vec<_> = values.into_iter().collect();
            values.sort_by(|left, right| collator.compare(&left.0, &right.0));
            Ok(values
                .into_iter()
                .map(|(alias, paths)| {
                    if truthy(params, "verbose") {
                        format!("{alias}\t{}", paths.join(", "))
                    } else {
                        alias
                    }
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        "outline" => {
            let path = engine.resolve_target(target(params), true)?;
            if mnemosyne_core::extension(mnemosyne_core::basename(&path)) != "md" {
                bail!("File is not a markdown file.")
            }
            let headings = engine
                .file_cache(&path)
                .map(|cache| cache.headings.as_slice())
                .unwrap_or_default();
            if headings.is_empty() {
                return Ok("No headings found.".into());
            }
            if truthy(params, "total") {
                return Ok(headings.len().to_string());
            }
            if value(params, "format") == Some("json") {
                return Ok(serde_json::to_string_pretty(&headings.iter().map(|heading|json!({"level":heading.level,"heading":heading.heading,"line":heading.position.start.line+1})).collect::<Vec<_>>())?);
            }
            if value(params, "format") == Some("md") {
                return Ok(headings
                    .iter()
                    .map(|heading| {
                        format!("{} {}", "#".repeat(heading.level as usize), heading.heading)
                    })
                    .collect::<Vec<_>>()
                    .join("\n"));
            }
            // Build parent indices using the observed nearest lower-level stack.
            let mut children = vec![Vec::<usize>::new(); headings.len() + 1];
            let mut stack = Vec::<usize>::new();
            for (index, heading) in headings.iter().enumerate() {
                while stack
                    .last()
                    .is_some_and(|parent| headings[*parent].level >= heading.level)
                {
                    stack.pop();
                }
                let parent = stack.last().copied().unwrap_or(headings.len());
                children[parent].push(index);
                stack.push(index);
            }
            fn emit(
                parent: usize,
                prefix: &str,
                children: &[Vec<usize>],
                headings: &[mnemosyne_core::cache::HeadingCache],
                out: &mut Vec<String>,
            ) {
                for (position, index) in children[parent].iter().copied().enumerate() {
                    let last = position + 1 == children[parent].len();
                    out.push(format!(
                        "{prefix}{}{}",
                        if last { "└── " } else { "├── " },
                        headings[index].heading
                    ));
                    emit(
                        index,
                        &format!("{prefix}{}", if last { "    " } else { "│   " }),
                        children,
                        headings,
                        out,
                    );
                }
            }
            let mut out = Vec::new();
            emit(headings.len(), "", &children, headings, &mut out);
            Ok(out.join("\n"))
        }
        _ => bail!("unknown metadata command: {command}"),
    }
}
