//! CLI callbacks for the retained multi-vault engine host.
//! Routing and implemented command dispatch are shared with the single-vault
//! application. Stateful interactive execution is owned by interactive_dispatch.
use mnemosyne_engine::{
    host::{CliQueueResult, FilesystemOpenEffect, VaultHost},
    server::CliRequest,
    vault_registry::{CliDispatchAction, CLI_DISABLED, VAULT_NOT_FOUND},
};
use std::path::Path;
/// Initialize the implemented dispatch registry after loading a vault snapshot.
/// This installs the handler boundary; it does not mark workspace layout or
/// enabled plugin backends ready and does not discard earlier queued requests.
pub fn install_dispatch(host: &mut VaultHost, id: &str) -> anyhow::Result<()> {
    install_dispatch_mode(host, id, false)
}
pub fn install_retained_dispatch(host: &mut VaultHost, id: &str) -> anyhow::Result<()> {
    host.enable_retained_services()?;
    install_dispatch_mode(host, id, true)
}
fn install_dispatch_mode(host: &mut VaultHost, id: &str, retained: bool) -> anyhow::Result<()> {
    let engine = host.open_vault(id)?;
    if retained {
        engine.enable_metadata_lifecycle()?;
        engine.enable_retained_watch()?;
    }
    crate::app::sync_builtin_registry(engine)?;
    host.mark_handler_installed(id)?;
    Ok(())
}
pub struct HostCommandExecution {
    pub execution: crate::app::CommandExecution,
    pub vault_id: Option<String>,
}
pub fn begin(host: &mut VaultHost, request: &CliRequest) -> HostCommandExecution {
    begin_mode(host, request, false)
}
pub fn begin_retained(host: &mut VaultHost, request: &CliRequest) -> HostCommandExecution {
    begin_mode(host, request, true)
}
fn begin_mode(host: &mut VaultHost, request: &CliRequest, retained: bool) -> HostCommandExecution {
    if retained {
        if let Err(error) = host.enable_retained_services() {
            return HostCommandExecution {
                execution: crate::app::CommandExecution::Complete(crate::error::render(
                    &error.into(),
                )),
                vault_id: None,
            };
        }
    }
    let plan = host.plan_cli_request(&request.argv, request.tty, Path::new(&request.cwd));
    if let CliDispatchAction::Execute {
        vault_id: Some(id),
        argv,
    } = plan.action
    {
        if let Err(error) = install_dispatch_mode(host, &id, retained) {
            return HostCommandExecution {
                execution: crate::app::CommandExecution::Complete(crate::error::render(&error)),
                vault_id: None,
            };
        }
        let request = match host.enqueue_cli(&id, argv) {
            Ok(CliQueueResult::Dispatch(request)) => request,
            Ok(CliQueueResult::Queued { .. }) => {
                unreachable!("installed handler queued a new request")
            }
            Err(error) => {
                return HostCommandExecution {
                    execution: crate::app::CommandExecution::Complete(crate::error::render(
                        &error.into(),
                    )),
                    vault_id: None,
                }
            }
        };
        return HostCommandExecution {
            execution: crate::app::begin(host.engine_mut(&id).unwrap(), &request.argv),
            vault_id: Some(id),
        };
    }
    HostCommandExecution {
        execution: crate::app::CommandExecution::Complete(execute_with_launcher_mode(
            host,
            request,
            launch_default_application,
            retained,
        )),
        vault_id: None,
    }
}
pub fn drive(host: &mut VaultHost, execution: HostCommandExecution) -> String {
    match execution.execution {
        crate::app::CommandExecution::Complete(output) => output,
        pending @ crate::app::CommandExecution::Pending(_) => crate::app::drive(
            host.engine_mut(execution.vault_id.as_deref().unwrap())
                .unwrap(),
            pending,
        ),
    }
}

