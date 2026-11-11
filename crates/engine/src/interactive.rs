//! Nonvisual terminal-session state, recovered from pinned main.js Ro/Lo/nt.
//! ANSI drawing is separate. Completion catalogs are supplied by the caller;
//! this module never invents missing command/plugin metadata.
use crate::metadata_types::lower_units;
use mnemosyne_core::collation::UnicodeCaseError;

pub type Units = Vec<u16>;
/// Nonvisual completion text and its native description. ANSI rendering is separate.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    pub text: Units,
    pub description: Option<Units>,
}
#[derive(Clone, Debug, Default)]
struct CompletionCommand {
    name: Units,
    flags: Vec<(Units, Option<Units>)>,
    values: Vec<(Units, Units)>,
}
/// Frozen inputs to native `nt.initCompletions` (main.js 6868..10300).
/// Catalog availability is the caller's responsibility; absent actions stay absent.
#[derive(Clone, Debug, Default)]
pub struct CompletionCatalog {
    commands: Vec<CompletionCommand>,
    names: Vec<(Units, Option<Units>)>,
    vaults: Vec<Units>,
    files: Vec<Units>,
    actions: Vec<(Units, Units)>,
}
fn metadata_entries(value: &serde_json::Value) -> Vec<(&String, &serde_json::Value)> {
    let mut entries: Vec<_> = value
        .as_object()
        .map(|o| o.iter().collect())
        .unwrap_or_default();
    entries.sort_by(|(a, _), (b, _)| crate::command_args::object_key_compare(a, b));
    entries
}
fn usage_flags(value: &str) -> Vec<Units> {
    // Io's ASCII JS \w expression: \[?(\w+(?:=(?:<[^>]+>)?)?)\]?
    let bytes = value.as_bytes();
    let mut flags = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if !bytes[i].is_ascii_alphanumeric() && bytes[i] != b'_' {
            i += 1;
            continue;
        }
        let start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        let name_end = i;
        if i < bytes.len() && bytes[i] == b'=' {
            i += 1;
            flags.push(units(&value[start..i]));
            if i < bytes.len() && bytes[i] == b'<' {
                if let Some(end) = bytes[i + 1..].iter().position(|b| *b == b'>') {
                    if end > 0 {
                        i += end + 2;
                    }
                }
            }
        } else {
            flags.push(units(&value[start..name_end]));
        }
    }
    flags
}
fn contains_units(input: &[u16], needle: &[u16]) -> bool {
    needle.is_empty() || input.windows(needle.len()).any(|part| part == needle)
}
fn join_words(words: &[&[u16]]) -> Units {
    let mut out = Vec::new();
    for word in words {
        if !out.is_empty() {
            out.push(32);
        }
        out.extend_from_slice(word);
    }
    out
}
impl CompletionCatalog {
    pub fn new(
        commands: serde_json::Value,
        vaults: Vec<Units>,
        files: Vec<Units>,
        command_actions: Vec<(Units, Units)>,
    ) -> Self {
        let mut result = Self {
            vaults,
            files,
            actions: command_actions,
            ..Self::default()
        };
        for (name, metadata) in metadata_entries(&commands) {
            let description = metadata
                .get("description")
                .and_then(|v| v.as_str())
                .map(units);
            let mut command = CompletionCommand {
                name: units(name),
                ..Default::default()
            };
            if metadata
                .get("flags")
                .is_some_and(|v| !v.is_null() && v != &serde_json::Value::Bool(false))
            {
                for (key, flag) in metadata_entries(&metadata["flags"]) {
                    let value = flag
                        .get("value")
                        .and_then(|v| v.as_str())
                        .filter(|v| !v.is_empty());
                    let suffix = if value.is_some() { "=" } else { "" };
                    command.flags.push((
                        units(&format!("{key}{suffix}")),
                        flag.get("description").and_then(|v| v.as_str()).map(units),
                    ));
                    if let Some(value) = value {
                        command.values.push((units(key), units(value)));
                    }
                }
            } else {
                command.flags =
                    usage_flags(metadata.get("usage").and_then(|v| v.as_str()).unwrap_or(""))
                        .into_iter()
                        .map(|v| (v, None))
                        .collect();
            }
            result.names.push((command.name.clone(), description));
            result.commands.push(command);
        }
        for (name, description) in [
            ("exit", "Exit the CLI"),
            ("quit", "Exit the CLI"),
            ("vault:open", "Switch to a different vault"),
        ] {
            // Source overwrites descriptions, then concatenates names (duplicates remain).
            for (key, d) in &mut result.names {
                if equals(key, name) {
                    *d = Some(units(description));
                }
            }
            result.names.push((units(name), Some(units(description))));
        }
        result.names.sort_by(|a, b| a.0.cmp(&b.0)); // Array.sort() compares UTF16 code units.
        result
    }
    /// Convenience API for callers whose pinned ICU backend is available.
    /// Use try_suggestions when backend failure must be delivered as an error.
    pub fn suggestions(&self, input: &[u16]) -> Vec<Units> {
        self.try_suggestions(input)
            .expect("pinned ICU completion casing unavailable")
    }
    pub fn try_suggestions(&self, input: &[u16]) -> Result<Vec<Units>, UnicodeCaseError> {
        Ok(self
            .suggestion_details(input)?
            .into_iter()
            .map(|c| c.text)
            .collect())
    }
    pub fn suggestion_details(&self, input: &[u16]) -> Result<Vec<Completion>, UnicodeCaseError> {
        let trimmed = trim(input);
        let words: Vec<&[u16]> = trimmed
            .split(|u| whitespace(*u))
            .filter(|w| !w.is_empty())
            .collect();
        let trailing_space = input.last() == Some(&32);
        let current = if trailing_space {
            &[][..]
        } else {
            words.last().copied().unwrap_or(&[])
        };
        let mut out = Vec::new();
        if words.len() <= 1 && !trailing_space {
            let prefix = lower_units(current)?;
            for (name, description) in &self.names {
                if lower_units(name)?.starts_with(&prefix) {
                    out.push(Completion {
                        text: name.clone(),
                        description: description.clone(),
                    });
                }
            }
            if !prefix.is_empty() {
                for (name, description) in &self.names {
                    if !lower_units(name)?.starts_with(&prefix)
                        && description
                            .as_ref()
                            .map(|d| lower_units(d))
                            .transpose()?
                            .is_some_and(|d| contains_units(&d, &prefix))
                    {
                        out.push(Completion {
                            text: name.clone(),
                            description: description.clone(),
                        });
                    }
                }
            }
            return Ok(out);
        }
        let command_name = words.first().copied().unwrap_or(&[]);
        let before = if trailing_space {
            trimmed.to_vec()
        } else {
            join_words(&words[..words.len().saturating_sub(1)])
        };
        if equals(command_name, "vault:open") && words.len() <= 2 {
            let prefix = lower_units(current)?;
            for vault in &self.vaults {
                if lower_units(vault)?.starts_with(&prefix) {
                    let mut text = units("vault:open ");
                    if vault.contains(&32) {
                        text.push(34);
                        text.extend(vault);
                        text.push(34);
                    } else {
                        text.extend(vault);
                    }
                    out.push(Completion {
                        text,
                        description: Some(units("Open vault")),
                    });
                }
            }
            return Ok(out);
        }
        let command = self.commands.iter().find(|c| c.name == command_name);
        if let Some(equal) = current.iter().position(|u| *u == 61) {
            let flag = &current[..equal];
            let value = lower_units(&current[equal + 1..])?;
            let mut add = |candidate: &[u16], quoted: bool, description: Option<Units>| {
                let mut text = before.clone();
                if !text.is_empty() {
                    text.push(32);
                }
                text.extend(flag);
                text.push(61);
                if quoted {
                    text.push(34);
                }
                text.extend(candidate);
                if quoted {
                    text.push(34);
                }
                out.push(Completion { text, description });
                out.len()
            };
            if equals(flag, "file") || equals(flag, "path") {
                let base = starts(command_name, "base:");
                for file in &self.files {
                    if (!base || ends(file, ".base")) && contains_units(&lower_units(file)?, &value)
                    {
                        let file = if equals(flag, "file") && ends(file, ".md") {
                            &file[..file.len() - 3]
                        } else {
                            file
                        };
                        if add(file, true, Some(vec![])) == 20 {
                            break;
                        }
                    }
                }
                return Ok(out);
            }
            if equals(flag, "folder") {
                let mut folders: Vec<Units> = Vec::new();
                for file in &self.files {
                    if let Some(slash) = file.iter().rposition(|u| *u == 47).filter(|i| *i > 0) {
                        let folder = file[..slash].to_vec();
                        if !folders.contains(&folder) {
                            folders.push(folder);
                        }
                    }
                }
                for folder in folders {
                    if contains_units(&lower_units(&folder)?, &value) {
                        if add(&folder, true, Some(vec![])) == 20 {
                            break;
                        }
                    }
                }
                return Ok(out);
            }
            if let Some(command) = command {
                if let Some((_, values)) = command.values.iter().find(|(key, _)| key == flag) {
                    if equals(values, "<command-id>") {
                        for (id, name) in &self.actions {
                            if contains_units(&lower_units(id)?, &value)
                                || contains_units(&lower_units(name)?, &value)
                            {
                                if add(id, false, Some(name.clone())) == 20 {
                                    break;
                                }
                            }
                        }
                        return Ok(out);
                    }
                    if values.contains(&124) {
                        let mut suffixed = flag.to_vec();
                        suffixed.push(61);
                        let description = command
                            .flags
                            .iter()
                            .find(|(f, _)| f == &suffixed)
                            .and_then(|(_, d)| d.clone());
                        for option in values.split(|u| *u == 124) {
                            if lower_units(option)?.starts_with(&value) {
                                add(option, false, description.clone());
                            }
                        }
                        return Ok(out);
                    }
                }
            }
        }
        if let Some(command) = command {
            let used: Vec<Units> = words
                .iter()
                .skip(1)
                .map(|w| {
                    let end = w.iter().position(|u| *u == 61).map_or(w.len(), |i| i + 1);
                    w[..end].to_vec()
                })
                .collect();
            for (flag, description) in &command.flags {
                if flag.starts_with(current) && !used.contains(flag) {
                    let mut text = before.clone();
                    text.push(32);
                    text.extend(flag);
                    out.push(Completion {
                        text,
                        description: description.clone(),
                    });
                }
            }
        }
        Ok(out)
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Command {
    Execute {
        vault_selector: Option<Units>,
        argv: Vec<Units>,
    },
    OpenVault {
        name: Units,
    },
    Message {
        text: Units,
        error: bool,
    },
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InputOutcome {
    Continue,
    Execute(Command),
    Exit,
}
#[derive(Clone, Debug, Default)]
pub struct InteractiveSession {
    pub current_vault_id: Option<String>,
    pub current_vault_name: Units,
    pub input_buffer: Units,
    pub cursor: usize,
    pub history: Vec<Units>,
    pub history_index: Option<usize>,
    pub is_processing: bool,
    pub suggestions: Vec<Units>,
    pub suggestion_index: Option<usize>,
    pub input_before_autocomplete: Units,
    pub is_search_mode: bool,
    pub search_query: Units,
    pub search_match_index: usize,
    pub search_matches: Vec<Units>,
    pub input_before_search: Units,
}
fn units(text: &str) -> Units {
    text.encode_utf16().collect()
}
fn whitespace(unit: u16) -> bool {
    char::from_u32(unit as u32).is_some_and(mnemosyne_core::vault_search::js_whitespace)
}
fn trim_end(input: &[u16]) -> &[u16] {
    let end = input
        .iter()
        .rposition(|unit| !whitespace(*unit))
        .map_or(0, |i| i + 1);
    &input[..end]
}
fn trim(input: &[u16]) -> &[u16] {
    let input = trim_end(input);
    let begin = input
        .iter()
        .position(|unit| !whitespace(*unit))
        .unwrap_or(input.len());
    &input[begin..]
}
fn starts(input: &[u16], text: &str) -> bool {
    input.starts_with(&units(text))
}
fn ends(input: &[u16], text: &str) -> bool {
    input.ends_with(&units(text))
}
fn equals(input: &[u16], text: &str) -> bool {
    input == units(text)
}

/// Main-process TUI lexer: ASCII space/tab delimiters, either quote style,
/// backslash escapes even inside quotes, empty tokens and a final escape omitted.
/// UTF16 units (including lone surrogates created by editing) are preserved.
pub fn lex(input: &[u16]) -> Vec<Units> {
    let mut words = Vec::new();
    let mut word = Vec::new();
    let mut quote = None;
    let mut escaped = false;
    for &unit in input {
        if escaped {
            word.push(unit);
            escaped = false;
            continue;
        }
        if unit == b'\\' as u16 {
            escaped = true;
            continue;
        }
        if let Some(delimiter) = quote {
            if delimiter == unit {
                quote = None;
            } else {
                word.push(unit);
            }
        } else if unit == b'\'' as u16 || unit == b'"' as u16 {
            quote = Some(unit);
        } else if unit == b' ' as u16 || unit == b'\t' as u16 {
            if !word.is_empty() {
                words.push(std::mem::take(&mut word));
            }
        } else {
            word.push(unit);
        }
    }
    if !word.is_empty() {
        words.push(word);
    }
    words
}
#[derive(Debug)]
struct Key {
    name: Units,
    ctrl: bool,
    alt: bool,
}
fn key(bytes: &[u8]) -> Key {
    let decoded = String::from_utf8_lossy(bytes);
    let (name, ctrl, alt) = match decoded.as_ref() {
        "\x1b[A" => ("up", false, false),
        "\x1b[B" => ("down", false, false),
        "\x1b[C" => ("right", false, false),
        "\x1b[D" => ("left", false, false),
        "\x1b[3~" => ("delete", false, false),
        "\x1b[Z" => ("shift-tab", false, false),
        "\r" => ("return", false, false),
        "\t" => ("tab", false, false),
        "\x7f" | "\x08" => ("backspace", false, false),
        "\x01" => ("a", true, false),
        "\x02" => ("b", true, false),
        "\x03" => ("c", true, false),
        "\x04" => ("d", true, false),
        "\x05" => ("e", true, false),
        "\x06" => ("f", true, false),
        "\x0b" => ("k", true, false),
        "\x0c" => ("l", true, false),
        "\x0e" => ("n", true, false),
        "\x10" => ("p", true, false),
        "\x12" => ("r", true, false),
        "\x15" => ("u", true, false),
        "\x17" => ("w", true, false),
        "\x1bb" => ("b", false, true),
        "\x1bf" => ("f", false, true),
        "\x1b\x7f" | "\x1b\x08" => ("backspace", false, true),
        "\x1b" | "\x1b\x1b" | "\x1b[" => ("escape", false, false),
        other => (other, false, false),
    };
    Key {
        name: units(name),
        ctrl,
        alt,
    }
}
impl InteractiveSession {
    pub fn new(vault_id: Option<String>, name: Units) -> Self {
        Self {
            current_vault_id: vault_id,
            current_vault_name: name,
            ..Self::default()
        }
    }
    pub fn select_vault(&mut self, id: String, name: Units) {
        self.current_vault_id = Some(id);
        self.current_vault_name = name;
    }
    fn suggestions(&mut self, provider: &mut impl FnMut(&[u16]) -> Vec<Units>) {
        self.suggestions = provider(&self.input_buffer);
        self.suggestion_index = None;
    }
    pub fn finish_command(&mut self, mut provider: impl FnMut(&[u16]) -> Vec<Units>) {
        self.is_processing = false;
        self.input_buffer.clear();
        self.cursor = 0;
        self.history_index = None;
        self.suggestions(&mut provider);
    }
    fn accepted_suggestion(
        &mut self,
        index: usize,
        provider: &mut impl FnMut(&[u16]) -> Vec<Units>,
    ) {
        let Some(value) = self.suggestions.get(index).cloned() else {
            return;
        };
        self.input_buffer = value;
        if !ends(&self.input_buffer, "=") {
            self.input_buffer.push(b' ' as u16);
        }
        self.cursor = self.input_buffer.len();
        self.suggestions(provider);
    }
    fn search(&mut self) -> Result<(), UnicodeCaseError> {
        let query = lower_units(&self.search_query)?;
        self.search_matches.clear();
        for line in &self.history {
            let lower = lower_units(line)?;
            if query.is_empty() || lower.windows(query.len()).any(|window| window == query) {
                self.search_matches.push(line.clone());
            }
        }
        self.search_match_index = 0;
        Ok(())
    }
    fn command(&mut self, line: Units) -> InputOutcome {
        if equals(&line, "exit") || equals(&line, "quit") {
            return InputOutcome::Exit;
        }
        if self.history.first() != Some(&line) {
            self.history.insert(0, line.clone());
        }
        let mut argv = lex(&line);
        let command = if argv.first().is_some_and(|s| equals(s, "mnemosyne")) {
            let mut text = units("You are already in the Mnemosyne TUI. Commands can be typed directly, e.g. \"help\"");
            if argv.len() > 1 {
                text.extend(units("\nDid you mean: "));
                for (index, word) in argv[1..].iter().enumerate() {
                    if index > 0 {
                        text.push(b' ' as u16);
                    }
                    text.extend(word);
                }
            }
            Command::Message { text, error: false }
        } else if argv.first().is_some_and(|s| equals(s, "vault:open")) {
            match argv.get(1) {
                Some(name) => Command::OpenVault { name: name.clone() },
                None => Command::Message {
                    text: units("Missing vault name: vault:open <vault-name>"),
                    error: true,
                },
            }
        } else {
            let vault_selector = if argv.first().is_some_and(|s| starts(s, "vault=")) {
                Some(argv.remove(0)[6..].to_vec())
            } else {
                None
            };
            Command::Execute {
                vault_selector,
                argv,
            }
        };
        self.is_processing = true;
        InputOutcome::Execute(command)
    }
    /// Each socket data chunk is one native key event. A combined `text\r`
    /// chunk is text, not separate Return; do not silently improve this framing.
    pub fn input(
        &mut self,
        bytes: &[u8],
        mut provider: impl FnMut(&[u16]) -> Vec<Units>,
    ) -> Result<InputOutcome, UnicodeCaseError> {
        if self.is_processing {
            return Ok(InputOutcome::Continue);
        }
        let k = key(bytes);
        let name = &k.name;
        let is = |s| equals(name, s);
        if k.ctrl && (is("c") || is("d")) {
            self.suggestions.clear();
            self.suggestion_index = None;
            return Ok(InputOutcome::Exit);
        }
        if k.ctrl && is("l") {
            return Ok(InputOutcome::Continue);
        }
        if k.ctrl && is("r") {
            if self.is_search_mode {
                if !self.search_matches.is_empty() {
                    self.search_match_index =
                        (self.search_match_index + 1) % self.search_matches.len();
                }
            } else {
                self.is_search_mode = true;
                self.input_before_search = self.input_buffer.clone();
                self.search_query.clear();
                self.search_match_index = 0;
                self.search_matches = self.history.clone();
            }
            return Ok(InputOutcome::Continue);
        }
        if self.is_search_mode {
            if is("escape") {
                self.is_search_mode = false;
                self.input_buffer = self.input_before_search.clone();
                self.cursor = self.input_buffer.len();
                self.suggestions(&mut provider);
            } else if is("return") {
                self.is_search_mode = false;
                if let Some(line) = self.search_matches.get(self.search_match_index) {
                    self.input_buffer = line.clone();
                    if !ends(&self.input_buffer, " ") {
                        self.input_buffer.push(b' ' as u16);
                    }
                } else {
                    self.input_buffer = self.input_before_search.clone();
                }
                self.cursor = self.input_buffer.len();
                self.suggestions(&mut provider);
            } else if is("backspace") {
                if self.search_query.pop().is_some() {
                    self.search()?;
                }
            } else if name.len() == 1 && !k.ctrl {
                self.search_query.extend(name);
                self.search()?;
            }
            return Ok(InputOutcome::Continue);
        }
        if is("escape") {
            if !self.input_before_autocomplete.is_empty() {
                self.input_buffer = std::mem::take(&mut self.input_before_autocomplete);
                self.cursor = self.input_buffer.len();
                self.suggestions(&mut provider);
            } else if self.suggestion_index.is_some() {
                self.suggestion_index = None;
            } else if !self.input_buffer.is_empty() {
                self.input_buffer.clear();
                self.cursor = 0;
                self.history_index = None;
                self.suggestions(&mut provider);
            }
        } else if is("return") {
            if trim(&self.input_buffer).is_empty() && self.suggestion_index.is_none() {
                return Ok(InputOutcome::Continue);
            }
            if let Some(index) = self.suggestion_index {
                if let Some(value) = self.suggestions.get(index) {
                    if value != trim(&self.input_buffer) || !ends(&self.input_buffer, " ") {
                        self.input_before_autocomplete = self.input_buffer.clone();
                        self.accepted_suggestion(index, &mut provider);
                        return Ok(InputOutcome::Continue);
                    }
                }
            }
            let line = trim(&self.input_buffer).to_vec();
            self.suggestions.clear();
            self.suggestion_index = None;
            return Ok(self.command(line));
        } else if is("tab") {
            if !self.suggestions.is_empty() {
                if let Some(index) = self.suggestion_index {
                    self.input_before_autocomplete = self.input_buffer.clone();
                    self.accepted_suggestion(index, &mut provider);
                } else {
                    self.input_before_autocomplete = self.input_buffer.clone();
                    self.suggestion_index = Some(0);
                }
            }
        } else if is("shift-tab") {
            if self.suggestion_index.is_some() {
                if !self.input_before_autocomplete.is_empty() {
                    self.input_buffer = std::mem::take(&mut self.input_before_autocomplete);
                    self.cursor = self.input_buffer.len();
                }
                self.suggestion_index = None;
                self.suggestions(&mut provider);
            }
        } else if is("up") || (k.ctrl && is("p")) {
            if let Some(index) = self.suggestion_index {
                self.suggestion_index = Some(if index == 0 {
                    self.suggestions.len() - 1
                } else {
                    index - 1
                });
            } else if !self.history.is_empty()
                && self
                    .history_index
                    .is_none_or(|i| i < self.history.len() - 1)
            {
                let index = self.history_index.map_or(0, |i| i + 1);
                self.history_index = Some(index);
                self.input_buffer = self.history[index].clone();
                self.cursor = self.input_buffer.len();
                self.suggestions(&mut provider);
            }
        } else if is("down") || (k.ctrl && is("n")) {
            if let Some(index) = self.suggestion_index {
                self.suggestion_index = Some((index + 1) % self.suggestions.len());
            } else if let Some(index) = self.history_index {
                if index > 0 {
                    self.history_index = Some(index - 1);
                    self.input_buffer = self.history[index - 1].clone();
                } else {
                    self.history_index = None;
                    self.input_buffer.clear();
                }
                self.cursor = self.input_buffer.len();
                self.suggestions(&mut provider);
            } else if !self.suggestions.is_empty() {
                self.input_before_autocomplete = self.input_buffer.clone();
                self.suggestion_index = Some(0);
            }
        } else if is("backspace") {
            if self.cursor > 0 {
                self.input_buffer.remove(self.cursor - 1);
                self.cursor -= 1;
                self.suggestions(&mut provider);
            }
        } else if is("delete") {
            if self.cursor < self.input_buffer.len() {
                self.input_buffer.remove(self.cursor);
                self.suggestions(&mut provider);
            }
        } else if is("left") || (k.ctrl && is("b")) {
            self.cursor = self.cursor.saturating_sub(1);
        } else if is("right") || (k.ctrl && is("f")) {
            if self.cursor == self.input_buffer.len() && !self.suggestions.is_empty() {
                let index = self.suggestion_index.unwrap_or(0);
                self.input_before_autocomplete.clear();
                self.accepted_suggestion(index, &mut provider);
            } else {
                self.cursor = (self.cursor + 1).min(self.input_buffer.len());
            }
        } else if k.ctrl && is("a") {
            self.cursor = 0;
        } else if k.ctrl && is("e") {
            self.cursor = self.input_buffer.len();
        } else if k.ctrl && is("u") {
            self.input_buffer.drain(..self.cursor);
            self.cursor = 0;
            self.suggestions(&mut provider);
        } else if k.ctrl && is("k") {
            self.input_buffer.truncate(self.cursor);
            self.suggestions(&mut provider);
        } else if (k.ctrl && is("w")) || (k.alt && is("backspace")) {
            if self.cursor > 0 {
                let prefix = trim_end(&self.input_buffer[..self.cursor]);
                let keep = prefix
                    .iter()
                    .rposition(|unit| *unit == b' ' as u16)
                    .map_or(0, |i| i + 1);
                self.input_buffer.drain(keep..self.cursor);
                self.cursor = keep;
                self.suggestions(&mut provider);
            }
        } else if k.alt && is("b") {
            let prefix = trim_end(&self.input_buffer[..self.cursor]);
            self.cursor = prefix
                .iter()
                .rposition(|unit| *unit == b' ' as u16)
                .map_or(0, |i| i + 1);
        } else if k.alt && is("f") {
            while self.cursor < self.input_buffer.len()
                && whitespace(self.input_buffer[self.cursor])
            {
                self.cursor += 1;
            }
            while self.cursor < self.input_buffer.len()
                && !whitespace(self.input_buffer[self.cursor])
            {
                self.cursor += 1;
            }
        } else if !k.ctrl && !k.alt && !starts(name, "\x1b") {
            self.input_buffer
                .splice(self.cursor..self.cursor, name.iter().copied());
            self.cursor += name.len();
            self.suggestions(&mut provider);
        }
        Ok(InputOutcome::Continue)
    }
}
