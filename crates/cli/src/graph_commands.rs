//! Graph CLI handlers over saved metadata and filename destination lookup.
//! Frontmatter aliases describe notes; they do not resolve graph destinations.
use crate::search::Params;
use anyhow::{bail, Result};
use indexmap::IndexMap;
use mnemosyne_core::collation::NaturalCollator;
use mnemosyne_core::graph_refs::{link_path, references};
use mnemosyne_engine::{Engine, FileTarget};
use serde_json::{Map, Value};
use std::cmp::Ordering;

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
        path: value(params, "path"),
        file: value(params, "file"),
    }
}
fn unresolved_path(path: &str) -> &str {
    // FR checks the basename extension through the case-folding Ll helper.
    if mnemosyne_core::path::extension(mnemosyne_core::path::basename(path)) == "md" {
        &path[..path.len() - 3]
    } else {
        path
    }
}
#[derive(Default)]
struct Links {
    resolved: IndexMap<String, IndexMap<String, usize>>,
    unresolved: IndexMap<String, IndexMap<String, usize>>,
}
fn links(engine: &Engine, source_order: &[String]) -> Links {
    let mut result = Links::default();
    for source in source_order {
        let Some(cache) = engine.file_cache(source) else {
            continue;
        };
        let mut resolved = IndexMap::new();
        let mut unresolved = IndexMap::new();
        for reference in references(cache) {
            let path = link_path(reference);
            if let Some(destination) = engine.resolve_cached_link(source, path) {
                *resolved.entry(destination).or_insert(0) += 1;
            } else {
                *unresolved
                    .entry(unresolved_path(path).to_owned())
                    .or_insert(0) += 1;
            }
        }
        result.resolved.insert(source.clone(), resolved);
        result.unresolved.insert(source.clone(), unresolved);
    }
    result
}

/// Use the retained cache-source chronology and the independently selected
/// application collation locale. Neither is derived from Moment date settings.
pub fn execute(engine: &Engine, params: &Params, command: &str) -> Result<String> {
    let collator = NaturalCollator::new(engine.collation_locale())?;
    let source_order = engine
        .resolved_link_paths()
        .into_iter()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    execute_with_order(engine, params, command, &source_order, |a, b| {
        collator.compare(a, b)
    })
}

