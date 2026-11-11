//! Daily-note getter recovered from the pinned internal daily-notes plugin.
//! Formatting uses the independent Moment-compatible Rust token/locale layer.
use mnemosyne_core::CONFIG_DIR;
use crate::{Engine, EngineError};
use serde_json::Value;
use time::OffsetDateTime;

#[derive(Debug, thiserror::Error)]
pub enum DailyError {
    #[error("Daily notes plugin is not enabled.")]
    Disabled,
    #[error("Failed to get daily note.")]
    Failed,
    #[error(
        "Daily note format token {0:?} needs locale/format compatibility that remains pending."
    )]
    PendingFormat(String),
    #[error("Failed to format daily note date.")]
    Clock,
    #[error("Daily note path {0:?} needs an unverified path contract.")]
    PendingPath(String),
}

pub(crate) fn now() -> Result<OffsetDateTime, DailyError> {
    OffsetDateTime::now_local().map_err(|_| DailyError::Clock)
}

pub(crate) fn get_daily_note(
    engine: &mut Engine,
    now: OffsetDateTime,
) -> Result<String, EngineError> {
    if !plugin_enabled(engine, "daily-notes", true)? {
        return Err(DailyError::Disabled.into());
    }
    let options = read_options(engine, "daily-notes");
    let format = options
        .get("format")
        .and_then(Value::as_str)
        .filter(|format| !format.is_empty())
        .unwrap_or("YYYY-MM-DD");
    let name = crate::moment::format(now, format, engine.locale())?
        .trim_matches(mnemosyne_core::vault_search::js_whitespace)
        .to_owned();
    let folder = if let Some(folder) = options
        .get("folder")
        .and_then(Value::as_str)
        .filter(|folder| !folder.is_empty())
    {
        let normalized = mnemosyne_core::normalize_path(folder);
        if !is_folder(engine, &normalized) {
            return Err(DailyError::Failed.into());
        }
        normalized
    } else {
        // getNewFileParent("") never uses active-file location. Invalid configured
        // newFileFolderPath falls back to the root.
        if engine.get_config("newFileLocation").as_str() == Some("folder") {
            let folder = engine
                .get_config("newFileFolderPath")
                .as_str()
                .unwrap_or("/");
            let normalized = mnemosyne_core::normalize_path(folder);
            if is_folder(engine, &normalized) {
                normalized
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    };
    let candidate = join(&folder, &format!("{name}.md"));
    if let Some(existing) = engine
        .vault
        .files()
        .iter()
        .find(|path| path.to_lowercase() == candidate.to_lowercase())
    {
        return Ok(existing.clone());
    }
    if engine.vault.root.join(&candidate).is_dir() {
        return Err(DailyError::Failed.into());
    }
    let mut content = String::new();
    if let Some(template) = options
        .get("template")
        .and_then(Value::as_str)
        .filter(|template| !template.is_empty())
    {
        let mut path = mnemosyne_core::normalize_path(template);
        if mnemosyne_core::extension(&path).is_empty() {
            path.push_str(".md");
        }
        if !engine.vault.has_file(&path) {
            return Err(DailyError::Failed.into());
        }
        let text =
            crate::read_text(&engine.vault.root.join(&path)).map_err(|_| DailyError::Failed)?;
        content = render_template(
            text.strip_prefix('\u{feff}').unwrap_or(&text),
            &name,
            now,
            "YYYY-MM-DD",
            "HH:mm",
            engine.locale(),
        )?;
    }
    // createNewMarkdownFile strips an existing .md suffix from the date title.
    // This is independent of the preceding lookup (which appends .md directly).
    let title = if mnemosyne_core::extension(&name) == "md" {
        mnemosyne_core::strip_extension(&name)
    } else {
        name.clone()
    };
    let new_path = mnemosyne_core::normalize_path(&join(&folder, &format!("{title}.md")));
    if new_path.split('/').any(|part| part == ".." || part == ".") {
        return Err(DailyError::PendingPath(new_path).into());
    }
    if new_path.contains(':') {
        return Err(DailyError::Failed.into());
    }
    let parent = mnemosyne_core::parent(&new_path);
    let mut missing_folders = Vec::new();
    let mut prefix = String::new();
    for part in parent.split('/').filter(|part| !part.is_empty()) {
        prefix = join(&prefix, part);
        if !engine.vault.root.join(&prefix).is_dir() {
            missing_folders.push(prefix.clone());
        }
    }
    let created = engine
        .vault
        .create(&new_path, &content)
        .map_err(|_| DailyError::Failed)?;
    for folder in missing_folders {
        engine.file_created(&folder);
    }
    engine.file_created(&created);
    // The pinned handler observes no file cache in the creation frame; the
    // metadata changed/resolve events follow asynchronously. Until the event bus
    // is implemented, explicit engine refresh is the readiness boundary.
    engine.cached_notes.remove(&created);
    Ok(created)
}

pub(crate) fn plugin_enabled(
    engine: &mut Engine,
    id: &str,
    _default_on: bool,
) -> Result<bool, EngineError> {
    Ok(engine.is_core_plugin_enabled(id))
}
pub(crate) fn read_options(engine: &mut Engine, name: &str) -> Value {
    let path = engine.vault.root.join(CONFIG_DIR).join(format!("{name}.json"));
    match std::fs::read(&path).map(|bytes| String::from_utf8_lossy(&bytes).into_owned()) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(error) => {
                engine
                    .config_diagnostics
                    .push(format!("failed to read JSON {name}.json: {error}"));
                Value::Null
            }
        },
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Value::Null,
        Err(error) => {
            engine
                .config_diagnostics
                .push(format!("failed to read JSON {name}.json: {error}"));
            Value::Null
        }
    }
}
fn join(folder: &str, name: &str) -> String {
    if folder.is_empty() {
        name.into()
    } else {
        format!("{folder}/{name}")
    }
}

