//! Pinned literal metadata catalog. Loading the catalog does not register
//! commands: applications must select implemented handlers and enabled owners.
use crate::registry::{CommandRegistry, CommandSpec, FlagSpec, RegistryError};
use indexmap::IndexMap;
use serde_json::Value;
use std::sync::OnceLock;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Core,
    /// Reference desktop/Electron guard; implemented headless handlers may map
    /// this to their own available backend capabilities.
    Desktop,
    CorePlugin(String),
}
#[derive(Debug, Clone)]
pub struct CommandMetadata {
    pub name: String,
    pub spec: CommandSpec,
    pub availability: Availability,
    pub desktop_required: bool,
    /// Caller must include policy permission in its loaded/enabled owner predicate.
    pub policy_feature: Option<String>,
    pub plugin_default_on: Option<bool>,
    pub source_declaration_index: usize,
}
impl CommandMetadata {
    pub fn available(&self, desktop_backend: bool, enabled_plugin: impl Fn(&str) -> bool) -> bool {
        if self.desktop_required && !desktop_backend {
            return false;
        }
        match &self.availability {
            Availability::Core => true,
            Availability::Desktop => desktop_backend,
            Availability::CorePlugin(id) => enabled_plugin(id),
        }
    }
}
#[derive(Debug)]
pub struct MetadataCatalog {
    commands: Vec<CommandMetadata>,
}
impl MetadataCatalog {
    pub fn commands(&self) -> &[CommandMetadata] {
        &self.commands
    }
    pub fn get(&self, name: &str) -> Option<&CommandMetadata> {
        self.commands.iter().find(|c| c.name == name)
    }
    /// Register only available entries for which the caller supplies an
    /// implemented handler. Unknown/unimplemented entries are never advertised.
    /// A freshly constructed registry uses source declaration order; retained
    /// engines should register/unregister entries at actual lifecycle boundaries.
    pub fn register_implemented<H>(
        &self,
        registry: &mut CommandRegistry<H>,
        desktop_backend: bool,
        enabled_plugin: impl Fn(&str) -> bool,
        mut implemented_handler: impl FnMut(&CommandMetadata) -> Option<H>,
    ) -> Result<Vec<String>, RegistryError> {
        let mut registered = Vec::new();
        for command in &self.commands {
            if command.available(desktop_backend, &enabled_plugin) {
                if let Some(handler) = implemented_handler(command) {
                    registry.register(&command.name, command.spec.clone(), handler)?;
                    registered.push(command.name.clone());
                }
            }
        }
        Ok(registered)
    }
    /// Add just one enabled plugin's implemented entries to a retained registry.
    /// Caller owns async activation and matching unregister callbacks.
    pub fn register_plugin<H>(
        &self,
        registry: &mut CommandRegistry<H>,
        plugin: &str,
        mut implemented_handler: impl FnMut(&CommandMetadata) -> Option<H>,
    ) -> Result<Vec<String>, RegistryError> {
        let mut registered = Vec::new();
        for command in &self.commands {
            if command.availability == Availability::CorePlugin(plugin.into()) {
                if let Some(handler) = implemented_handler(command) {
                    registry.register(&command.name, command.spec.clone(), handler)?;
                    registered.push(command.name.clone());
                }
            }
        }
        Ok(registered)
    }
}

pub fn metadata_catalog() -> &'static MetadataCatalog {
    static CATALOG: OnceLock<MetadataCatalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let snapshot: Value =
            serde_json::from_str(include_str!("../data/command_metadata.json"))
                .expect("pinned command metadata must be valid JSON");
        let commands = snapshot["commands"]
            .as_array()
            .expect("metadata commands array")
            .iter()
            .map(|entry| {
                let flags = entry["flags"].as_object().map(|flags| {
                    flags
                        .iter()
                        .map(|(name, metadata)| {
                            (
                                name.clone(),
                                FlagSpec::from_json(
                                    metadata.as_object().expect("flag metadata object").clone(),
                                ),
                            )
                        })
                        .collect::<IndexMap<_, _>>()
                });
                let availability = match entry["availability"]["kind"].as_str().unwrap() {
                    "core" => Availability::Core,
                    "desktop" => Availability::Desktop,
                    "core_plugin" => Availability::CorePlugin(
                        entry["availability"]["owner_plugin"]
                            .as_str()
                            .unwrap()
                            .into(),
                    ),
                    other => panic!("unknown availability: {other}"),
                };
                CommandMetadata {
                    name: entry["command"].as_str().unwrap().into(),
                    spec: CommandSpec::new(entry["description"].as_str().unwrap(), flags),
                    availability,
                    desktop_required: entry["availability"]["desktop_required"]
                        .as_bool()
                        .unwrap_or(false),
                    policy_feature: entry["availability"]["policy_feature"]
                        .as_str()
                        .map(str::to_owned),
                    plugin_default_on: entry["availability"]["plugin_default_on"].as_bool(),
                    source_declaration_index: entry["source_declaration_index"].as_u64().unwrap()
                        as usize,
                }
            })
            .collect();
        MetadataCatalog { commands }
    })
}
