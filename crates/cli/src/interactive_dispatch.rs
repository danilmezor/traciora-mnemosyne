//! Per-socket nonvisual terminal actors over the ordinary retained application.
//! Prompt/ANSI drawing is deferred with UI. Input ownership, command execution,
//! history and vault selection use the recovered main-process contracts.
use crate::{
    app::{CommandExecution, PendingCommand},
    host_dispatch::HostCommandExecution,
};
use mnemosyne_engine::{
    host::VaultHost,
    interactive::{Command, CompletionCatalog, InputOutcome, InteractiveSession, Units},
    server::{CliRequest, ConnectionId, InputPolicy, StreamAction, StreamEvent},
    vault_registry::{CliDispatchAction, CLI_DISABLED, VAULT_NOT_FOUND},
    Engine,
};
use std::{collections::BTreeMap, path::Path};
struct PendingExecution {
    command: PendingCommand,
    vault_id: Option<String>,
    interactive: bool,
}
fn complete(output: String) -> HostCommandExecution {
    HostCommandExecution {
        execution: CommandExecution::Complete(output),
        vault_id: None,
    }
}
fn waiting() -> StreamAction {
    StreamAction {
        input_policy: InputPolicy::WaitForCompletion,
        ..StreamAction::default()
    }
}

struct Actor {
    session: InteractiveSession,
    // Native initCompletions captures these once, including across vault:open.
    catalog: CompletionCatalog,
}
#[derive(Default)]
pub struct InteractiveDispatch {
    actors: BTreeMap<ConnectionId, Actor>,
    pending: BTreeMap<ConnectionId, PendingExecution>,
}
impl InteractiveDispatch {
    pub fn engine(
        &mut self,
        engine: &mut Engine,
        id: ConnectionId,
        event: StreamEvent,
    ) -> StreamAction {
        self.event(&mut SingleEngine(engine), id, event)
    }
    pub fn host(
        &mut self,
        host: &mut VaultHost,
        id: ConnectionId,
        event: StreamEvent,
    ) -> StreamAction {
        self.event(host, id, event)
    }
    pub fn script(
        &mut self,
        application: &mut mnemosyne_engine::script_application::ScriptApplication,
        id: ConnectionId,
        event: StreamEvent,
    ) -> StreamAction {
        self.event(application, id, event)
    }
    fn event(
        &mut self,
        backend: &mut impl Backend,
        id: ConnectionId,
        event: StreamEvent,
    ) -> StreamAction {
        match event {
            StreamEvent::Request(request) => {
                let Some(mut session) = backend.start(&request) else {
                    let result = backend.dispatch(&request);
                    return self.accept_result(id, result, false);
                };
                let catalog = backend.catalog(session.current_vault_id.as_deref());
                session.finish_command(|input| catalog.suggestions(input));
                self.actors.insert(id, Actor { session, catalog });
                StreamAction::default()
            }
            StreamEvent::Poll => {
                let Some(pending) = self.pending.get(&id) else {
                    return StreamAction::default();
                };
                let Some(engine) = backend.operation_engine(pending.vault_id.as_deref()) else {
                    return waiting();
                };
                let Some(output) = crate::app::poll_pending(engine, &pending.command) else {
                    return waiting();
                };
                let pending = self.pending.remove(&id).unwrap();
                self.finish_result(id, output, pending.interactive)
            }
            StreamEvent::Input(bytes) => {
                if self.pending.contains_key(&id) {
                    return waiting();
                }
                let Some(actor) = self.actors.get_mut(&id) else {
                    return StreamAction::finish(Vec::new());
                };
                let outcome = actor
                    .session
                    .input(&bytes, |input| actor.catalog.suggestions(input));
                let command = match outcome {
                    Ok(InputOutcome::Continue) => return StreamAction::default(),
                    Ok(InputOutcome::Exit) => {
                        self.actors.remove(&id);
                        return StreamAction::finish(Vec::new());
                    }
                    Ok(InputOutcome::Execute(command)) => command,
                    Err(error) => {
                        return StreamAction::finish(format!("Error: {error}\n").into_bytes())
                    }
                };
                let result = match command {
                    Command::Message { text, .. } => text_string(&text).map(complete),
                    Command::OpenVault { name } => text_string(&name).and_then(|name| {
                        let id = backend.open_vault(&name)?;
                        actor
                            .session
                            .select_vault(id, name.encode_utf16().collect());
                        Ok(complete(format!("Opened vault: {name}")))
                    }),
                    Command::Execute {
                        vault_selector,
                        argv,
                    } => {
                        let argv = argv
                            .iter()
                            .map(|arg| text_string(arg))
                            .collect::<Result<Vec<_>, _>>();
                        let selector = vault_selector.as_deref().map(text_string).transpose();
                        match (argv, selector) {
                            (Ok(argv), Ok(selector)) => Ok(backend.execute(
                                actor.session.current_vault_id.as_deref(),
                                selector.as_deref(),
                                &argv,
                            )),
                            (Err(error), _) | (_, Err(error)) => Err(error),
                        }
                    }
                };
                let result = result.unwrap_or_else(|error| complete(error));
                self.accept_result(id, result, true)
            }
            StreamEvent::Eof | StreamEvent::Closed => {
                if let Some(pending) = self.pending.remove(&id) {
                    if let Some(engine) = backend.operation_engine(pending.vault_id.as_deref()) {
                        pending.command.abandon(engine);
                    }
                }
                self.actors.remove(&id);
                StreamAction::finish(Vec::new())
            }
        }
    }
    fn accept_result(
        &mut self,
        id: ConnectionId,
        result: HostCommandExecution,
        interactive: bool,
    ) -> StreamAction {
        match result.execution {
            CommandExecution::Complete(output) => self.finish_result(id, output, interactive),
            CommandExecution::Pending(command) => {
                self.pending.insert(
                    id,
                    PendingExecution {
                        command,
                        vault_id: result.vault_id,
                        interactive,
                    },
                );
                waiting()
            }
        }
    }
    fn finish_result(
        &mut self,
        id: ConnectionId,
        output: String,
        interactive: bool,
    ) -> StreamAction {
        if !interactive {
            return StreamAction::finish(output.into_bytes());
        }
        if let Some(actor) = self.actors.get_mut(&id) {
            actor
                .session
                .finish_command(|input| actor.catalog.suggestions(input));
        }
        StreamAction {
            output: crate::registry::frame_output(&output).into_bytes(),
            close_after_output: false,
            input_policy: InputPolicy::DiscardBuffered,
        }
    }
}