/// Handler text without native stdout framing. The source order must be the
/// metadata-cache resolution insertion chronology, not vault traversal order.
/// The comparator is the caller's natural filename collator (the shipped
/// `links` command independently uses JavaScript UTF-16 lexical ordering).
pub fn execute_with_order(
    engine: &Engine,
    params: &Params,
    command: &str,
    source_order: &[String],
    compare: impl Fn(&str, &str) -> Ordering,
) -> Result<String> {
    if command == "links" {
        let path = engine.resolve_target(target(params), true)?;
        if !engine.vault.notes.contains_key(&path) && engine.file_cache(&path).is_none() {
            return Ok(if truthy(params, "total") {
                "0"
            } else {
                "No links found."
            }
            .into());
        }
        let Some(cache) = engine.file_cache(&path) else {
            return Ok("No links found.".into());
        };
        let mut destinations = IndexMap::<String, ()>::new();
        for reference in references(cache) {
            let reference = link_path(reference);
            let destination = engine
                .resolve_cached_link(&path, reference)
                .unwrap_or_else(|| format!("{reference} (unresolved)"));
            destinations.insert(destination, ());
        }
        if truthy(params, "total") {
            return Ok(destinations.len().to_string());
        }
        if destinations.is_empty() {
            return Ok("No links found.".into());
        }
        let mut destinations: Vec<_> = destinations.into_keys().collect();
        destinations.sort_by(|a, b| a.encode_utf16().cmp(b.encode_utf16()));
        return Ok(destinations.join("\n"));
    }
    let graph = links(engine, source_order);
    match command {
        "backlinks" => {
            let target = engine.resolve_target(target(params), true)?;
            let mut incoming: Vec<_> = graph
                .resolved
                .iter()
                .filter_map(|(source, links)| {
                    links.get(&target).map(|count| (source.clone(), *count))
                })
                .collect();
            if truthy(params, "total") {
                return Ok(incoming
                    .iter()
                    .map(|(_, count)| count)
                    .sum::<usize>()
                    .to_string());
            }
            if incoming.is_empty() {
                return Ok("No backlinks found.".into());
            }
            incoming.sort_by(|a, b| compare(&a.0, &b.0));
            let counts = truthy(params, "counts");
            let header = if counts {
                vec!["file", "count"]
            } else {
                vec!["file"]
            };
            let rows = incoming
                .into_iter()
                .map(|(source, count)| {
                    if counts {
                        vec![source, count.to_string()]
                    } else {
                        vec![source]
                    }
                })
                .collect();
            Ok(format_table(&header, rows, value(params, "format")))
        }
        "unresolved" => {
            let mut aggregate: IndexMap<String, (usize, Vec<String>)> = IndexMap::new();
            for (source, links) in graph.unresolved {
                for (link, count) in links {
                    let entry = aggregate.entry(link).or_default();
                    entry.0 += count;
                    entry.1.push(source.clone());
                }
            }
            if truthy(params, "total") {
                return Ok(aggregate.len().to_string());
            }
            if aggregate.is_empty() {
                return Ok("No unresolved links found.".into());
            }
            let mut aggregate: Vec<_> = aggregate.into_iter().collect();
            aggregate.sort_by(|a, b| compare(&a.0, &b.0));
            let verbose = truthy(params, "verbose");
            let counts = truthy(params, "counts");
            let header = if verbose {
                vec!["link", "count", "sources"]
            } else if counts {
                vec!["link", "count"]
            } else {
                vec!["link"]
            };
            let rows = aggregate
                .into_iter()
                .map(|(link, (count, sources))| {
                    if verbose {
                        vec![link, count.to_string(), sources.join(", ")]
                    } else if counts {
                        vec![link, count.to_string()]
                    } else {
                        vec![link]
                    }
                })
                .collect();
            Ok(format_table(&header, rows, value(params, "format")))
        }
        "orphans" | "deadends" => {
            let mut paths: Vec<_> = engine
                .vault
                .files()
                .iter()
                .filter(|path| {
                    if command == "orphans" {
                        !graph
                            .resolved
                            .values()
                            .any(|links| links.contains_key(*path))
                    } else {
                        graph.resolved.get(*path).is_none_or(IndexMap::is_empty)
                    }
                })
                .cloned()
                .collect();
            if truthy(params, "total") {
                return Ok(paths.len().to_string());
            }
            if paths.is_empty() {
                return Ok(if command == "orphans" {
                    "No orphan files found."
                } else {
                    "No dead-end files found."
                }
                .into());
            }
            paths.sort_by(|a, b| compare(a, b));
            Ok(paths.join("\n"))
        }
        _ => bail!("Unsupported graph command: {command}"),
    }
}
pub(crate) fn format_table(
    header: &[&str],
    rows: Vec<Vec<String>>,
    format: Option<&str>,
) -> String {
    if format == Some("json") {
        let rows: Vec<_> = rows
            .into_iter()
            .map(|row| {
                let mut object = Map::new();
                for (index, key) in header.iter().enumerate() {
                    object.insert(
                        (*key).into(),
                        Value::String(row.get(index).cloned().unwrap_or_default()),
                    );
                }
                Value::Object(object)
            })
            .collect();
        return serde_json::to_string_pretty(&rows).unwrap();
    }
    let delimiter = if format == Some("csv") { ',' } else { '\t' };
    rows.into_iter()
        .map(|row| {
            row.into_iter()
                .map(|field| {
                    if field.contains(['"', '\r', '\n']) || field.contains(delimiter) {
                        format!("\"{}\"", field.replace('"', "\"\""))
                    } else {
                        field
                    }
                })
                .collect::<Vec<_>>()
                .join(&delimiter.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n")
}