/// Format using the default English locale. Engine operations use their active
/// locale through the same independent formatter.
pub fn format_moment(now: OffsetDateTime, format: &str) -> Result<String, DailyError> {
    crate::moment::format(now, format, "en")
}
#[cfg(test)]
fn substitute_template(text: &str, title: &str, now: OffsetDateTime) -> Result<String, DailyError> {
    render_template(text, title, now, "YYYY-MM-DD", "HH:mm", "en")
}
pub(crate) fn render_template(
    text: &str,
    title: &str,
    now: OffsetDateTime,
    date_format: &str,
    time_format: &str,
    locale: &str,
) -> Result<String, DailyError> {
    let text = replace_title(text, title);
    let mut result = String::new();
    let mut remaining = text.as_str();
    while let Some(start) = remaining.find("{{") {
        result.push_str(&remaining[..start]);
        let rest = &remaining[start + 2..];
        let Some(end) = rest.find("}}") else {
            result.push_str(&remaining[start..]);
            return Ok(result);
        };
        let expression = &rest[..end];
        let (kind, format) = expression
            .split_once(':')
            .map(|(kind, format)| (kind, Some(format)))
            .unwrap_or((expression, None));
        if expression
            .chars()
            .any(|ch| matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}'))
        {
            result.push_str(&remaining[start..start + 2 + end + 2]);
            remaining = &rest[end + 2..];
            continue;
        }
        match kind.to_ascii_lowercase().as_str() {
            "date" => result.push_str(&crate::moment::format(
                now,
                format
                    .filter(|format| !format.is_empty())
                    .unwrap_or(date_format),
                locale,
            )?),
            "time" => result.push_str(&crate::moment::format(
                now,
                format
                    .filter(|format| !format.is_empty())
                    .unwrap_or(time_format),
                locale,
            )?),
            _ => result.push_str(&remaining[start..start + 2 + end + 2]),
        }
        remaining = &rest[end + 2..];
    }
    result.push_str(remaining);
    Ok(result)
}

