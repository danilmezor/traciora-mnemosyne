//! Command dispatch over a live, ordered registry.
//!
//! Metadata and argument behavior are backed by the isolated 1.14.4 framework
//! fixtures. Handlers remain caller-owned, so plugin registration can use the
//! same dispatcher as built-in commands without a second alias implementation.
use crate::{command_args as args, command_args::Params};
use indexmap::IndexMap;
use serde_json::{Map, Value};
use std::cmp::Ordering;

#[derive(Clone, Debug, PartialEq)]
pub struct FlagSpec {
    /// Preserve declared fields and their order for completion output.
    pub metadata: Map<String, Value>,
}
impl FlagSpec {
    pub fn new(description: impl Into<String>) -> Self {
        let mut metadata = Map::new();
        metadata.insert("description".into(), Value::String(description.into()));
        Self { metadata }
    }
    pub fn from_json(metadata: Map<String, Value>) -> Self {
        Self { metadata }
    }
    pub fn with_value(mut self, value: impl Into<String>) -> Self {
        // A conventional value flag declares its value before its description.
        let mut metadata = Map::new();
        metadata.insert("value".into(), Value::String(value.into()));
        metadata.extend(self.metadata);
        self.metadata = metadata;
        self
    }
    pub fn required(mut self) -> Self {
        self.metadata.insert("required".into(), Value::Bool(true));
        self
    }
    pub fn value(&self) -> Option<&str> {
        self.metadata.get("value").and_then(Value::as_str)
    }
    pub fn is_required(&self) -> bool {
        self.metadata.get("required").is_some_and(json_truthy)
    }
    pub fn description(&self) -> String {
        match self.metadata.get("description") {
            Some(Value::String(value)) => value.clone(),
            Some(value) => value.to_string(),
            None => "undefined".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CommandSpec {
    pub description: String,
    /// None means no flag object; Some(empty) still allows inherited aliases.
    pub flags: Option<IndexMap<String, FlagSpec>>,
}
impl CommandSpec {
    pub fn new(description: impl Into<String>, flags: Option<IndexMap<String, FlagSpec>>) -> Self {
        Self {
            description: description.into(),
            flags,
        }
    }
    pub fn usage(&self) -> String {
        self.flag_entries()
            .into_iter()
            .map(|(name, flag)| {
                let text = flag_name(name, flag);
                if flag.is_required() {
                    text
                } else {
                    format!("[{text}]")
                }
            })
            .collect::<Vec<_>>()
            .join(" ")
    }
    fn flag_entries(&self) -> Vec<(&str, &FlagSpec)> {
        let mut entries: Vec<_> = self
            .flags
            .iter()
            .flat_map(|flags| flags.iter())
            .map(|(name, flag)| (name.as_str(), flag))
            .collect();
        entries.sort_by(|(a, _), (b, _)| args::object_key_compare(a, b));
        entries
    }
    fn has_flag(&self, name: &str) -> bool {
        self.flags
            .as_ref()
            .is_some_and(|flags| flags.contains_key(name) || args::is_prototype_key(name))
    }
}

#[derive(Debug)]
pub struct Registration<H> {
    pub spec: CommandSpec,
    pub handler: H,
}

#[derive(Debug)]
pub struct CommandRegistry<H = ()> {
    entries: IndexMap<String, Registration<H>>,
}
impl<H> Default for CommandRegistry<H> {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreparedCommand {
    pub command: String,
    pub params: Params,
    pub copy_requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{message}")]
pub struct RegistryError {
    pub message: String,
}
impl RegistryError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl<H> CommandRegistry<H> {
    pub fn new() -> Self {
        Self {
            entries: IndexMap::new(),
        }
    }
    pub fn register(
        &mut self,
        name: impl Into<String>,
        spec: CommandSpec,
        handler: H,
    ) -> Result<(), RegistryError> {
        let name = name.into();
        if self.entries.contains_key(&name) {
            return Err(RegistryError::new(format!(
                "Command \"{name}\" is already registered as a handler."
            )));
        }
        self.entries.insert(name, Registration { spec, handler });
        Ok(())
    }
    pub fn get(&self, name: &str) -> Option<&Registration<H>> {
        self.entries.get(name)
    }
    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.keys().map(String::as_str)
    }
    pub fn unregister(&mut self, name: &str) -> Option<Registration<H>> {
        self.entries.shift_remove(name)
    }
    pub fn unregister_if(&mut self, name: &str, same_handler: impl FnOnce(&H) -> bool) -> bool {
        if self
            .get(name)
            .is_some_and(|entry| same_handler(&entry.handler))
        {
            self.unregister(name);
            true
        } else {
            false
        }
    }
    pub fn prepare(&self, argv: &[String]) -> Result<PreparedCommand, RegistryError> {
        let (command, params) = args::parse_argv(argv);
        self.prepare_params(&command, params)
    }
    /// Prepare already-parsed application arguments after transport vault selection.
    pub fn prepare_params(
        &self,
        command: &str,
        mut params: Params,
    ) -> Result<PreparedCommand, RegistryError> {
        args::normalize_params(&mut params);
        let mut command = if command.is_empty() || command == "--help" {
            "help"
        } else {
            command
        }
        .to_string();
        if !self.entries.contains_key(&command) {
            if let Some((base, flag)) = command.rsplit_once(':') {
                if self
                    .get(base)
                    .is_some_and(|entry| entry.spec.has_flag(flag))
                {
                    let base = base.to_owned();
                    args::assign(&mut params, flag, "true");
                    args::normalize_params(&mut params);
                    command = base;
                }
            }
        }
        let Some(entry) = self.get(&command) else {
            let hints = suggestions(&command, self.names());
            let mut message = format!("Command \"{command}\" not found.");
            if hints.is_empty() {
                message.push_str(" It may require a plugin to be enabled.");
            } else {
                message.push_str(&format!(" Did you mean: {}?", hints.join(", ")));
            }
            return Err(RegistryError::new(message));
        };
        args::apply_format_aliases(
            &mut params,
            entry
                .spec
                .flags
                .as_ref()
                .and_then(|flags| flags.get("format"))
                .and_then(FlagSpec::value),
        );
        let missing: Vec<_> = entry
            .spec
            .flag_entries()
            .into_iter()
            .filter(|(name, flag)| flag.is_required() && !args::property_present(&params, name))
            .map(|(name, flag)| flag_name(name, flag))
            .collect();
        if !missing.is_empty() {
            return Err(RegistryError::new(format!(
                "Missing required parameter: {}\nUsage: {command} {}",
                missing.join(", "),
                entry.spec.usage()
            )));
        }
        let copy_requested =
            params.get("--copy").and_then(|value| value.as_deref()) == Some("true");
        params.shift_remove("--copy");
        Ok(PreparedCommand {
            command,
            params,
            copy_requested,
        })
    }
    pub fn help(&self, params: &Params) -> String {
        self.help_with(params, natural_compare)
    }
    /// Supply the application's locale collator for non-ASCII command names.
    /// The default comparator covers the captured command-name subset; ICU
    /// locale collation is an explicit application integration boundary.
    pub fn help_with(&self, params: &Params, compare: impl Fn(&str, &str) -> Ordering) -> String {
        let mut names: Vec<_> = self.names().collect();
        names.sort_by(|a, b| compare(a, b));
        let mut keys: Vec<_> = params.keys().map(String::as_str).collect();
        keys.sort_by(|a, b| args::object_key_compare(a, b));
        let requested = keys
            .into_iter()
            .find(|key| params.get(*key).and_then(|v| v.as_deref()) == Some("true"));
        if let Some(requested) = requested {
            let matching: Vec<_> = names
                .iter()
                .copied()
                .filter(|name| *name == requested || name.starts_with(&format!("{requested}:")))
                .collect();
            if matching.is_empty() {
                let hints = suggestions(requested, names.iter().copied());
                let mut message = format!("No commands matching \"{requested}\".");
                if !hints.is_empty() {
                    message.push_str(&format!(" Did you mean: {}?", hints.join(", ")));
                }
                return message;
            }
            return matching
                .into_iter()
                .map(|name| self.command_help(name))
                .collect::<Vec<_>>()
                .join("\n");
        }
        let mut commands = Vec::new();
        let mut developer = Vec::new();
        for name in names.into_iter().filter(|name| !name.starts_with("__")) {
            if name.starts_with("dev:") || name == "devtools" || name == "eval" {
                developer.push(self.command_help(name));
            } else {
                commands.push(self.command_help(name));
            }
        }
        format!("Mnemosyne CLI\n\nUsage: mnemosyne <command> [options]\n\nOptions:\n  vault=<name>          Target a specific vault by name\n\nNotes:\n  file resolves by name (like wikilinks), path is exact (folder/note.md)\n  Most commands default to the active file when file/path is omitted\n  Quote values with spaces: name=\"My Note\"\n  Use \\n for newline, \\t for tab in content values\n\nCommands:\n{}\n\nDeveloper:\n{}", commands.join("\n"), developer.join("\n"))
    }
    pub fn command_help(&self, name: &str) -> String {
        let Some(entry) = self.get(name) else {
            return String::new();
        };
        let mut lines = vec![format!("  {}{}", pad_end(name, 22), entry.spec.description)];
        for (name, flag) in entry.spec.flag_entries() {
            let label = flag_name(name, flag);
            let label = if label.encode_utf16().count() >= 20 {
                format!("{label}  ")
            } else {
                pad_end(&label, 20)
            };
            let description = flag.description();
            lines.push(format!(
                "    {label}- {description}{}",
                if flag.is_required() {
                    " (required)"
                } else {
                    ""
                }
            ));
        }
        lines.join("\n") + "\n"
    }
    pub fn completions(&self) -> Value {
        let mut result = Map::new();
        for (name, entry) in &self.entries {
            if name.starts_with("__") {
                continue;
            }
            let mut metadata = Map::new();
            metadata.insert("usage".into(), Value::String(entry.spec.usage()));
            metadata.insert(
                "description".into(),
                Value::String(entry.spec.description.clone()),
            );
            if entry.spec.flags.is_some() {
                let mut flags = Map::new();
                for (name, flag) in entry.spec.flag_entries() {
                    let mut fields = flag.metadata.clone();
                    let description = flag.description();
                    fields.insert(
                        "description".into(),
                        Value::String(if flag.is_required() {
                            format!("{description} (required)")
                        } else {
                            description
                        }),
                    );
                    flags.insert(name.to_owned(), Value::Object(fields));
                }
                metadata.insert("flags".into(), Value::Object(flags));
            }
            result.insert(name.clone(), Value::Object(metadata));
        }
        let mut entries: Vec<_> = result.into_iter().collect();
        entries.sort_by(|(a, _), (b, _)| args::object_key_compare(a, b));
        Value::Object(entries.into_iter().collect())
    }
}

fn flag_name(name: &str, flag: &FlagSpec) -> String {
    match flag.value().filter(|value| !value.is_empty()) {
        Some(value) => format!("{name}={value}"),
        None => name.to_owned(),
    }
}
fn json_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|v| v != 0.0),
        Value::String(value) => !value.is_empty(),
        _ => true,
    }
}
fn pad_end(value: &str, width: usize) -> String {
    format!(
        "{value}{}",
        " ".repeat(width.saturating_sub(value.encode_utf16().count()))
    )
}

/// Native application replies contain no bytes for empty results and add LF
/// only when a nonempty result lacks one; existing CRLF remains intact.
pub fn frame_output(result: &str) -> String {
    if result.is_empty() || result.ends_with('\n') {
        result.to_owned()
    } else {
        format!("{result}\n")
    }
}
pub fn frame_error(error: &RegistryError) -> String {
    frame_output(&format!("Error: {error}"))
}

pub fn suggestions<'a>(query: &str, names: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    let folded = query.to_lowercase();
    let query_units: Vec<_> = folded.encode_utf16().collect();
    let query_len = query.encode_utf16().count();
    let mut candidates = Vec::new();
    for name in names {
        let lower = name.to_lowercase();
        let score = if lower.starts_with(&folded) {
            0
        } else if lower.contains(&folded) {
            1
        } else {
            let units: Vec<_> = lower.encode_utf16().collect();
            let mut distances: Vec<usize> = (0..=query_len).collect();
            for row in 0..name.encode_utf16().count() {
                let mut previous = distances[0];
                distances[0] += 1;
                for column in 1..=query_len {
                    let old = distances[column];
                    distances[column] = (old + 1)
                        .min(distances[column - 1] + 1)
                        .min(previous + usize::from(query_units.get(column - 1) != units.get(row)));
                    previous = old;
                }
            }
            if distances[query_len] <= 3 {
                2 + distances[query_len]
            } else {
                999
            }
        };
        if score < 999 {
            candidates.push((score, name.to_owned()));
        }
    }
    candidates.sort_by_key(|entry| entry.0);
    candidates
        .into_iter()
        .take(3)
        .map(|entry| entry.1)
        .collect()
}

/// Stable natural comparison for the captured command-name subset; applications can pass
/// their locale's Intl-compatible comparator to `help_with`.
fn natural_compare(a: &str, b: &str) -> Ordering {
    let a = a.to_lowercase();
    let b = b.to_lowercase();
    let (mut a, mut b) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (a.peek().copied(), b.peek().copied()) {
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let mut x = String::new();
                let mut y = String::new();
                while a.peek().is_some_and(|c| c.is_ascii_digit()) {
                    x.push(a.next().unwrap());
                }
                while b.peek().is_some_and(|c| c.is_ascii_digit()) {
                    y.push(b.next().unwrap());
                }
                let x = x.trim_start_matches('0');
                let y = y.trim_start_matches('0');
                let order = x.len().cmp(&y.len()).then_with(|| x.cmp(y));
                if order != Ordering::Equal {
                    return order;
                }
            }
            (Some(x), Some(y)) => {
                a.next();
                b.next();
                if x != y {
                    return x.cmp(&y);
                }
            }
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
        }
    }
}
