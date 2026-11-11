//! Read-only snapshots of actual retained engine state for differential probes.
//! This reports current authority, including unfinished behavior, without
//! importing oracle snapshots or reconstructing session/cache state from disk.
use crate::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Selection {
    #[serde(default)]
    pub effective_config_keys: Vec<String>,
    #[serde(default)]
    pub metadata_paths: Vec<String>,
    #[serde(default)]
    pub metadata_fields: Vec<String>,
    #[serde(default)]
    pub session_fields: Vec<String>,
    #[serde(default)]
    pub event_names: Vec<String>,
    #[serde(default)]
    pub after_cursor: u64,
    #[serde(default)]
    pub readiness_fields: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct Observation {
    pub observation: Map<String, Value>,
    pub unavailable: BTreeMap<String, String>,
    pub readiness: Value,
}

pub fn snapshot(engine: &Engine, selection: &Selection) -> Result<Observation, serde_json::Error> {
    let mut observation = Map::new();
    let mut unavailable = BTreeMap::new();
    if !selection.effective_config_keys.is_empty() {
        let config: Map<String, Value> = selection
            .effective_config_keys
            .iter()
            .map(|key| {
                let value = engine
                    .get_config_value(key)
                    .map(|value| json!({"present": true, "value": value}))
                    .unwrap_or_else(|| json!({"present": false}));
                (key.clone(), value)
            })
            .collect();
        observation.insert("config".into(), json!({"effective": config}));
    }
    if !selection.metadata_paths.is_empty() || !selection.metadata_fields.is_empty() {
        let default_fields = vec![
            "files".into(),
            "markdown_files".into(),
            "cached_files".into(),
            "cache".into(),
        ];
        let fields = if selection.metadata_fields.is_empty() {
            &default_fields
        } else {
            &selection.metadata_fields
        };
        let mut metadata = Map::new();
        let frame = engine
            .metadata_lifecycle_enabled()
            .then(|| engine.metadata_lifecycle_snapshot());
        for field in fields {
            let value = match field.as_str() {
                "files" => json!(engine.ordered_file_paths()),
                "saved_sources" => json!(engine
                    .vault
                    .notes
                    .iter()
                    .map(|(path, note)| (path, &note.source))
                    .collect::<BTreeMap<_, _>>()),
                "markdown_files" => json!(engine.ordered_note_paths()),
                "cached_files" => json!(engine.cached_file_paths()),
                "cache" | "cache_json" => {
                    let mut cache = Map::new();
                    for path in &selection.metadata_paths {
                        let value = match engine.file_cache(path) {
                            Some(value) if field == "cache_json" => {
                                json!({"present":true,"json":serde_json::to_string(value)?})
                            }
                            Some(value) => {
                                json!({"present":true,"value":serde_json::to_value(value)?})
                            }
                            None => json!({"present":false}),
                        };
                        cache.insert(path.clone(), value);
                    }
                    Value::Object(cache)
                }
                "readiness"
                | "resolved_links"
                | "unresolved_links"
                | "file_info"
                | "file_stat"
                | "info_stat_matches"
                | "preload_promise_present"
                | "layout_ready" => {
                    let Some(frame) = &frame else {
                        unavailable.insert(
                            format!("metadata/{field}"),
                            "The metadata lifecycle owner is not initialized.".into(),
                        );
                        continue;
                    };
                    match field.as_str() {
                        "readiness" => serde_json::to_value(&frame.readiness)?,
                        "preload_promise_present" => json!(frame.preload_promise_present),
                        "layout_ready" => json!(frame.layout_ready),
                        "resolved_links" | "unresolved_links" => {
                            let links = if field == "resolved_links" {
                                &frame.resolved
                            } else {
                                &frame.unresolved
                            };
                            let selected: Map<String, Value> = selection
                                .metadata_paths
                                .iter()
                                .map(|path| {
                                    let value = links
                                        .get(path)
                                        .map(|links| json!({"present":true,"value":links}))
                                        .unwrap_or_else(|| json!({"present":false,"value":null}));
                                    (path.clone(), value)
                                })
                                .collect();
                            Value::Object(selected)
                        }
                        "file_info" => Value::Object(
                            selection
                                .metadata_paths
                                .iter()
                                .map(|path| {
                                    (
                                        path.clone(),
                                        json!(frame.file_info.get(path).and_then(Option::as_ref)),
                                    )
                                })
                                .collect(),
                        ),
                        "file_stat" => Value::Object(
                            selection
                                .metadata_paths
                                .iter()
                                .map(|path| {
                                    (
                                        path.clone(),
                                        json!(frame.file_stat.get(path).and_then(Option::as_ref)),
                                    )
                                })
                                .collect(),
                        ),
                        "info_stat_matches" => Value::Object(
                            selection
                                .metadata_paths
                                .iter()
                                .map(|path| {
                                    (
                                        path.clone(),
                                        json!(frame.info_stat_matches.get(path).copied().flatten()),
                                    )
                                })
                                .collect(),
                        ),
                        _ => unreachable!(),
                    }
                }
                _ => {
                    unavailable.insert(
                        format!("metadata/{field}"),
                        "This metadata field has no retained observer authority yet.".into(),
                    );
                    continue;
                }
            };
            metadata.insert(field.clone(), value);
        }
        observation.insert("metadata".into(), Value::Object(metadata));
    }
    if !selection.session_fields.is_empty() {
        let state = engine.session();
        let mut session = Map::new();
        for field in &selection.session_fields {
            let value = match field.as_str() {
                "active_file" => json!(engine.active_file()),
                "active_leaf" => json!(state.active_leaf),
                "leaves" => json!(state
                    .leaves
                    .iter()
                    .map(|leaf| json!({"id":leaf.id,"file":leaf.file,"view_type":leaf.view_type}))
                    .collect::<Vec<_>>()),
                "last_open_files" => json!(engine.recents()),
                "buffers" => {
                    let buffers: Map<String, Value> = state.leaf_buffers.iter().map(|(id, buffer)| {
                        (id.clone(), json!({"file":state.leaves.iter().find(|leaf| leaf.id == *id).and_then(|leaf|leaf.file.as_ref()),"text": buffer.text, "saved_text": buffer.saved_text,
                                            "dirty": buffer.is_dirty(), "selection": buffer.selection}))
                    }).collect();
                    Value::Object(buffers)
                }
                _ => {
                    unavailable.insert(
                        format!("session/{field}"),
                        "This retained session field has no observer contract yet.".into(),
                    );
                    continue;
                }
            };
            session.insert(field.clone(), value);
        }
        observation.insert("session".into(), Value::Object(session));
    }
    if !selection.event_names.is_empty() {
        let mut vault_requested = Vec::new();
        let mut lifecycle_requested = Vec::new();
        let mut requires_lifecycle = false;
        for name in &selection.event_names {
            let qualified = if name.contains(':') {
                name.clone()
            } else {
                format!("vault:{name}")
            };
            match qualified.as_str() {
                "vault:create" => vault_requested.push(crate::events::EventName::Create),
                "vault:modify" => vault_requested.push(crate::events::EventName::Modify),
                "vault:rename" => vault_requested.push(crate::events::EventName::Rename),
                "vault:delete" => vault_requested.push(crate::events::EventName::Delete),
                "vault:external-watch"
                | "metadata:changed"
                | "metadata:resolve"
                | "metadata:resolved"
                | "metadata:deleted"
                | "metadata:finished" => requires_lifecycle = true,
                _ => {
                    unavailable.insert(
                        format!("events/{name}"),
                        "This event domain/name has no journal authority yet.".into(),
                    );
                    continue;
                }
            }
            lifecycle_requested.push(qualified);
        }
        if requires_lifecycle {
            match engine.lifecycle_journal_since(selection.after_cursor) {
                Ok(records) => {
                    let records: Vec<_> = records
                        .iter()
                        .filter(|event| lifecycle_requested.contains(&event.qualified_name()))
                        .collect();
                    observation.insert("events".into(),json!({"next_cursor":engine.lifecycle_cursor(),"records":records,"cursor_domain":"lifecycle"}));
                }
                Err(error) => {
                    // Lifecycle cursors belong to a separate collector. Never
                    // reinterpret a disabled/stale/future cursor as Vault history.
                    for name in &lifecycle_requested {
                        unavailable.insert(format!("events/{name}"), error.to_string());
                    }
                    unavailable.insert("events".into(), error.to_string());
                }
            }
        } else {
            insert_vault_events(
                engine,
                selection.after_cursor,
                &vault_requested,
                &mut observation,
                &mut unavailable,
            );
        }
    }
    let mut readiness = json!({"ready":false,"pending":["Readiness must be established by the operation/observer host; snapshots alone cannot prove it."]});
    for field in &selection.readiness_fields {
        if field != "metadata" {
            unavailable.insert(
                format!("readiness/{field}"),
                "This readiness field has no owner authority yet.".into(),
            );
            continue;
        }
        if !engine.metadata_lifecycle_enabled() {
            unavailable.insert(
                "readiness/metadata".into(),
                "The metadata lifecycle owner is not initialized.".into(),
            );
            continue;
        }
        let state = engine.metadata_readiness();
        let mut pending = Vec::new();
        if !state.initialized {
            pending.push("Metadata owner initialization pending");
        }
        if state.in_progress != 0 {
            pending.push("Metadata computations pending");
        }
        if state.queue_length != 0 {
            pending.push("Link resolutions queued");
        }
        if state.resolver_running {
            pending.push("Link resolver running");
        }
        if !state.is_cache_clean && pending.is_empty() {
            pending.push("Metadata owner reports a pending transition");
        }
        readiness = json!({"ready":state.initialized && state.is_cache_clean,"scope":"metadata_owner","metadata":state,"pending":pending});
    }
    Ok(Observation {
        observation,
        unavailable,
        readiness,
    })
}

fn insert_vault_events(
    engine: &Engine,
    cursor: u64,
    requested: &[crate::events::EventName],
    observation: &mut Map<String, Value>,
    unavailable: &mut BTreeMap<String, String>,
) {
    match engine.event_journal_since(cursor) {
        Ok(records) => {
            let records: Vec<_> = records
                .iter()
                .filter(|record| requested.contains(&record.name))
                .collect();
            observation.insert(
                "events".into(),
                json!({"next_cursor":engine.event_cursor(),"records":records}),
            );
        }
        Err(error) => {
            unavailable.insert("events".into(), error.to_string());
        }
    }
}
