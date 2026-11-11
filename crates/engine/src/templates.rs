//! Named templates resolve through the observed core-plugin rules. Template
//! variable substitution uses the shared Rust Moment-format compatibility layer.
//! Insertion shares retained editor selection, metadata merge and save timers.
use crate::{daily, Engine, EngineError};
use serde_json::Value;
use time::OffsetDateTime;

/// Native insertion returns without changing state when there is no active
/// editor or when a non-start template has invalid YAML properties.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TemplateInsertOutcome {
    NoActiveEditor,
    InvalidProperties,
    Inserted { property_save_requested: bool },
}

impl Engine {
    pub fn insert_named_template(
        &mut self,
        name: &str,
    ) -> Result<TemplateInsertOutcome, EngineError> {
        self.insert_named_template_inner(name, daily::now()?, None)
    }

    /// Deterministic date/time substitution and the existing logical editor clock.
    pub fn insert_named_template_at(
        &mut self,
        name: &str,
        now: OffsetDateTime,
        edit_ms: u64,
    ) -> Result<TemplateInsertOutcome, EngineError> {
        self.insert_named_template_inner(name, now, Some(edit_ms))
    }

    fn insert_named_template_inner(
        &mut self,
        name: &str,
        now: OffsetDateTime,
        edit_ms: Option<u64>,
    ) -> Result<TemplateInsertOutcome, EngineError> {
        let Some(title) = self.template_editor_title() else {
            return Ok(TemplateInsertOutcome::NoActiveEditor);
        };
        let rendered = self.render_named_template_at(name, &title, now)?;
        self.insert_rendered_template(&rendered, edit_ms)
    }

    /// Backend operation after a caller has selected a concrete vault template.
    pub fn insert_template_file(
        &mut self,
        path: &str,
    ) -> Result<TemplateInsertOutcome, EngineError> {
        self.insert_template_file_inner(path, daily::now()?, None)
    }

    pub fn insert_template_file_at(
        &mut self,
        path: &str,
        now: OffsetDateTime,
        edit_ms: u64,
    ) -> Result<TemplateInsertOutcome, EngineError> {
        self.insert_template_file_inner(path, now, Some(edit_ms))
    }

    fn insert_template_file_inner(
        &mut self,
        path: &str,
        now: OffsetDateTime,
        edit_ms: Option<u64>,
    ) -> Result<TemplateInsertOutcome, EngineError> {
        let Some(title) = self.template_editor_title() else {
            return Ok(TemplateInsertOutcome::NoActiveEditor);
        };
        let rendered = render_file(self, path, &title, now)?;
        self.insert_rendered_template(&rendered, edit_ms)
    }

    fn template_editor_title(&self) -> Option<String> {
        self.markdown_state()?;
        self.editor()?;
        Some(
            self.active_file()
                .map(mnemosyne_core::basename_no_md)
                .unwrap_or_default(),
        )
    }

    fn insert_rendered_template(
        &mut self,
        rendered: &str,
        edit_ms: Option<u64>,
    ) -> Result<TemplateInsertOutcome, EngineError> {
        let buffer = self.editor().ok_or(EngineError::NoEditor)?;
        let from = buffer
            .offset(buffer.selection.anchor)?
            .min(buffer.offset(buffer.selection.head)?);
        if from == 0 {
            self.editor_mut().unwrap().replace_selection(rendered)?;
            if let Some(edit_ms) = edit_ms {
                self.mark_editor_changed_at(edit_ms)?;
            }
            return Ok(TemplateInsertOutcome::Inserted {
                property_save_requested: false,
            });
        }
        let bounds = mnemosyne_core::frontmatter::processing_bounds(rendered);
        let properties = match bounds.as_ref() {
            Some(bounds) if bounds.from != bounds.to => {
                match mnemosyne_core::yaml_graph::parse(&rendered[bounds.from..bounds.to]) {
                    Ok(document) => document,
                    Err(_) => return Ok(TemplateInsertOutcome::InvalidProperties),
                }
            }
            _ => mnemosyne_core::yaml_graph::parse("{}").unwrap(),
        };
        let body = &rendered[bounds.as_ref().map_or(0, |bounds| bounds.content_start)..];
        self.editor_mut().unwrap().replace_selection(body)?;
        if let Some(edit_ms) = edit_ms {
            self.mark_editor_changed_at(edit_ms)?;
        }
        // yB returns a truthy empty object for plain templates. A parsed null,
        // false, zero or empty string skips MetadataEditor.insertProperties.
        use mnemosyne_core::yaml_value::Node;
        let truthy = match &properties.nodes[properties.root] {
            Node::Null => false,
            Node::Bool(value) => *value,
            Node::Number(value) => value
                .as_f64()
                .is_some_and(|value| value != 0.0 && !value.is_nan()),
            Node::String(value) => !value.is_empty(),
            Node::Wtf16String(value) => !value.is_empty(),
            _ => true,
        };
        if truthy {
            self.insert_template_properties(&properties)?;
        }
        Ok(TemplateInsertOutcome::Inserted {
            property_save_requested: truthy,
        })
    }

