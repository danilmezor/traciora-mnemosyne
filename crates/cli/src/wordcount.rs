use crate::search::Params;
use mnemosyne_engine::{Engine, FileTarget};
pub fn execute(engine: &Engine, params: &Params) -> anyhow::Result<String> {
    let value = |key: &str| params.get(key).and_then(|v| v.as_deref());
    let path = engine.resolve_target(
        FileTarget {
            file: value("file"),
            path: value("path"),
        },
        true,
    )?;
    if mnemosyne_core::extension(mnemosyne_core::basename(&path)) != "md" {
        anyhow::bail!("Word count is only available for markdown files.");
    }
    let text = engine.read_saved(FileTarget::path(&path))?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let counts = mnemosyne_core::wordcount::count(text);
    let truthy = |key: &str| {
        params
            .get(key)
            .is_some_and(|v| v.as_ref().is_none_or(|s| !s.is_empty()))
    };
    let words = truthy("words");
    let characters = truthy("characters");
    Ok(match (words, characters) {
        (true, false) => counts.words.to_string(),
        (false, true) => counts.characters.to_string(),
        _ => format!("words: {}\ncharacters: {}", counts.words, counts.characters),
    })
}
