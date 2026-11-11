//! CLI snapshot compatibility helpers; shared argument semantics live in the engine.
pub use mnemosyne_engine::command_args::*;
use std::{borrow::Cow, sync::OnceLock};

fn inventory() -> &'static serde_json::Value {
    static INVENTORY: OnceLock<serde_json::Value> = OnceLock::new();
    INVENTORY.get_or_init(|| {
        serde_json::from_str(include_str!("../data/commands.json"))
            .expect("command inventory must be valid JSON")
    })
}

/// Compatibility fallback for current built-ins. Dynamic dispatch should use
/// CommandRegistry, whose live metadata distinguishes absent and empty flags.
pub fn flag_alias<'a>(command: &'a str, params: &mut Params) -> Cow<'a, str> {
    let commands = inventory()["commands"].as_array().unwrap();
    if commands.iter().any(|entry| entry["command"] == command) {
        return Cow::Borrowed(command);
    }
    if let Some((base, flag)) = command.rsplit_once(':') {
        if commands
            .iter()
            .any(|entry| entry["command"] == base && entry["flags"].get(flag).is_some())
        {
            assign(params, flag, "true");
            normalize_params(params);
            return Cow::Owned(base.to_owned());
        }
    }
    Cow::Borrowed(command)
}
/// Existing caller API; the snapshot does not define live plugin registrations.
pub fn prepare_command<'a>(command: &'a str, params: &mut Params) -> Cow<'a, str> {
    normalize_params(params);
    let command = if command.is_empty() || command == "--help" {
        Cow::Borrowed("help")
    } else {
        flag_alias(command, params)
    };
    let formats = inventory()["commands"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["command"] == command.as_ref())
        .and_then(|entry| entry["flags"]["format"]["value"].as_str());
    apply_format_aliases(params, formats);
    command
}
