//! Pipe-output contract shared by the CLI and ordered engine requests.
use mnemosyne_core::{vault_search, Note};

pub type Params = mnemosyne_engine::command_args::Params;

fn value<'a>(params: &'a Params, key: &str) -> Option<&'a str> {
    params.get(key).and_then(|v| v.as_deref())
}

fn truthy(params: &Params, key: &str) -> bool {
    params
        .get(key)
        .is_some_and(|value| value.as_ref().is_none_or(|text| !text.is_empty()))
}

/// JavaScript parseInt's positive prefix determines a search limit. Zero,
/// negative values and NaN leave the reference result unlimited.
fn limit(text: Option<&str>) -> usize {
    let Some(text) = text else { return usize::MAX };
    let text = text.trim_start_matches(vault_search::js_whitespace);
    if text.starts_with('-') {
        return usize::MAX;
    }
    let text = text.strip_prefix('+').unwrap_or(text);
    let digits: String = text.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits
        .parse::<usize>()
        .ok()
        .filter(|n| *n > 0)
        .unwrap_or(usize::MAX)
}

pub fn response<'a>(
    notes: impl IntoIterator<Item = &'a Note>,
    params: &Params,
    context: bool,
) -> String {
    response_impl(params, context, |query, sensitive| {
        vault_search::search(notes, query, sensitive)
    })
}

pub fn response_cached<'a>(
    notes: impl IntoIterator<Item = (&'a Note, &'a mnemosyne_core::cache::CachedMetadata)>,
    params: &Params,
    context: bool,
) -> String {
    response_impl(params, context, |query, sensitive| {
        vault_search::search_cached(notes, query, sensitive)
    })
}

pub fn response_with_optional_cache<'a>(
    notes: impl IntoIterator<Item = (&'a Note, Option<&'a mnemosyne_core::cache::CachedMetadata>)>,
    params: &Params,
    context: bool,
) -> String {
    response_impl(params, context, |query, sensitive| {
        vault_search::search_with_optional_cache(notes, query, sensitive)
    })
}

pub fn response_in_realm<'a>(
    notes: impl IntoIterator<Item = (&'a Note, Option<&'a mnemosyne_core::cache::CachedMetadata>)>,
    realm: &vault_search::RegexCacheRealm,
    params: &Params,
    context: bool,
) -> String {
    response_impl(params, context, |query, sensitive| {
        vault_search::search_in_realm(notes, query, sensitive, realm)
    })
}

fn response_impl(
    params: &Params,
    context: bool,
    search: impl FnOnce(&str, bool) -> Result<Vec<vault_search::SearchHit>, vault_search::QueryError>,
) -> String {
    let command = if context { "search:context" } else { "search" };
    let Some(query) = value(params, "query") else {
        return format!("Error: Missing required parameter: query=<text>\nUsage: {command} query=<text> [path=<folder>] [limit=<n>] {}[case] [format=text|json]\n", if context { "" } else { "[total] " });
    };
    if query.is_empty() {
        return "Error: Missing required parameter: query\n".into();
    }
    let mut hits = match search(query, truthy(params, "case")) {
        Ok(hits) => hits,
        Err(error @ vault_search::QueryError::Exception { .. }) => return format!("{error}\n"),
        Err(error) => return format!("Error: {error}\n"),
    };
    if let Some(path) = value(params, "path").filter(|p| !p.is_empty()) {
        let prefix = if path.ends_with('/') {
            path.to_string()
        } else {
            format!("{path}/")
        };
        hits.retain(|hit| hit.file.starts_with(&prefix));
    }
    let json = value(params, "format") == Some("json");
    if !context && truthy(params, "total") {
        return if json {
            format!("{{\"total\":{}}}\n", hits.len())
        } else {
            format!("{}\n", hits.len())
        };
    }
    hits.truncate(limit(value(params, "limit")));
    if hits.is_empty() {
        return "No matches found.\n".into();
    }
    if json {
        let encoded = if context {
            serde_json::to_string(&hits).unwrap()
        } else {
            serde_json::to_string(&hits.iter().map(|h| &h.file).collect::<Vec<_>>()).unwrap()
        };
        return encoded + "\n";
    }
    let mut output = String::new();
    for hit in hits {
        if !context || hit.matches.is_empty() {
            output.push_str(&hit.file);
            output.push('\n');
        } else {
            for line in hit.matches {
                output.push_str(&format!("{}:{}: {}\n", hit.file, line.line, line.text));
            }
        }
    }
    output
}