fn text_string(input: &[u16]) -> Result<String, String> {
    String::from_utf16(input).map_err(|_| {
        "Error: This backend cannot execute a parameter containing an unpaired UTF-16 surrogate.".into()
    })
}
trait Backend {
    fn start(&mut self, request: &CliRequest) -> Option<InteractiveSession>;
    fn dispatch(&mut self, request: &CliRequest) -> HostCommandExecution;
    fn operation_engine(&mut self, id: Option<&str>) -> Option<&mut Engine>;
    fn catalog(&mut self, id: Option<&str>) -> CompletionCatalog;
    fn execute(
        &mut self,
        current: Option<&str>,
        selector: Option<&str>,
        argv: &[String],
    ) -> HostCommandExecution;
    fn open_vault(&mut self, selector: &str) -> Result<String, String>;
}
fn vault_name(engine: &Engine) -> String {
    engine
        .vault
        .root
        .file_name()
        .unwrap_or_default()
        .to_string_lossy()
        .into_owned()
}
fn engine_catalog(engine: &mut Engine, vaults: Vec<Units>) -> CompletionCatalog {
    let _ = crate::app::sync_builtin_registry(engine);
    let paths = engine.ordered_file_paths();
    let recent = engine.recents();
    let ordered = paths
        .iter()
        .filter(|path| recent.iter().any(|recent| recent == **path))
        .chain(
            paths
                .iter()
                .filter(|path| !recent.iter().any(|recent| recent == **path)),
        )
        .take(1000)
        .map(|path| path.encode_utf16().collect())
        .collect();
    // Workspace action callbacks and the full __commands family are not offered. The
    // CLI registry is a different catalog and must not masquerade as actions.
    CompletionCatalog::new(engine.commands.completions(), vaults, ordered, Vec::new())
}
impl Backend for VaultHost {
    fn start(&mut self, request: &CliRequest) -> Option<InteractiveSession> {
        match self
            .plan_cli_request(&request.argv, request.tty, Path::new(&request.cwd))
            .action
        {
            CliDispatchAction::Interactive { vault_id } => {
                let name = vault_id
                    .as_deref()
                    .and_then(|id| self.registry().registration(id))
                    .and_then(|entry| entry.path.file_name())
                    .unwrap_or_default()
                    .to_string_lossy();
                Some(InteractiveSession::new(
                    vault_id.clone(),
                    name.encode_utf16().collect(),
                ))
            }
            _ => None,
        }
    }
    fn dispatch(&mut self, request: &CliRequest) -> HostCommandExecution {
        crate::host_dispatch::begin_retained(self, request)
    }
    fn operation_engine(&mut self, id: Option<&str>) -> Option<&mut Engine> {
        self.engine_mut(id?)
    }
    fn catalog(&mut self, id: Option<&str>) -> CompletionCatalog {
        let vaults = self
            .registry()
            .registrations()
            .map(|entry| {
                entry
                    .path
                    .file_name()
                    .unwrap_or_default()
                    .to_string_lossy()
                    .encode_utf16()
                    .collect()
            })
            .collect();
        if let Some(id) = id {
            if crate::host_dispatch::install_retained_dispatch(self, id).is_ok() {
                return engine_catalog(self.engine_mut(id).unwrap(), vaults);
            }
        }
        // nt splits At's bare failure into a vault-name line on this path.
        unavailable_catalog()
    }
    fn execute(
        &mut self,
        current: Option<&str>,
        selector: Option<&str>,
        argv: &[String],
    ) -> HostCommandExecution {
        if !self.registry().cli_enabled() {
            return complete(CLI_DISABLED.into());
        }
        let id = match selector {
            Some(selector) => self.registry().find_selector(selector),
            None => current,
        }
        .map(str::to_owned);
        let Some(id) = id else {
            return complete(VAULT_NOT_FOUND.into());
        };
        if let Err(error) = crate::host_dispatch::install_retained_dispatch(self, &id) {
            return complete(crate::error::render(&error));
        }
        HostCommandExecution {
            execution: crate::app::begin(self.engine_mut(&id).unwrap(), argv),
            vault_id: Some(id),
        }
    }
    fn open_vault(&mut self, selector: &str) -> Result<String, String> {
        let id = self
            .registry()
            .find_selector(selector)
            .map(str::to_owned)
            .ok_or_else(|| format!("Vault not found: {selector}"))?;
        crate::host_dispatch::install_retained_dispatch(self, &id)
            .map_err(|error| crate::error::render(&error))?;
        Ok(id)
    }
}
struct SingleEngine<'a>(&'a mut Engine);
fn single_start(engine: &Engine, request: &CliRequest) -> Option<InteractiveSession> {
    let selector = request
        .argv
        .first()
        .and_then(|arg| arg.strip_prefix("vault="));
    let argv = if request
        .argv
        .first()
        .is_some_and(|arg| arg.starts_with("vault="))
    {
        &request.argv[1..]
    } else {
        &request.argv
    };
    if request.tty && argv.is_empty() {
        let id = match selector {
            Some(selector) => single_open(engine, selector).ok(),
            None => Some(engine.vault.root.to_string_lossy().into_owned()),
        };
        let name = if id.is_some() {
            vault_name(engine)
        } else {
            String::new()
        };
        Some(InteractiveSession::new(id, name.encode_utf16().collect()))
    } else {
        None
    }
}
fn single_open(engine: &Engine, selector: &str) -> Result<String, String> {
    let root = engine.vault.root.to_string_lossy();
    if selector == root || selector.to_uppercase() == vault_name(engine).to_uppercase() {
        Ok(root.into_owned())
    } else {
        Err(format!("Vault not found: {selector}"))
    }
}
fn single_argv(
    engine: &Engine,
    selector: Option<&str>,
    argv: &[String],
) -> Result<Vec<String>, String> {
    if let Some(selector) = selector {
        single_open(engine, selector).map_err(|_| VAULT_NOT_FOUND.to_string())?;
    }
    Ok(argv.to_vec())
}
impl Backend for SingleEngine<'_> {
    fn start(&mut self, request: &CliRequest) -> Option<InteractiveSession> {
        single_start(self.0, request)
    }
    fn dispatch(&mut self, request: &CliRequest) -> HostCommandExecution {
        HostCommandExecution {
            execution: crate::app::begin(self.0, &request.argv),
            vault_id: None,
        }
    }
    fn operation_engine(&mut self, _: Option<&str>) -> Option<&mut Engine> {
        Some(self.0)
    }
    fn catalog(&mut self, id: Option<&str>) -> CompletionCatalog {
        if id.is_none() {
            return unavailable_catalog();
        }
        let vaults = vec![vault_name(self.0).encode_utf16().collect()];
        engine_catalog(self.0, vaults)
    }
    fn execute(
        &mut self,
        current: Option<&str>,
        selector: Option<&str>,
        argv: &[String],
    ) -> HostCommandExecution {
        if current.is_none() && selector.is_none() {
            return complete(VAULT_NOT_FOUND.into());
        }
        match single_argv(self.0, selector, argv) {
            Ok(argv) => HostCommandExecution {
                execution: crate::app::begin(self.0, &argv),
                vault_id: None,
            },
            Err(error) => complete(error),
        }
    }
    fn open_vault(&mut self, selector: &str) -> Result<String, String> {
        single_open(self.0, selector)
    }
}
impl Backend for mnemosyne_engine::script_application::ScriptApplication {
    fn start(&mut self, request: &CliRequest) -> Option<InteractiveSession> {
        single_start(self.engine_mut(), request)
    }
    fn dispatch(&mut self, request: &CliRequest) -> HostCommandExecution {
        HostCommandExecution {
            execution: crate::app::begin_script(self, &request.argv),
            vault_id: None,
        }
    }
    fn operation_engine(&mut self, _: Option<&str>) -> Option<&mut Engine> {
        Some(self.engine_mut())
    }
    fn catalog(&mut self, id: Option<&str>) -> CompletionCatalog {
        if id.is_none() {
            return unavailable_catalog();
        }
        // Runtime eval registration uses the same ordinary prepare boundary.
        let _ = crate::app::execute_script(self, &["__completions".into()]);
        let engine = self.engine_mut();
        let vaults = vec![vault_name(engine).encode_utf16().collect()];
        engine_catalog(engine, vaults)
    }
    fn execute(
        &mut self,
        current: Option<&str>,
        selector: Option<&str>,
        argv: &[String],
    ) -> HostCommandExecution {
        if current.is_none() && selector.is_none() {
            return complete(VAULT_NOT_FOUND.into());
        }
        match single_argv(self.engine_mut(), selector, argv) {
            Ok(argv) => HostCommandExecution {
                execution: crate::app::begin_script(self, &argv),
                vault_id: None,
            },
            Err(error) => complete(error),
        }
    }
    fn open_vault(&mut self, selector: &str) -> Result<String, String> {
        single_open(self.engine_mut(), selector)
    }
}

fn unavailable_catalog() -> CompletionCatalog {
    CompletionCatalog::new(
        serde_json::json!({}),
        vec![VAULT_NOT_FOUND.encode_utf16().collect()],
        Vec::new(),
        Vec::new(),
    )
}