fn replace_title(text: &str, title: &str) -> String {
    const NEEDLE: &[u8] = b"{{title}}";
    let mut output = String::new();
    let mut offset = 0;
    while let Some(relative) = text.as_bytes()[offset..]
        .windows(NEEDLE.len())
        .position(|window| window.eq_ignore_ascii_case(NEEDLE))
    {
        let start = offset + relative;
        let end = start + NEEDLE.len();
        output.push_str(&text[offset..start]);
        let mut replacement = title;
        while let Some(dollar) = replacement.find('$') {
            output.push_str(&replacement[..dollar]);
            let rest = &replacement[dollar + 1..];
            match rest.as_bytes().first().copied() {
                Some(b'$') => {
                    output.push('$');
                    replacement = &rest[1..];
                }
                Some(b'&') => {
                    output.push_str(&text[start..end]);
                    replacement = &rest[1..];
                }
                Some(b'`') => {
                    output.push_str(&text[..start]);
                    replacement = &rest[1..];
                }
                Some(b'\'') => {
                    output.push_str(&text[end..]);
                    replacement = &rest[1..];
                }
                _ => {
                    output.push('$');
                    replacement = rest;
                }
            }
        }
        output.push_str(replacement);
        offset = end;
    }
    output.push_str(&text[offset..]);
    output
}

pub(crate) fn is_folder(engine: &Engine, path: &str) -> bool {
    (path.is_empty() || (!mnemosyne_core::has_hidden_ancestor(path) && crate::safe_relative(path)))
        && std::fs::symlink_metadata(engine.vault.root.join(path))
            .is_ok_and(|metadata| metadata.file_type().is_dir())
}

#[cfg(test)]
mod tests {
    use super::*;
    use time::{Date, Month, UtcOffset};
    fn pinned() -> OffsetDateTime {
        Date::from_calendar_date(2026, Month::October, 14)
            .unwrap()
            .with_hms_milli(16, 8, 7, 123)
            .unwrap()
            .assume_utc()
    }
    #[test]
    fn numeric_moment_tokens_use_calendar_and_timezone_not_rust_strftime() {
        let now = pinned().to_offset(UtcOffset::from_hms(-7, 0, 0).unwrap());
        assert_eq!(
            format_moment(
                now,
                "YYYY/MM/DD [at] HH:mm:ss.SSS ZZ [Q]Q [day]DDD [week]GGGG-WW"
            )
            .unwrap(),
            "2026/10/14 at 09:08:07.123 -0700 Q4 day287 week2026-42"
        );
        assert_eq!(
            format_moment(now, r"[literal YYYY] \Y YY M D H h k mm ss S SS SSSSS Z").unwrap(),
            "literal YYYY Y 26 10 14 9 9 9 08 07 1 12 12300 -07:00"
        );
        let boundary = Date::from_calendar_date(2021, Month::January, 1)
            .unwrap()
            .midnight()
            .assume_utc();
        assert_eq!(
            format_moment(boundary, "YYYY [ISO] GGGG-WW d E HH hh kk").unwrap(),
            "2021 ISO 2020-53 5 5 00 12 24"
        );
        assert_eq!(
            format_moment(now, "MMMM Do LLLL").unwrap(),
            "October 14th Wednesday, October 14, 2026 9:08 AM"
        );
        assert_eq!(format_moment(now, "Qo").unwrap(), "4th");
    }
    #[test]
    fn template_variables_respect_case_literal_title_and_numeric_date_overrides() {
        assert_eq!(
            substitute_template(
                "{{title}} {{TITLE}} {{date}} {{TIME}} {{date:YYYY/MM/DD}} {{unknown}}",
                "Daily-14",
                pinned()
            )
            .unwrap(),
            "Daily-14 Daily-14 2026-10-14 16:08 2026/10/14 {{unknown}}"
        );
    }
}
