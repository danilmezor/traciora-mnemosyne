//! Owned differential host: normal CLI dispatch and explicit backend adapters plus a read-only log.
//! This is a development observer, not an extra advertised product command.
use mnemosyne_core::CONFIG_DIR;
#[cfg(unix)]
enum Application {
    Single(mnemosyne_engine::Engine),
    Host {
        host: mnemosyne_engine::host::VaultHost,
        selected: String,
    },
}
#[cfg(unix)]
impl Application {
    fn engine(&self) -> &mnemosyne_engine::Engine {
        match self {
            Self::Single(engine) => engine,
            Self::Host { host, selected } => host.engine(selected).unwrap(),
        }
    }
    fn engine_mut(&mut self) -> &mut mnemosyne_engine::Engine {
        match self {
            Self::Single(engine) => engine,
            Self::Host { host, selected } => host.engine_mut(selected).unwrap(),
        }
    }
    fn dispatch(
        &mut self,
        interactive: &mut mnemosyne::interactive_dispatch::InteractiveDispatch,
        id: mnemosyne_engine::server::ConnectionId,
        event: mnemosyne_engine::server::StreamEvent,
    ) -> mnemosyne_engine::server::StreamAction {
        match self {
            Self::Single(engine) => interactive.engine(engine, id, event),
            Self::Host { host, .. } => interactive.host(host, id, event),
        }
    }
    // Explicit development operation, independent of observation selection.
    // Native fixture uses Vault.rename, whereas product CLI move uses FileManager.
    fn direct_vault_rename(&mut self, argv: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(argv.len() == 3, "backend rename requires old and new paths");
        let (from, to) = (&argv[1], &argv[2]);
        for path in [from, to] {
            anyhow::ensure!(
                !path.is_empty()
                    && std::path::Path::new(path)
                        .components()
                        .all(|component| match component {
                            std::path::Component::Normal(name) =>
                                !name.to_string_lossy().starts_with('.'),
                            _ => false,
                        }),
                "backend rename requires normal non-hidden relative file paths"
            );
        }
        let engine = self.engine_mut();
        anyhow::ensure!(engine.vault.has_file(from), "backend rename source missing");
        anyhow::ensure!(
            std::fs::symlink_metadata(engine.vault.root.join(from))?.is_file(),
            "backend rename source must be a regular file"
        );
        anyhow::ensure!(
            std::fs::symlink_metadata(engine.vault.root.join(to))
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound),
            "backend rename destination exists"
        );
        std::fs::rename(engine.vault.root.join(from), engine.vault.root.join(to))?;
        engine.vault.reconcile_path_from_disk(from)?;
        engine.vault.reconcile_path_from_disk(to)?;
        // The normal hook publishes rename and acknowledges both watcher paths.
        // Pending metadata work is serviced by the unchanged owner idle loop.
        engine.file_renamed(from, to);
        Ok(())
    }
    fn direct_file_decision(&mut self, argv: &[String]) -> anyhow::Result<()> {
        anyhow::ensure!(argv.len() == 2, "backend decision requires one choice");
        let decision = match argv[1].as_str() {
            "decline" => mnemosyne_engine::file_operations::LinkUpdateDecision::DoNotUpdate,
            "once" => mnemosyne_engine::file_operations::LinkUpdateDecision::Once,
            "always" => mnemosyne_engine::file_operations::LinkUpdateDecision::Always,
            _ => anyhow::bail!("unknown backend link-update decision"),
        };
        let engine = self.engine_mut();
        let pending = engine
            .pending_link_update()
            .ok_or_else(|| anyhow::anyhow!("no pending link-update decision"))?;
        engine.decide_link_update(pending.id, decision)?;
        Ok(())
    }
    fn shutdown(&mut self) -> anyhow::Result<()> {
        match self {
            Self::Single(engine) => Ok(engine.prepare_shutdown()?),
            Self::Host { host, .. } => mnemosyne::host_dispatch::shutdown(host),
        }
    }
}
#[cfg(unix)]
pub fn run(args: Vec<String>) -> anyhow::Result<()> {
    use mnemosyne_engine::{observation, server::UnixStateServer, Engine};
    use serde_json::json;
    use std::{
        cell::{Cell, RefCell},
        fs::OpenOptions,
        io::Write,
        os::unix::fs::OpenOptionsExt,
        path::PathBuf,
    };

    anyhow::ensure!(
        args.len() == 3 || args.len() == 4,
        "usage: observer_server <owned-vault> <owned-socket> <new-observation-log> [owned-profile-json]"
    );
    // Explicit owned-host schedule, independent of event collection. The real
    // metadata owner is held only at worker delivery; resolver and writes run.
    let controlled_startup = std::env::var_os("MNEMOSYNE_OBSERVER_STARTUP_GATE").is_some();
    // This option changes observer subscription placement, not owner work.
    // FILE01's real plugin subscribes before MetadataCache attaches handlers.
    let early_file_observer = std::env::var_os("MNEMOSYNE_OBSERVER_FILE_OPERATIONS").is_some();
    let early_observer = controlled_startup || early_file_observer;
    anyhow::ensure!(
        !early_observer || args.len() == 3,
        "early subscription requires the single-vault observer host"
    );
    let worker_held = Cell::new(controlled_startup);
    let vault = PathBuf::from(&args[0]).canonicalize()?;
    let mut application = if let Some(profile) = args.get(3) {
        let mut host = mnemosyne_engine::host::VaultHost::from_profile(
            std::path::Path::new(profile),
            &std::env::current_dir()?,
        )?;
        let selected = host
            .registry()
            .registrations()
            .find(|entry| entry.path == vault)
            .ok_or_else(|| anyhow::anyhow!("owned observer vault is absent from declared profile"))?
            .id
            .clone();
        host.enable_retained_services()?;
        host.open_vault(&selected)?;
        Application::Host { host, selected }
    } else {
        Application::Single(if early_observer {
            Engine::prepare_startup(&vault)?
        } else {
            Engine::open_retained(&vault)?
        })
    };
    let engine = application.engine_mut();
    engine.enable_metadata_lifecycle()?;
    if !early_observer {
        mnemosyne::app::sync_builtin_registry(engine)?;
        engine.enable_retained_watch()?;
    }
    let metadata_events = std::env::var_os("MNEMOSYNE_METADATA_EVENTS").is_some();
    let cursor = Cell::new(engine.start_event_journal());
    let lifecycle_cursor = Cell::new(engine.start_lifecycle_journal());
    let log = RefCell::new(
        OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&args[2])?,
    );
    let failure = RefCell::new(None::<String>);
    let record = |engine: &Engine, trigger: serde_json::Value| -> anyhow::Result<()> {
        let selection = observation::Selection {
            effective_config_keys: [
                "attachmentFolderPath",
                "alwaysUpdateLinks",
                "newFileLocation",
                "useMarkdownLinks",
                "defaultViewMode",
                "livePreview",
            ]
            .into_iter()
            .map(str::to_owned)
            .collect(),
            metadata_paths: engine
                .metadata_lifecycle_snapshot()
                .cache_json
                .keys()
                .cloned()
                .collect(),
            metadata_fields: {
                let mut fields = vec![
                    "files".into(),
                    "markdown_files".into(),
                    "cached_files".into(),
                    "cache".into(),
                    "cache_json".into(),
                    "readiness".into(),
                    "resolved_links".into(),
                    "unresolved_links".into(),
                    "file_info".into(),
                    "file_stat".into(),
                    "info_stat_matches".into(),
                ];
                if std::env::var_os("MNEMOSYNE_OBSERVER_SAVED_SOURCES").is_some() {
                    fields.push("saved_sources".into());
                }
                fields
            },
            readiness_fields: vec!["metadata".into()],
            session_fields: vec![
                "active_file".into(),
                "active_leaf".into(),
                "leaves".into(),
                "last_open_files".into(),
                "buffers".into(),
            ],
            event_names: if metadata_events {
                [
                    "vault:create",
                    "vault:modify",
                    "vault:rename",
                    "vault:delete",
                    "vault:external-watch",
                    "metadata:changed",
                    "metadata:deleted",
                    "metadata:resolve",
                    "metadata:resolved",
                    "metadata:finished",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            } else {
                [
                    "vault:create",
                    "vault:modify",
                    "vault:rename",
                    "vault:delete",
                ]
                .into_iter()
                .map(str::to_owned)
                .collect()
            },
            after_cursor: if metadata_events {
                lifecycle_cursor.get()
            } else {
                cursor.get()
            },
            ..observation::Selection::default()
        };
        let state = observation::snapshot(engine, &selection)?;
        // Immediate dispatch snapshots and actual idle owner snapshots are retained.
        // Readiness and held file stats are explicit facts; callers establish
        // their selected disk/owner endpoint barrier without forcing a flush.
        serde_json::to_writer(
            &mut *log.borrow_mut(),
            &json!({
                "schema_version":1, "vault":engine.vault.root, "trigger":trigger,
            "state":state, "owner_frame":engine.metadata_lifecycle_snapshot(),
                "pending_file_operation":engine.pending_link_update().map(|pending| json!({"id":pending.id.0,"from":pending.from,"to":pending.to,"affected_references":pending.affected_references})),
                "app_config_bytes":std::fs::read(engine.vault.root.join(CONFIG_DIR).join("app.json")).ok(), "command_registry":engine.commands.names().collect::<Vec<_>>(),
                "scope":if controlled_startup {"Explicit startup worker hold with independent resolver/persistence; actual empty/incremental owner and early callback frames retained; no SDK/eval or native IDB-format claim"} else {"Immediate dispatch and actual owner timer snapshots retained; metadata readiness and file-stat relations explicit; no native IDB-format claim"}
            }),
        )?;
        log.borrow_mut().write_all(b"\n")?;
        log.borrow_mut().flush()?;
        cursor.set(engine.event_cursor());
        lifecycle_cursor.set(engine.lifecycle_cursor());
        Ok(())
    };
    record(
        application.engine(),
        json!({"kind":if early_observer {"startup-empty"} else {"startup"}}),
    )?;
    if early_observer {
        application.engine_mut().start_startup_loading()?;
        record(application.engine(), json!({"kind":"startup-start"}))?;
        loop {
            let layout_ready = application.engine_mut().poll_startup()?;
            record(
                application.engine(),
                json!({"kind":"startup-owner","layout_ready":layout_ready}),
            )?;
            if layout_ready {
                break;
            }
        }
        mnemosyne::app::sync_builtin_registry(application.engine_mut())?;
        application.engine_mut().enable_retained_watch()?;
    }
    let mut server = UnixStateServer::bind(&args[1], application)?;
    let control = server.control();
    let signal_control = control.clone();
    ctrlc::set_handler(move || signal_control.stop())?;
    eprintln!("Engine listening: {}", server.socket_path().display());
    let mut interactive = mnemosyne::interactive_dispatch::InteractiveDispatch::default();
    let last_pending_file_operation = Cell::new(None::<u64>);
    let result = server.run_streaming_with_event_loop(
        |application, id, event| {
            use mnemosyne_engine::server::StreamEvent;
            let is_poll = matches!(&event, StreamEvent::Poll);
            let trigger = match &event {
                StreamEvent::Request(request) => json!({"kind":"command","connection":id.0,"request":request}),
                StreamEvent::Input(bytes) => json!({"kind":"input","connection":id.0,"bytes":bytes}),
                StreamEvent::Eof => json!({"kind":"eof","connection":id.0}),
                StreamEvent::Closed => json!({"kind":"closed","connection":id.0}),
                StreamEvent::Poll => json!({"kind":"command-complete","connection":id.0}),
            };
            let action = if let StreamEvent::Request(request) = &event {
                if controlled_startup && request.argv.as_slice() == ["__observer_backend_startup_release"] {
                    worker_held.set(false);
                    mnemosyne_engine::server::StreamAction::finish(b"Startup worker released\n".to_vec())
                } else if request.argv.first().is_some_and(|name| name == "__observer_backend_file_decision") {
                    let output = match application.direct_file_decision(&request.argv) {
                        Ok(()) => b"Backend FileManager decision delivered\n".to_vec(),
                        Err(error) => format!("Error: {error}\n").into_bytes(),
                    };
                    mnemosyne_engine::server::StreamAction::finish(output)
                } else if request.argv.first().is_some_and(|name| name == "__observer_backend_vault_rename") {
                    let output = match application.direct_vault_rename(&request.argv) {
                        Ok(()) => b"Backend Vault.rename completed\n".to_vec(),
                        Err(error) => format!("Error: {error}\n").into_bytes(),
                    };
                    mnemosyne_engine::server::StreamAction::finish(output)
                } else {
                    application.dispatch(&mut interactive, id, event)
                }
            } else {
                application.dispatch(&mut interactive, id, event)
            };
            // Suspended commands poll quietly; preserve their actual response
            // boundary instead of writing a synthetic completion every tick.
            if !is_poll || !action.output.is_empty() || action.close_after_output {
                if let Err(error) = record(application.engine(), trigger) {
                    *failure.borrow_mut() = Some(error.to_string());
                    control.stop();
                }
            }
            action
        },
        |application| {
            let engine = application.engine_mut();
            let watched = engine.poll_external_changes();
            let completed = engine.poll_editor_autosave();
            let metadata = if worker_held.get() {
                let count = engine.poll_metadata_resolution();
                engine.poll_metadata_persistence().map(|_| count)
            } else { engine.poll_metadata_lifecycle() };
            let file_operations = engine.poll_file_operations();
            let pending_id = engine.pending_link_update().map(|pending| pending.id.0);
            let file_operation_transition = last_pending_file_operation.replace(pending_id) != pending_id;
            let _ = engine.poll_core_plugin_config();
            match (watched, metadata) {
                (Ok(paths), Ok(count)) if !paths.is_empty() || count>0 || file_operations>0 || file_operation_transition => {
                    if let Err(error)=record(engine,json!({"kind":"metadata-owner","filesystem_paths":paths,"metadata_events":count,"file_operations":file_operations})) {
                        *failure.borrow_mut()=Some(error.to_string()); control.stop();
                    }
                },
                (Err(error),_)|(_,Err(error)) => {
                    *failure.borrow_mut()=Some(error.to_string()); control.stop();
                },
                _=>{},
            }
            if engine.poll_metadata_cache_gc().is_ok_and(|removed| !removed.is_empty()) {
                if let Err(error) = record(engine, json!({"kind":"metadata-cache-gc"})) {
                    *failure.borrow_mut() = Some(error.to_string());
                    control.stop();
                }
            }
            if !completed.is_empty() {
                let outcomes: Vec<_> = completed.into_iter().map(|(leaf, result)| json!({"leaf":leaf,"error":result.err().map(|e|e.to_string())})).collect();
                if let Err(error) = record(engine, json!({"kind":"autosave","outcomes":outcomes})) {
                    *failure.borrow_mut() = Some(error.to_string());
                    control.stop();
                }
            }
        }
    );
    let persisted = server.state_mut().shutdown();
    result?;
    persisted?;
    anyhow::ensure!(
        failure.borrow().is_none(),
        "observer failed: {:?}",
        failure.borrow()
    );
    Ok(())
}

#[cfg(unix)]
fn main() -> anyhow::Result<()> {
    run(std::env::args().skip(1).collect())
}

#[cfg(not(unix))]
fn main() {
    panic!("the observer server requires Unix");
}