    fn insert_template_properties(
        &mut self,
        properties: &mnemosyne_core::yaml_graph::Document,
    ) -> Result<(), EngineError> {
        // MetadataEditor.serialize reads the retained properties published by
        // the last internal-data callback, even while editor text is dirty.
        let state = self.markdown_state().ok_or(EngineError::NoEditor)?;
        let retained = state
            .template_properties_data
            .as_deref()
            .unwrap_or(state.reading_data());
        let existing = mnemosyne_core::frontmatter::processing_bounds(retained)
            .and_then(|bounds| {
                mnemosyne_core::yaml_graph::parse(&retained[bounds.from..bounds.to]).ok()
            })
            .filter(|document| {
                matches!(
                    document.nodes[document.root],
                    mnemosyne_core::yaml_value::Node::Object(_)
                        | mnemosyne_core::yaml_value::Node::ObjectWithUtf16Keys(_)
                )
            })
            .unwrap_or_else(|| mnemosyne_core::yaml_graph::parse("{}").unwrap());
        let merged = existing
            .merged_template_properties(properties)
            .map_err(|error| {
                EngineError::InvalidState(format!("Template properties: {error:?}"))
            })?;
        let yaml = merged.stringify().map_err(|error| {
            EngineError::InvalidState(format!("Template properties: {error:?}"))
        })?;
        let prefix = if merged.own_fields_units().is_empty() {
            String::new()
        } else {
            format!("---\n{}\n---\n", yaml.trim())
        };
        let text = self.markdown_view_data().ok_or(EngineError::NoEditor)?;
        let bounds = mnemosyne_core::frontmatter::processing_bounds(text);
        let data = format!(
            "{prefix}{}",
            &text[bounds.map_or(0, |bounds| bounds.content_start)..]
        );
        // setViewData skips a programmatic editor transaction for equal view
        // data. Reading mode therefore retains its independent hidden buffer.
        if data != self.markdown_state().unwrap().reading_data() {
            if self.markdown_state().unwrap().mode == crate::markdown::MarkdownMode::Source {
                crate::editor_merge::load_contents(self.editor_mut().unwrap(), &data)?;
            }
            let id = self
                .session
                .active_leaf
                .clone()
                .ok_or(EngineError::NoEditor)?;
            self.session
                .leaves
                .iter_mut()
                .find(|leaf| leaf.id == id)
                .unwrap()
                .markdown
                .as_mut()
                .unwrap()
                .data = data;
        }
        let id = self
            .session
            .active_leaf
            .clone()
            .ok_or(EngineError::NoEditor)?;
        self.publish_template_markdown_data(&id)?;
        self.queue_template_save(&id)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum TemplateError {
    #[error("Templates plugin is not enabled.")]
    Disabled,
    #[error("Missing required parameter: name\nUsage: name=<template>")]
    MissingName,
    #[error("No template folder configured.")]
    NoFolder,
    #[error("Template folder \"{0}\" not found.")]
    MissingFolder(String),
    #[error("Template \"{0}\" not found.")]
    MissingTemplate(String),
    #[error("Template folder root/surrogate slicing remains pending.")]
    PendingRoot,
}

pub(crate) fn render_named(
    engine: &mut Engine,
    name: &str,
    title: &str,
    now: OffsetDateTime,
) -> Result<String, EngineError> {
    if !daily::plugin_enabled(engine, "templates", true)? {
        return Err(TemplateError::Disabled.into());
    }
    if name.is_empty() {
        return Err(TemplateError::MissingName.into());
    }
    let options = daily::read_options(engine, "templates");
    let raw_folder = options
        .get("folder")
        .and_then(Value::as_str)
        .filter(|folder| !folder.is_empty())
        .ok_or(TemplateError::NoFolder)?;
    let folder = mnemosyne_core::normalize_path(raw_folder);
    if !daily::is_folder(engine, &folder) {
        return Err(TemplateError::MissingFolder(raw_folder.into()).into());
    }
    if folder.is_empty() {
        return Err(TemplateError::PendingRoot.into());
    }
    let mut path = format!("{folder}/{name}");
    if !path.ends_with(".md") {
        path.push_str(".md");
    }
    let found = if engine.vault.has_file(&path) {
        Some(path)
    } else {
        let prefix = format!("{folder}/");
        let lower = name.to_lowercase();
        engine
            .ordered_note_paths()
            .into_iter()
            .find(|path| {
                mnemosyne_core::extension(path) == "md"
                    && path.strip_prefix(&prefix).is_some_and(|relative| {
                        relative[..relative.len() - 3].to_lowercase() == lower
                    })
            })
            .map(str::to_owned)
    }
    .ok_or_else(|| TemplateError::MissingTemplate(name.into()))?;
    render_file(engine, &found, title, now)
}

fn render_file(
    engine: &mut Engine,
    path: &str,
    title: &str,
    now: OffsetDateTime,
) -> Result<String, EngineError> {
    let options = daily::read_options(engine, "templates");
    let text = engine.read_saved(crate::FileTarget {
        path: Some(path),
        file: None,
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let date_format = options
        .get("dateFormat")
        .and_then(Value::as_str)
        .filter(|format| !format.is_empty())
        .unwrap_or("YYYY-MM-DD");
    let time_format = options
        .get("timeFormat")
        .and_then(Value::as_str)
        .filter(|format| !format.is_empty())
        .unwrap_or("HH:mm");
    Ok(daily::render_template(
        text,
        title,
        now,
        date_format,
        time_format,
        engine.locale(),
    )?)
}