pub fn execute(host: &mut VaultHost, request: &CliRequest) -> String {
    execute_with_launcher(host, request, launch_default_application)
}
pub fn execute_retained(host: &mut VaultHost, request: &CliRequest) -> String {
    execute_with_launcher_mode(host, request, launch_default_application, true)
}
/// Explicit platform boundary for controlled tests. Ordinary requests use the
/// real launcher, rather than the injected branch used in those tests.
pub fn execute_with_launcher(
    host: &mut VaultHost,
    request: &CliRequest,
    launcher: impl FnMut(&Path) -> anyhow::Result<()>,
) -> String {
    execute_with_launcher_mode(host, request, launcher, false)
}
fn execute_with_launcher_mode(
    host: &mut VaultHost,
    request: &CliRequest,
    mut launcher: impl FnMut(&Path) -> anyhow::Result<()>,
    retained: bool,
) -> String {
    if retained {
        if let Err(error) = host.enable_retained_services() {
            return crate::error::render(&error.into());
        }
    }
    let plan = host.plan_cli_request(&request.argv, request.tty, Path::new(&request.cwd));
    match plan.action {
        CliDispatchAction::Disabled => crate::registry::frame_output(CLI_DISABLED),
        CliDispatchAction::Execute { vault_id: None, .. } => {
            crate::registry::frame_output(VAULT_NOT_FOUND)
        }
        CliDispatchAction::Execute {
            vault_id: Some(id),
            argv,
        } => {
            if let Err(error) = install_dispatch_mode(host, &id, retained) {
                return crate::error::render(&error);
            }
            let queued = match host.enqueue_cli(&id, argv) {
                Ok(request) => request,
                Err(error) => return crate::error::render(&error.into()),
            };
            match queued {
                CliQueueResult::Dispatch(request) => {
                    crate::app::execute(host.engine_mut(&id).unwrap(), &request.argv)
                }
                // Installing synchronously above guarantees the direct branch.
                // Async startup/stream promise ownership is separate host work.
                CliQueueResult::Queued { .. } => {
                    unreachable!("installed handler queued a new request")
                }
            }
        }
        CliDispatchAction::ProcessUri(_) => "Error: URI opening is unavailable.\n".into(),
        CliDispatchAction::OpenPath(path) => {
            match host.open_filesystem_path(Path::new(&path)) {
                Ok(result) => match result.effect {
                    FilesystemOpenEffect::OpenWithDefaultApplication { path, .. } => {
                        if let Err(error) = launcher(&path) {
                            host.record_dispatch_diagnostic(format!(
                                "Default application: {error}"
                            ));
                        }
                    }
                    FilesystemOpenEffect::PendingExternalView { view_type, .. } => {
                        host.record_dispatch_diagnostic(format!(
                            "External view backend pending: {view_type}"
                        ));
                    }
                    FilesystemOpenEffect::Opened {
                        pending_subpath: Some(subpath),
                        ..
                    } => {
                        host.record_dispatch_diagnostic(format!(
                            "Filesystem subpath navigation pending: {subpath}"
                        ));
                    }
                    _ => {}
                },
                Err(error) => host.record_dispatch_diagnostic(format!("Filesystem open: {error}")),
            }
            // Pt acknowledges He dispatch independently of its eventual
            // renderer result; this does not certify pending view backends.
            crate::registry::frame_output(&format!("Opened {path}"))
        }
        CliDispatchAction::Interactive { .. } => {
            "Error: Interactive command input is unavailable.\n".into()
        }
    }
}

fn launch_default_application(path: &Path) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    {
        use std::process::{Command, Stdio};
        let output = Command::new("/usr/bin/open")
            .arg(path)
            .stdin(Stdio::null())
            .output()?;
        if !output.status.success() {
            anyhow::bail!("{}", String::from_utf8_lossy(&output.stderr).trim_end());
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        anyhow::bail!("Default application integration is pending on this platform.")
    }
}
/// Persist an internal session checkpoint, retaining live editor state.
/// Real application shutdown uses shutdown and its view-save boundary.
pub fn persist(host: &VaultHost) -> anyhow::Result<()> {
    for (_, result) in host.persist_sessions() {
        result?;
    }
    Ok(())
}

pub fn shutdown(host: &mut VaultHost) -> anyhow::Result<()> {
    let results = host.prepare_shutdown_sessions();
    for (_, result) in results {
        result?;
    }
    Ok(())
}
