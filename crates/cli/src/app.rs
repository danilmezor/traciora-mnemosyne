//! Application request dispatch over a retained engine/session.
//! Command coverage is incremental; unsupported requests return an explicit
//! application error.
use crate::search::{self, Params};
use mnemosyne_engine::{Engine, FileTarget, OpenOptions};

fn value<'a>(params: &'a Params, key: &str) -> Option<&'a str> {
    params
        .get(key)
        .and_then(|v| v.as_deref())
        .filter(|v| !v.is_empty())
}
fn flag(params: &Params, key: &str) -> bool {
    params
        .get(key)
        .is_some_and(|v| v.as_ref().is_none_or(|s| !s.is_empty()))
}

pub enum CommandExecution {
    Complete(String),
    Pending(PendingCommand),
}
pub struct PendingCommand {
    rename: crate::file_manager::PendingRename,
    copy_requested: bool,
}
impl PendingCommand {
    pub fn operation_id(&self) -> mnemosyne_engine::file_operations::FileOperationId {
        self.rename.id
    }
    pub fn abandon(&self, engine: &mut Engine) {
        engine.abandon_file_operation_result(self.rename.id);
    }
}

pub fn begin(engine: &mut Engine, argv: &[String]) -> CommandExecution {
    let (prepared, handler) = match prepare(engine, argv, false) {
        Ok(prepared) => prepared,
        Err(output) => return CommandExecution::Complete(output),
    };
    begin_builtin(engine, &prepared, &handler)
}
pub fn poll_pending(engine: &mut Engine, pending: &PendingCommand) -> Option<String> {
    let result = engine.take_file_operation_result(pending.rename.id)?;
    Some(render_builtin_result(
        pending.rename.complete(engine, result),
        pending.copy_requested,
    ))
}
/// Blocking convenience, never used by retained event-loop dispatch. A genuine
/// link-update decision remains pending until the application decides or the
/// caller/process is interrupted; no terminal choice is invented here.
pub fn execute(engine: &mut Engine, argv: &[String]) -> String {
    let execution = begin(engine, argv);
    drive(engine, execution)
}
pub fn drive(engine: &mut Engine, execution: CommandExecution) -> String {
    match execution {
        CommandExecution::Complete(output) => output,
        CommandExecution::Pending(pending) => loop {
            let _ = engine.poll_metadata_lifecycle();
            engine.poll_file_operations();
            if let Some(output) = poll_pending(engine, &pending) {
                break output;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        },
    }
}

/// Use the same retained command registry and argument preparation with the
/// application's real script service. Eval framing belongs to the runtime;
/// do not wrap a native framework error in a second Error prefix.
#[cfg(unix)]
pub fn begin_script(
    application: &mut mnemosyne_engine::script_application::ScriptApplication,
    argv: &[String],
) -> CommandExecution {
    let (prepared, handler) = match prepare(application.engine_mut(), argv, true) {
        Ok(prepared) => prepared,
        Err(output) => return CommandExecution::Complete(output),
    };
    if handler == "builtin:eval" {
        let code = prepared
            .params
            .get("code")
            .and_then(|value| value.as_ref())
            .map(|code| code.encode_utf16().collect());
        return CommandExecution::Complete(match application.evaluate(code) {
            Ok(outcome) => {
                if prepared.copy_requested && !outcome.body.is_empty() {
                    if let Err(error) = copy_to_clipboard(&outcome.body) {
                        return CommandExecution::Complete(crate::error::render(&error));
                    }
                }
                outcome.pipe_output()
            }
            Err(error) => crate::error::render(&error.into()),
        });
    }
    begin_builtin(application.engine_mut(), &prepared, &handler)
}
#[cfg(unix)]
pub fn execute_script(
    application: &mut mnemosyne_engine::script_application::ScriptApplication,
    argv: &[String],
) -> String {
    let execution = begin_script(application, argv);
    drive(application.engine_mut(), execution)
}

fn prepare(
    engine: &mut Engine,
    argv: &[String],
    script_runtime: bool,
) -> Result<(crate::registry::PreparedCommand, String), String> {
    let argv = match argv.first().and_then(|arg| arg.strip_prefix("vault=")) {
        Some(vault) => {
            let root = engine.vault.root.to_string_lossy();
            let name = engine
                .vault
                .root
                .file_name()
                .unwrap_or_default()
                .to_string_lossy();
            if !vault.is_empty() && vault != root && vault != name {
                return Err(
                    "Error: Vault selection is not supported by this single-vault engine.\n".into(),
                );
            }
            &argv[1..]
        }
        None => argv,
    };
    let (command, params) = crate::args::parse_argv(argv);
    if let Err(error) = engine.refresh_vault() {
        return Err(crate::error::render(&error.into()));
    }
    let _ = engine.poll_metadata_cache_gc();
    if let Err(error) = sync_builtin_registry_with_runtime(engine, script_runtime) {
        return Err(crate::registry::frame_error(&error));
    }
    let prepared = match engine.commands.prepare_params(&command, params) {
        Ok(prepared) => prepared,
        Err(error) => return Err(crate::registry::frame_error(&error)),
    };
    let handler = engine
        .commands
        .get(&prepared.command)
        .unwrap()
        .handler
        .clone();
    Ok((prepared, handler))
}

fn begin_builtin(
    engine: &mut Engine,
    prepared: &crate::registry::PreparedCommand,
    handler: &str,
) -> CommandExecution {
    if matches!(handler, "builtin:move" | "builtin:rename") {
        return match crate::file_manager::begin(engine, &prepared.params, handler == "builtin:move")
        {
            Ok(rename) => CommandExecution::Pending(PendingCommand {
                rename,
                copy_requested: prepared.copy_requested,
            }),
            Err(error) => CommandExecution::Complete(crate::error::render(&error)),
        };
    }
    CommandExecution::Complete(execute_builtin(engine, prepared, handler))
}
fn render_builtin_result(result: anyhow::Result<String>, copy_requested: bool) -> String {
    match result {
        Ok(output) => {
            if copy_requested && !output.is_empty() {
                if let Err(error) = copy_to_clipboard(&output) {
                    return crate::error::render(&error);
                }
            }
            crate::registry::frame_output(&output)
        }
        Err(error) => crate::error::render(&error),
    }
}

fn execute_builtin(
    engine: &mut Engine,
    prepared: &crate::registry::PreparedCommand,
    handler: &str,
) -> String {
    let Some(command) = handler.strip_prefix("builtin:") else {
        return "Error: The registered handler requires a plugin runtime that is not implemented yet.\n".into();
    };
    render_builtin_result(
        run(engine, command, &prepared.params),
        prepared.copy_requested,
    )
}

/// This synchronizes the implemented built-in subset from retained settings. Full
/// asynchronous plugin activation and policy handling remain engine work.
pub fn sync_builtin_registry(engine: &mut Engine) -> Result<(), crate::registry::RegistryError> {
    sync_builtin_registry_with_runtime(engine, false)
}
fn sync_builtin_registry_with_runtime(
    engine: &mut Engine,
    script_runtime: bool,
) -> Result<(), crate::registry::RegistryError> {
    for metadata in crate::builtins::metadata_catalog().commands() {
        if !(script_runtime && metadata.name == "eval")
            && !matches!(
                metadata.name.as_str(),
                "help"
                    | "__completions"
                    | "version"
                    | "open"
                    | "read"
                    | "recents"
                    | "tabs"
                    | "search"
                    | "search:context"
                    | "task"
                    | "tasks"
                    | "create"
                    | "append"
                    | "prepend"
                    | "rename"
                    | "move"
                    | "backlinks"
                    | "links"
                    | "unresolved"
                    | "orphans"
                    | "deadends"
                    | "properties"
                    | "property:read"
                    | "property:set"
                    | "property:remove"
                    | "wordcount"
                    | "delete"
                    | "vault"
                    | "files"
                    | "folders"
                    | "folder"
                    | "file"
                    | "tags"
                    | "tag"
                    | "aliases"
                    | "outline"
                    | "bookmarks"
                    | "bookmark"
            )
        {
            continue;
        }
        let id = format!("builtin:{}", metadata.name);
        let available = metadata.available(true, |owner| match owner {
            "global-search" | "outgoing-link" | "word-count" | "outline" | "bookmarks" => {
                engine.is_core_plugin_enabled(owner)
            }
            _ => false,
        });
        if available {
            if engine.commands.get(&metadata.name).is_none() {
                engine
                    .commands
                    .register(&metadata.name, metadata.spec.clone(), id)?;
            }
        } else {
            engine
                .commands
                .unregister_if(&metadata.name, |handler| handler == &id);
        }
    }
    Ok(())
}

fn copy_to_clipboard(text: &str) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::io::Write;
        use std::process::{Command, Stdio};
        let mut child = Command::new("/usr/bin/pbcopy")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(text.as_bytes())?;
        let output = child.wait_with_output()?;
        if !output.status.success() {
            anyhow::bail!(
                "Clipboard write failed: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    anyhow::bail!("Clipboard integration is not implemented on this platform.")
}

fn run(engine: &mut Engine, command: &str, params: &Params) -> anyhow::Result<String> {
    let target = FileTarget {
        file: value(params, "file"),
        path: value(params, "path"),
    };
    match command {
        "version" => Ok(crate::version::response()),
        "bookmarks" => crate::bookmarks::list(engine, params),
        "bookmark" => crate::bookmarks::add(engine, params),
        "wordcount" => crate::wordcount::execute(engine, params),
        "delete" => crate::deletion::execute(engine, params),
        "file" => crate::file_info::execute(engine, params),
        "tags" | "tag" | "aliases" | "outline" => {
            crate::metadata_commands::execute(engine, params, command)
        }
        "vault" | "files" | "folders" | "folder" => {
            crate::inventory::execute(engine, params, command)
        }
        "help" => {
            let collator =
                mnemosyne_core::collation::NaturalCollator::new(engine.collation_locale())?;
            Ok(engine
                .commands
                .help_with(params, |left, right| collator.compare(left, right)))
        }
        "__completions" => Ok(serde_json::to_string(&engine.commands.completions())?),
        "open" => {
            let path = engine.open_file(
                target,
                OpenOptions {
                    new_tab: flag(params, "newtab"),
                },
            )?;
            engine.persist_session()?;
            Ok(format!("Opened: {path}"))
        }
        "read" => {
            let text = engine.read_saved(target)?;
            Ok(text.strip_prefix('\u{feff}').unwrap_or(&text).to_string())
        }
        "recents" => {
            if flag(params, "total") {
                Ok(engine.recents().len().to_string())
            } else if engine.recents().is_empty() {
                Ok("No recent files.".into())
            } else {
                Ok(engine.recents().join("\n"))
            }
        }
        "tabs" => {
            let rows: Vec<String> = engine
                .session()
                .leaves
                .iter()
                .map(|leaf| {
                    let title = leaf
                        .file
                        .as_deref()
                        .and_then(|file| std::path::Path::new(file).file_stem())
                        .and_then(|stem| stem.to_str())
                        .unwrap_or("New tab");
                    format!("[{}] {title}", leaf.view_type)
                })
                .collect();
            Ok(rows.join("\n"))
        }
        "search" | "search:context" => {
            if !search_enabled(engine) {
                return Err(anyhow::anyhow!(
                    "Command \"{command}\" not found. It may require a plugin to be enabled."
                ));
            }
            Ok(search::response_in_realm(
                engine
                    .ordered_notes_with_cache()
                    .filter(|(note, _)| !engine.is_user_ignored(&note.path.to_string_lossy())),
                &engine.search_regex_realm,
                params,
                command == "search:context",
            ))
        }
        "task" | "tasks" => Ok(crate::tasks::execute(engine, params, command == "task")?),
        "create" => Ok(crate::create::execute(engine, params)?),
        "append" | "prepend" => crate::text_edit::execute(engine, params, command == "prepend"),
        "rename" | "move" => crate::file_manager::execute(engine, params, command == "move"),
        "backlinks" | "links" | "unresolved" | "orphans" | "deadends" => {
            crate::graph_commands::execute(engine, params, command)
        }
        "properties" | "property:read" | "property:set" | "property:remove" => {
            crate::properties::execute(engine, params, command)
        }
        _ => Err(anyhow::anyhow!(
            "Command \"{command}\" not found. It may require a plugin to be enabled."
        )),
    }
}

pub fn search_enabled(engine: &Engine) -> bool {
    engine.is_core_plugin_enabled("global-search")
}
