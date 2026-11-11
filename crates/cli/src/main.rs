//! A clean-room Markdown knowledge base CLI.
//!
//! Commands take `command key=value` arguments. `vault=<name>` resolves against
//! the desktop vault registry.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use mnemosyne_core::{Note, Vault};
use mnemosyne_index::SearchIndex;
use notify::Watcher;

use mnemosyne::search::Params;

fn main() {
    if mnemosyne_index::semantic::worker_main(&std::env::args().collect::<Vec<_>>()) {
        return;
    }
    if mnemosyne_content::extraction::worker_main(&std::env::args().collect::<Vec<_>>()) {
        return;
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(status) = mnemosyne::content::run(&args) {
        std::process::exit(status);
    }
    #[cfg(unix)]
    if let Some(result) = relay_socket(&args) {
        if let Err(error) = result {
            if matches!(error, mnemosyne::transport::RelayError::Startup) {
                eprint!("{}", mnemosyne::transport::STARTUP_ERROR);
            }
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = run(&args) {
        // The reference native client receives application errors as output;
        // transport startup failures are a separate concern.
        print!("{}", mnemosyne::error::render(&error));
    }
}

#[cfg(unix)]
fn relay_socket(
    args: &[String],
) -> Option<std::result::Result<(), mnemosyne::transport::RelayError>> {
    if args
        .iter()
        .find(|arg| !arg.starts_with("vault="))
        .is_some_and(|arg| arg == "serve")
    {
        return None;
    }
    let socket = std::env::var_os("MNEMOSYNE_SOCKET")?;
    Some(mnemosyne::transport::relay(Path::new(&socket), args))
}

fn run(args: &[String]) -> Result<()> {
    let vault = args
        .first()
        .and_then(|arg| arg.strip_prefix("vault="))
        .map(str::to_owned);
    let command_args = if vault.is_some() { &args[1..] } else { args };
    let (command, mut params) = mnemosyne::args::parse_argv(command_args);
    let command = mnemosyne::args::prepare_command(&command, &mut params).into_owned();

    #[cfg(unix)]
    if command == "serve"
        && val(&params, "runtime").is_some()
        && (val(&params, "profile").is_some() || std::env::var_os("MNEMOSYNE_PROFILE").is_some())
    {
        bail!("Multi-vault runtime hosting remains pending; use an explicit single vault for this development server.");
    }

    #[cfg(unix)]
    if let Some(profile) = (command == "serve")
        .then(|| val(&params, "profile").map(std::ffi::OsString::from))
        .flatten()
        .or_else(|| std::env::var_os("MNEMOSYNE_PROFILE"))
        .or_else(|| {
            // Explicit development vaults retain the single-engine route.
            // Ordinary requests discover the desktop registry and use the
            // same client-cwd/focus/selector rules as a retained profile host.
            if std::env::var_os("MNEMOSYNE_VAULT").is_some()
                || vault
                    .as_deref()
                    .is_some_and(|value| Path::new(value).is_dir())
            {
                None
            } else {
                let profile = registry_path();
                profile.is_file().then(|| profile.into_os_string())
            }
        })
    {
        use std::io::IsTerminal;
        let cwd = std::env::current_dir()?;
        let mut host = mnemosyne_engine::host::VaultHost::from_profile(Path::new(&profile), &cwd)?;
        // Individual startup failures remain observable in host lifecycle events;
        // they do not overwrite an unrelated selected vault's CLI response.
        if command == "serve" {
            host.enable_retained_services()?;
        }
        let _ = host.restore_open_vaults();
        if command == "serve" {
            let ids: Vec<_> = host.loaded_vault_ids().map(str::to_owned).collect();
            for id in ids {
                if let Some(engine) = host.engine_mut(&id) {
                    engine.enable_metadata_lifecycle()?;
                    engine.enable_retained_watch()?;
                }
            }
            let socket = val(&params, "socket")
                .ok_or_else(|| anyhow::anyhow!("serve requires socket=<path>"))?;
            let mut server = mnemosyne_engine::server::UnixStateServer::bind(socket, host)?;
            let control = server.control();
            ctrlc::set_handler(move || control.stop())?;
            eprintln!("Engine listening: {}", server.socket_path().display());
            let mut interactive = mnemosyne::interactive_dispatch::InteractiveDispatch::default();
            server.run_streaming_with_event_loop(
                |host, id, event| interactive.host(host, id, event),
                |host| {
                    let ids: Vec<_> = host.loaded_vault_ids().map(str::to_owned).collect();
                    for id in ids {
                        if let Some(engine) = host.engine_mut(&id) {
                            let _ = engine.poll_external_changes();
                            engine.poll_editor_autosave();
                            let _ = engine.poll_metadata_lifecycle();
                            engine.poll_file_operations();
                            let _ = engine.poll_metadata_cache_gc();
                            let _ = engine.poll_core_plugin_config();
                        }
                    }
                },
            )?;
            mnemosyne::host_dispatch::shutdown(server.state_mut())?;
        } else {
            let request = mnemosyne_engine::server::CliRequest {
                argv: args.to_vec(),
                tty: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
                cwd: cwd.to_string_lossy().into_owned(),
            };
            let execution = mnemosyne::host_dispatch::begin(&mut host, &request);
            print!("{}", mnemosyne::host_dispatch::drive(&mut host, execution));
            mnemosyne::host_dispatch::persist(&host)?;
        }
        return Ok(());
    }

    if command == "version" {
        println!("{}", mnemosyne::version::response());
        return Ok(());
    }
    if command == "vaults" {
        cmd_vaults(&params)?;
        return Ok(());
    }

    let vault_path = resolve_vault(vault.as_deref())?;
    #[cfg(unix)]
    if command == "serve" {
        let socket = val(&params, "socket")
            .ok_or_else(|| anyhow::anyhow!("serve requires socket=<path>"))?;
        let mut engine = mnemosyne_engine::Engine::open_retained(&vault_path)?;
        engine.enable_metadata_lifecycle()?;
        engine.enable_retained_watch()?;
        if let Some(manifest) = val(&params, "runtime") {
            use mnemosyne_engine::script_application::{RuntimeInstallation, ScriptApplication};
            let manifest = Path::new(manifest).canonicalize()?;
            let mut installation: RuntimeInstallation =
                serde_json::from_slice(&std::fs::read(&manifest)?)?;
            installation.resolve_relative_to(manifest.parent().unwrap());
            let runtime_profile = installation.profile.clone();
            let application = ScriptApplication::launch(engine, installation)?;
            let mut server = mnemosyne_engine::server::UnixStateServer::bind(socket, application)?;
            let control = server.control();
            ctrlc::set_handler(move || control.stop())?;
            eprintln!("Engine listening: {}", server.socket_path().display());
            let mut interactive = mnemosyne::interactive_dispatch::InteractiveDispatch::default();
            let result = server.run_streaming_with_event_loop(
                |application, id, event| interactive.script(application, id, event),
                |application| {
                    let _ = application.engine_mut().poll_external_changes();
                    application.engine_mut().poll_editor_autosave();
                    let _ = application.engine_mut().poll_metadata_lifecycle();
                    application.engine_mut().poll_file_operations();
                    let _ = application.engine_mut().poll_metadata_cache_gc();
                    let _ = application.engine_mut().poll_core_plugin_config();
                },
            );
            // Always attempt both persistence and owned runtime cleanup before
            // propagating a shutdown error. No unrelated process is stopped.
            let persisted = server.state_mut().engine_mut().prepare_shutdown();
            let stopped = server.state_mut().stop();
            if let Ok(receipt) = &stopped {
                std::fs::write(
                    runtime_profile.join("stop-receipt.json"),
                    serde_json::to_vec_pretty(&serde_json::json!({
                        "pid":receipt.pid,"runtime_pid":receipt.runtime_pid,"exit_code":receipt.exit_code,
                        "reaped":receipt.reaped,"process_group_stopped":receipt.process_group_stopped
                    }))?,
                )?;
            }
            result?;
            persisted?;
            stopped?;
            return Ok(());
        }
        let mut server = mnemosyne_engine::server::UnixEngineServer::bind(socket, engine)?;
        let control = server.control();
        ctrlc::set_handler(move || control.stop())?;
        eprintln!("Engine listening: {}", server.socket_path().display());
        let mut interactive = mnemosyne::interactive_dispatch::InteractiveDispatch::default();
        server.run_streaming_with_event_loop(
            |engine, id, event| interactive.engine(engine, id, event),
            |engine| {
                let _ = engine.poll_external_changes();
                engine.poll_editor_autosave();
                let _ = engine.poll_metadata_lifecycle();
                engine.poll_file_operations();
                let _ = engine.poll_metadata_cache_gc();
                let _ = engine.poll_core_plugin_config();
            },
        )?;
        server.engine_mut().prepare_shutdown()?;
        return Ok(());
    }
    if matches!(command.as_str(), "index" | "watch") {
        dispatch(&command, &vault_path, &params)
    } else {
        let mut engine = mnemosyne_engine::Engine::open(&vault_path)?;
        let execution = mnemosyne::app::begin(&mut engine, args);
        print!("{}", mnemosyne::app::drive(&mut engine, execution));
        Ok(())
    }
}

fn flag(p: &Params, k: &str) -> bool {
    p.get(k)
        .is_some_and(|value| value.as_ref().is_none_or(|s| !s.is_empty()))
}

fn val<'a>(p: &'a Params, k: &str) -> Option<&'a str> {
    p.get(k).and_then(|v| v.as_deref())
}

fn format_of(p: &Params) -> &str {
    val(p, "format").unwrap_or("text")
}

fn dispatch(command: &str, vault_path: &Path, p: &Params) -> Result<()> {
    match command {
        "vault" => cmd_vault(vault_path, p)?,
        "files" => cmd_files(vault_path, p)?,
        "folders" => cmd_folders(vault_path, p)?,
        "file" => cmd_file(vault_path, p)?,
        "read" => cmd_read(vault_path, p)?,
        "search" => cmd_search(vault_path, p)?,
        "search:context" => cmd_search_context(vault_path, p)?,
        "backlinks" => cmd_backlinks(vault_path, p)?,
        "links" => cmd_links(vault_path, p)?,
        "tags" => cmd_tags(vault_path, p)?,
        "tag" => cmd_tag(vault_path, p)?,
        "unresolved" => cmd_unresolved(vault_path, p)?,
        "orphans" => cmd_orphans(vault_path, p, false)?,
        "deadends" => cmd_orphans(vault_path, p, true)?,
        "outline" => cmd_outline(vault_path, p)?,
        "properties" => cmd_properties(vault_path, p)?,
        "property:read" => cmd_property_read(vault_path, p)?,
        "wordcount" => cmd_wordcount(vault_path, p)?,
        "aliases" => cmd_aliases(vault_path, p)?,
        "create" => cmd_create(vault_path, p)?,
        "rename" => cmd_rename(vault_path, p)?,
        "move" => cmd_move(vault_path, p)?,
        "delete" => cmd_delete(vault_path, p)?,
        "append" => cmd_append(vault_path, p, false)?,
        "prepend" => cmd_append(vault_path, p, true)?,
        "tasks" => cmd_tasks(vault_path, p, false)?,
        "task" => cmd_tasks(vault_path, p, true)?,
        "watch" => watch(vault_path)?,
        "index" => cmd_index(vault_path)?,
        _ => {
            println!(
                "Error: Command \"{command}\" not found. It may require a plugin to be enabled."
            );
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Vault resolution
// ---------------------------------------------------------------------------

fn resolve_vault(name: Option<&str>) -> Result<PathBuf> {
    if let Some(n) = name {
        if let Some(p) = find_vault_in_registry(n)? {
            return Ok(p);
        }
        let path = PathBuf::from(n);
        if path.is_dir() {
            return Ok(path);
        }
        bail!("Vault not found: {n}");
    }
    if let Ok(p) = std::env::var("MNEMOSYNE_VAULT") {
        if !p.is_empty() {
            return Ok(PathBuf::from(p));
        }
    }
    bail!("No vault specified. Use vault=<name> or set MNEMOSYNE_VAULT.")
}

fn find_vault_in_registry(name: &str) -> Result<Option<PathBuf>> {
    let config = registry_path();
    let Ok(content) = std::fs::read_to_string(&config) else {
        return Ok(None);
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) else {
        return Ok(None);
    };
    let Some(vaults) = json.get("vaults").and_then(|v| v.as_object()) else {
        return Ok(None);
    };
    for entry in vaults.values() {
        if let Some(path) = entry.get("path").and_then(|v| v.as_str()) {
            let base = Path::new(path)
                .file_name()
                .map(|s| s.to_string_lossy().into_owned());
            if base.as_deref() == Some(name) {
                return Ok(Some(PathBuf::from(path)));
            }
        }
    }
    Ok(None)
}

fn registry_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_default();
    mnemosyne_engine::vault_registry::registry_path(Path::new(&home))
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn note_path(note: &Note) -> String {
    note.path.to_string_lossy().replace('\\', "/")
}

/// Resolves a `file=` (name/title) or `path=` (exact) target.
fn resolve_target<'a>(
    vault: &'a Vault,
    file: Option<&str>,
    path: Option<&str>,
) -> Option<&'a Note> {
    if let Some(p) = path {
        if let Some(n) = vault.get(p) {
            return Some(n);
        }
        if let Some(n) = vault.get(&format!("{p}.md")) {
            return Some(n);
        }
        return None;
    }
    if let Some(f) = file {
        if let Some(n) = vault.get_by_title(f) {
            return Some(n);
        }
        if let Some(n) = vault.get(&format!("{f}.md")) {
            return Some(n);
        }
        if let Some(n) = vault.get(f) {
            return Some(n);
        }
    }
    None
}

/// Resolves a `file=`/`path=` target to any file (markdown or attachment),
/// returning the vault-relative path.
fn resolve_file(vault: &Vault, file: Option<&str>, path: Option<&str>) -> Option<String> {
    if let Some(p) = path {
        if vault.has_file(p) {
            return Some(p.to_string());
        }
        let pmd = format!("{p}.md");
        if vault.has_file(&pmd) {
            return Some(pmd);
        }
        return None;
    }
    if let Some(f) = file {
        if vault.has_file(f) {
            return Some(f.to_string());
        }
        let fmd = format!("{f}.md");
        if vault.has_file(&fmd) {
            return Some(fmd);
        }
        if let Some(p) = vault
            .files()
            .iter()
            .find(|p| mnemosyne_core::basename(p).eq_ignore_ascii_case(f))
        {
            return Some(p.clone());
        }
    }
    None
}

/// Walks all non-hidden files, returning (files, folders, total bytes).
fn walk_all(root: &Path) -> Result<(Vec<String>, Vec<String>, u64)> {
    let mut files = Vec::new();
    let mut folders = Vec::new();
    let mut size = 0u64;
    walk_dir(root, root, &mut files, &mut folders, &mut size)?;
    files.sort();
    folders.sort();
    Ok((files, folders, size))
}

fn walk_dir(
    root: &Path,
    dir: &Path,
    files: &mut Vec<String>,
    folders: &mut Vec<String>,
    size: &mut u64,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') {
            continue;
        }
        let ft = entry.file_type()?;
        if ft.is_dir() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            folders.push(rel.to_string_lossy().replace('\\', "/"));
            walk_dir(root, &path, files, folders, size)?;
        } else if ft.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            files.push(rel.to_string_lossy().replace('\\', "/"));
            *size += entry.metadata()?.len();
        }
    }
    Ok(())
}

fn open_index(vault_path: &Path) -> Result<SearchIndex> {
    let dir = vault_path.join(".mnemosyne").join("index");
    Ok(SearchIndex::open_or_create(&dir)?)
}

// ---------------------------------------------------------------------------
// Read commands
// ---------------------------------------------------------------------------

fn cmd_vault(vault_path: &Path, p: &Params) -> Result<()> {
    let (files, folders, size) = walk_all(vault_path)?;
    let name = vault_path.file_name().unwrap_or_default().to_string_lossy();
    let info = val(p, "info");
    let print = |k: &str, v: String| {
        if info.is_none() || info == Some(k) {
            println!("{k}\t{v}");
        }
    };
    print("name", name.to_string());
    print("path", vault_path.display().to_string());
    print("files", files.len().to_string());
    print("folders", folders.len().to_string());
    print("size", size.to_string());
    Ok(())
}

fn cmd_vaults(p: &Params) -> Result<()> {
    let mut vaults: Vec<(String, String)> = Vec::new();
    if let Ok(json) = std::fs::read_to_string(registry_path())
        .and_then(|c| serde_json::from_str::<serde_json::Value>(&c).map_err(Into::into))
    {
        if let Some(map) = json.get("vaults").and_then(|v| v.as_object()) {
            for entry in map.values() {
                if let Some(path) = entry.get("path").and_then(|v| v.as_str()) {
                    let name = Path::new(path)
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into_owned();
                    vaults.push((name, path.to_string()));
                }
            }
        }
    }
    if flag(p, "total") {
        println!("{}", vaults.len());
        return Ok(());
    }
    if flag(p, "verbose") {
        for (name, path) in vaults {
            println!("{name}\t{path}");
        }
    } else {
        for (name, _) in vaults {
            println!("{name}");
        }
    }
    Ok(())
}

fn cmd_files(vault_path: &Path, p: &Params) -> Result<()> {
    let (files, _, _) = walk_all(vault_path)?;
    let folder = val(p, "folder");
    let ext = val(p, "ext");
    let filtered: Vec<&String> = files
        .iter()
        .filter(|f| {
            folder
                .map(|fo| {
                    Path::new(f)
                        .parent()
                        .map(|pa| pa.to_string_lossy() == fo)
                        .unwrap_or(false)
                })
                .unwrap_or(true)
        })
        .filter(|f| {
            ext.map(|e| {
                Path::new(f)
                    .extension()
                    .map(|x| x.to_string_lossy() == e)
                    .unwrap_or(false)
            })
            .unwrap_or(true)
        })
        .collect();
    if flag(p, "total") {
        println!("{}", filtered.len());
        return Ok(());
    }
    for f in filtered {
        println!("{f}");
    }
    Ok(())
}

fn cmd_folders(vault_path: &Path, p: &Params) -> Result<()> {
    let (_, folders, _) = walk_all(vault_path)?;
    let folder = val(p, "folder");
    let mut out: Vec<String> = vec!["/".to_string()];
    for f in folders {
        if let Some(fo) = folder {
            if f != fo && !f.starts_with(&format!("{fo}/")) {
                continue;
            }
        }
        out.push(f);
    }
    if flag(p, "total") {
        println!("{}", out.len());
        return Ok(());
    }
    for f in out {
        println!("{f}");
    }
    Ok(())
}

fn cmd_read(vault_path: &Path, p: &Params) -> Result<()> {
    let engine = mnemosyne_engine::Engine::open(vault_path)?;
    let text = engine.read_saved(mnemosyne_engine::FileTarget {
        file: val(p, "file").filter(|v| !v.is_empty()),
        path: val(p, "path").filter(|v| !v.is_empty()),
    })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    print!("{text}");
    if !text.is_empty() && !text.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn cmd_file(vault_path: &Path, p: &Params) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let path = resolve_file(&vault, val(p, "file"), val(p, "path"))
        .ok_or_else(|| anyhow::anyhow!("File not found."))?;
    let abs = vault.root.join(&path);
    let meta = std::fs::metadata(&abs)?;
    let base = mnemosyne_core::basename(&path);
    let ext = mnemosyne_core::extension(&path);
    let name = if ext.is_empty() {
        base.to_string()
    } else {
        base[..base.len() - ext.len() - 1].to_string()
    };
    let epoch_ms = |t: std::time::SystemTime| {
        t.duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    };
    println!("path\t{path}");
    println!("name\t{name}");
    println!("extension\t{ext}");
    println!("size\t{}", meta.len());
    println!("created\t{}", meta.created().map(epoch_ms).unwrap_or(0));
    println!("modified\t{}", meta.modified().map(epoch_ms).unwrap_or(0));
    Ok(())
}

fn cmd_search(vault_path: &Path, p: &Params) -> Result<()> {
    search_command(vault_path, p, false)
}

fn cmd_search_context(vault_path: &Path, p: &Params) -> Result<()> {
    search_command(vault_path, p, true)
}

fn search_command(vault_path: &Path, p: &Params, context: bool) -> Result<()> {
    let engine = mnemosyne_engine::Engine::open(vault_path)?;
    if !mnemosyne::app::search_enabled(&engine) {
        let command = if context { "search:context" } else { "search" };
        println!("Error: Command \"{command}\" not found. It may require a plugin to be enabled.");
        return Ok(());
    }
    print!(
        "{}",
        mnemosyne::search::response_cached(
            engine
                .ordered_cached_notes()
                .filter(|(note, _)| !engine.is_user_ignored(&note.path.to_string_lossy())),
            p,
            context
        )
    );
    Ok(())
}

fn cmd_backlinks(vault_path: &Path, p: &Params) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let path = resolve_file(&vault, val(p, "file"), val(p, "path"))
        .ok_or_else(|| anyhow::anyhow!("File not found."))?;
    let mut backlinks: Vec<String> = vault.backlinks_to(&path).to_vec();
    backlinks.sort_by(|a, b| natural_compare(a, b));
    if flag(p, "total") {
        println!("{}", backlinks.len());
        return Ok(());
    }
    if backlinks.is_empty() {
        println!("No backlinks found.");
        return Ok(());
    }
    let fmt = format_of(p);
    if flag(p, "counts") {
        let mut counts = vault.backlink_counts(&path);
        counts.sort_by(|a, b| natural_compare(&a.0, &b.0));
        match fmt {
            "json" => {
                let v: Vec<serde_json::Value> = counts
                    .iter()
                    .map(|(f, c)| serde_json::json!({"file": f, "count": c.to_string()}))
                    .collect();
                println!("{}", serde_json::to_string(&v)?);
            }
            "csv" => {
                for (f, c) in &counts {
                    println!("{f},{c}");
                }
            }
            _ => {
                for (f, c) in &counts {
                    println!("{f}\t{c}");
                }
            }
        }
    } else {
        match fmt {
            "json" => {
                let v: Vec<serde_json::Value> = backlinks
                    .iter()
                    .map(|f| serde_json::json!({"file": f}))
                    .collect();
                println!("{}", serde_json::to_string(&v)?);
            }
            _ => {
                for f in &backlinks {
                    println!("{f}");
                }
            }
        }
    }
    Ok(())
}

fn cmd_links(vault_path: &Path, p: &Params) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let path = resolve_file(&vault, val(p, "file"), val(p, "path"))
        .ok_or_else(|| anyhow::anyhow!("File not found."))?;
    let mut links: Vec<String> = Vec::new();
    if let Some(note) = vault.get(&path) {
        for r in note.references() {
            if !mnemosyne_core::is_internal_link(&r.link) {
                continue;
            }
            let lt = mnemosyne_core::parse_linktext(&r.link);
            match vault.resolve_linkpath_alias(note, &lt.path) {
                Some(dest) => links.push(dest),
                None => links.push(format!("{} (unresolved)", lt.path)),
            }
        }
    }
    if flag(p, "total") {
        println!("{}", links.len());
        return Ok(());
    }
    if links.is_empty() {
        println!("No links found.");
        return Ok(());
    }
    for l in links {
        println!("{l}");
    }
    Ok(())
}

fn cmd_tags(vault_path: &Path, p: &Params) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let target = val(p, "file").or(val(p, "path"));
    let mut tags: Vec<(String, usize)> = if flag(p, "active") {
        Vec::new()
    } else if let Some(_t) = target {
        resolve_target(&vault, val(p, "file"), val(p, "path"))
            .map(|n| n.all_tags().iter().map(|x| (x.clone(), 1)).collect())
            .unwrap_or_default()
    } else {
        vault.get_tags().into_iter().collect()
    };
    if val(p, "sort") == Some("count") {
        tags.sort_by(|a, b| b.1.cmp(&a.1));
    } else {
        tags.sort_by(|a, b| natural_compare(&a.0, &b.0));
    }

    if flag(p, "total") {
        println!("{}", tags.len());
        return Ok(());
    }
    if tags.is_empty() {
        println!("No tags found.");
        return Ok(());
    }
    let fmt = format_of(p);
    let counts = flag(p, "counts");
    match fmt {
        "json" if counts => {
            let v: Vec<serde_json::Value> = tags
                .iter()
                .map(|(t, c)| serde_json::json!({"tag": t, "count": c.to_string()}))
                .collect();
            println!("{}", serde_json::to_string(&v)?);
        }
        "json" => {
            let v: Vec<serde_json::Value> = tags
                .iter()
                .map(|(t, _)| serde_json::json!({"tag": t}))
                .collect();
            println!("{}", serde_json::to_string(&v)?);
        }
        "tsv" if counts => {
            for (t, c) in &tags {
                println!("{t}\t{c}");
            }
        }
        "csv" if counts => {
            for (t, c) in &tags {
                println!("{t},{c}");
            }
        }
        _ => {
            for (t, _) in &tags {
                println!("{t}");
            }
        }
    }
    Ok(())
}

/// Approximates the desktop app's `Intl.Collator({sensitivity:"base", numeric:true})`:
/// case-insensitive with digit runs compared numerically.
fn natural_compare(a: &str, b: &str) -> std::cmp::Ordering {
    let a: Vec<char> = a.to_lowercase().chars().collect();
    let b: Vec<char> = b.to_lowercase().chars().collect();
    let mut i = 0;
    let mut j = 0;
    while i < a.len() && j < b.len() {
        if a[i].is_ascii_digit() && b[j].is_ascii_digit() {
            let (na, ni) = num_run(&a, i);
            let (nb, nj) = num_run(&b, j);
            if na != nb {
                return na.cmp(&nb);
            }
            i = ni;
            j = nj;
        } else {
            if a[i] != b[j] {
                return a[i].cmp(&b[j]);
            }
            i += 1;
            j += 1;
        }
    }
    a.len().cmp(&b.len())
}

fn num_run(chars: &[char], start: usize) -> (u64, usize) {
    let mut n: u64 = 0;
    let mut i = start;
    while i < chars.len() && chars[i].is_ascii_digit() {
        n = n
            .saturating_mul(10)
            .saturating_add(chars[i].to_digit(10).unwrap() as u64);
        i += 1;
    }
    (n, i)
}

fn cmd_tag(vault_path: &Path, p: &Params) -> Result<()> {
    let name = val(p, "name").ok_or_else(|| anyhow::anyhow!("name is required"))?;
    let vault = Vault::open(vault_path)?;
    let files = vault.notes_with_tag(name);
    if flag(p, "total") {
        println!("{}", files.len());
        return Ok(());
    }
    if flag(p, "verbose") {
        println!("{name}\t{}", files.len());
    }
    for f in files {
        println!("{f}");
    }
    Ok(())
}

fn cmd_unresolved(vault_path: &Path, p: &Params) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let graph = vault.graph();
    let mut links: Vec<(String, usize, Vec<String>)> = graph
        .unresolved
        .into_iter()
        .map(|(link, count)| {
            let sources = graph
                .unresolved_sources
                .get(&link)
                .cloned()
                .unwrap_or_default();
            (link, count, sources)
        })
        .collect();
    links.sort_by(|a, b| natural_compare(&a.0, &b.0));

    if flag(p, "total") {
        println!("{}", links.len());
        return Ok(());
    }
    if links.is_empty() {
        println!("No unresolved links found.");
        return Ok(());
    }
    let fmt = format_of(p);
    let counts = flag(p, "counts");
    let verbose = flag(p, "verbose");
    if fmt == "json" {
        let v: Vec<serde_json::Value> = links
            .iter()
            .map(|(l, c, s)| {
                let mut obj = serde_json::json!({"link": l});
                if counts || verbose {
                    obj["count"] = serde_json::json!(c.to_string());
                }
                if verbose {
                    obj["sources"] = serde_json::json!(s.join(", "));
                }
                obj
            })
            .collect();
        println!("{}", serde_json::to_string(&v)?);
    } else if verbose {
        for (l, c, s) in &links {
            println!("{l}\t{c}\t{}", s.join(", "));
        }
    } else if counts {
        for (l, c, _) in &links {
            println!("{l}\t{c}");
        }
    } else {
        for (l, _, _) in &links {
            println!("{l}");
        }
    }
    Ok(())
}

fn cmd_orphans(vault_path: &Path, p: &Params, deadends: bool) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let mut paths = if deadends {
        vault.deadends()
    } else {
        vault.orphans()
    };
    paths.sort();
    if flag(p, "total") {
        println!("{}", paths.len());
        return Ok(());
    }
    for path in paths {
        println!("{path}");
    }
    Ok(())
}

fn cmd_outline(vault_path: &Path, p: &Params) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let path = resolve_file(&vault, val(p, "file"), val(p, "path"))
        .ok_or_else(|| anyhow::anyhow!("File not found."))?;
    let note = vault
        .get(&path)
        .ok_or_else(|| anyhow::anyhow!("File is not a markdown file."))?;
    let headings = &note.headings;
    if flag(p, "total") {
        println!("{}", headings.len());
        return Ok(());
    }
    if headings.is_empty() {
        println!("No headings found.");
        return Ok(());
    }
    match format_of(p) {
        "json" => {
            let v: Vec<serde_json::Value> = headings
                .iter()
                .map(|h| {
                    let line = note.body[..h.start].matches('\n').count() + 1;
                    serde_json::json!({"level": h.level, "heading": h.text, "line": line})
                })
                .collect();
            println!("{}", serde_json::to_string(&v)?);
        }
        "md" => {
            for h in headings {
                println!("{} {}", "#".repeat(h.level as usize), h.text);
            }
        }
        _ => print_tree(headings),
    }
    Ok(())
}

fn print_tree(headings: &[mnemosyne_core::Heading]) {
    let mut last_level = 0;
    for h in headings {
        let level = h.level as usize;
        let mut prefix = String::new();
        for i in 1..level {
            prefix.push_str("    ");
            let _ = i;
        }
        if level > 1 {
            prefix.push_str("└── ");
        }
        let _ = last_level;
        last_level = level;
        println!("{}{}", prefix, h.text);
    }
}

fn cmd_properties(vault_path: &Path, p: &Params) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let fmt = format_of(p);

    // Active-file mode: headless has no active file.
    if flag(p, "active") {
        println!("No frontmatter found.");
        return Ok(());
    }

    // Per-file mode: show the file's frontmatter.
    if val(p, "file").is_some() || val(p, "path").is_some() {
        let note = resolve_target(&vault, val(p, "file"), val(p, "path"))
            .ok_or_else(|| anyhow::anyhow!("File not found."))?;
        match fmt {
            "json" => {
                let data = note.frontmatter.as_ref().map(|f| &f.data);
                match data {
                    Some(d) => println!("{}", serde_json::to_string(d)?),
                    None => println!("{{}}"),
                }
            }
            _ => {
                if let Some(fm) = &note.frontmatter {
                    for k in fm.data.keys() {
                        println!("{k}");
                    }
                }
            }
        }
        return Ok(());
    }

    // Name mode: count files using a specific property.
    if let Some(name) = val(p, "name") {
        let count = vault
            .notes
            .values()
            .filter(|n| {
                n.frontmatter
                    .as_ref()
                    .map(|f| f.data.contains_key(name))
                    .unwrap_or(false)
            })
            .count();
        println!("{count}");
        return Ok(());
    }

    // Global mode: property names with inferred types and counts.
    let mut props: BTreeMap<String, (String, usize)> = BTreeMap::new();
    for note in vault.notes.values() {
        if let Some(fm) = &note.frontmatter {
            for (k, v) in &fm.data {
                let ty = infer_type(v);
                let entry = props.entry(k.clone()).or_insert((ty, 0));
                entry.1 += 1;
            }
        }
    }
    for (name, ty) in [
        ("aliases", "aliases"),
        ("cssclasses", "multitext"),
        ("tags", "tags"),
    ] {
        let entry = props.entry(name.to_string()).or_insert((String::new(), 0));
        entry.0 = ty.to_string();
    }

    let mut out: Vec<(String, String, usize)> = props
        .into_iter()
        .map(|(name, (ty, count))| (name, ty, count))
        .collect();
    if val(p, "sort") == Some("count") {
        out.sort_by(|a, b| b.2.cmp(&a.2).then_with(|| a.0.cmp(&b.0)));
    } else {
        out.sort_by(|a, b| a.0.cmp(&b.0));
    }

    if flag(p, "total") {
        println!("{}", out.len());
        return Ok(());
    }
    let counts = flag(p, "counts");
    if fmt == "json" {
        let v: Vec<serde_json::Value> = out
            .iter()
            .map(|(name, ty, count)| serde_json::json!({"name": name, "type": ty, "count": count}))
            .collect();
        println!("{}", serde_json::to_string(&v)?);
    } else if counts {
        for (name, _, count) in &out {
            println!("{name}\t{count}");
        }
    } else {
        for (name, _, _) in &out {
            println!("{name}");
        }
    }
    Ok(())
}

fn infer_type(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::Bool(_) => "checkbox".to_string(),
        serde_yaml::Value::Number(_) => "number".to_string(),
        serde_yaml::Value::Sequence(_) => "multitext".to_string(),
        serde_yaml::Value::Mapping(_) | serde_yaml::Value::Null => "unknown".to_string(),
        _ => "text".to_string(),
    }
}

fn cmd_property_read(vault_path: &Path, p: &Params) -> Result<()> {
    let name = val(p, "name").ok_or_else(|| anyhow::anyhow!("name is required"))?;
    let vault = Vault::open(vault_path)?;
    let note = resolve_target(&vault, val(p, "file"), val(p, "path"))
        .ok_or_else(|| anyhow::anyhow!("File not found."))?;
    if let Some(fm) = &note.frontmatter {
        if let Some(v) = fm.data.get(name) {
            println!("{}", yaml_scalar(v));
            return Ok(());
        }
    }
    eprintln!("Property \"{name}\" not found.");
    std::process::exit(1);
}

fn yaml_scalar(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Null => "".to_string(),
        serde_yaml::Value::Sequence(seq) => {
            seq.iter().map(yaml_scalar).collect::<Vec<_>>().join(", ")
        }
        other => serde_json::to_string(other).unwrap_or_default(),
    }
}

fn cmd_wordcount(vault_path: &Path, p: &Params) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let note = resolve_target(&vault, val(p, "file"), val(p, "path"))
        .ok_or_else(|| anyhow::anyhow!("File not found."))?;
    let words = note.source.split_whitespace().count();
    let chars = note.source.chars().count();
    if flag(p, "words") {
        println!("{words}");
    } else if flag(p, "characters") {
        println!("{chars}");
    } else {
        println!("words: {words}");
        println!("characters: {chars}");
    }
    Ok(())
}

fn cmd_aliases(vault_path: &Path, p: &Params) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let mut aliases: Vec<(String, String)> = Vec::new();
    let per_file = val(p, "file").is_some() || val(p, "path").is_some();
    if flag(p, "active") {
        aliases.clear();
    } else if per_file {
        let note = resolve_target(&vault, val(p, "file"), val(p, "path"))
            .ok_or_else(|| anyhow::anyhow!("File not found."))?;
        if let Some(fm) = &note.frontmatter {
            for a in fm.aliases() {
                aliases.push((a, note_path(note)));
            }
        }
    } else {
        for note in vault.notes.values() {
            if let Some(fm) = &note.frontmatter {
                for a in fm.aliases() {
                    aliases.push((a, note_path(note)));
                }
            }
        }
    }
    aliases.sort_by(|a, b| a.0.cmp(&b.0));
    if flag(p, "total") {
        println!("{}", aliases.len());
        return Ok(());
    }
    if aliases.is_empty() {
        println!("No aliases found.");
        return Ok(());
    }
    if flag(p, "verbose") && !per_file {
        for (a, path) in aliases {
            println!("{a}\t{path}");
        }
    } else {
        for (a, _) in aliases {
            println!("{a}");
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mutation commands
// ---------------------------------------------------------------------------

fn cmd_create(vault_path: &Path, p: &Params) -> Result<()> {
    let mut engine = mnemosyne_engine::Engine::open(vault_path)?;
    let text = mnemosyne::create::execute(&mut engine, p)?;
    print!("{text}");
    if !text.is_empty() && !text.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn cmd_file_manager(vault_path: &Path, p: &Params, moving: bool) -> Result<()> {
    let mut engine = mnemosyne_engine::Engine::open(vault_path)?;
    let text = mnemosyne::file_manager::execute(&mut engine, p, moving)?;
    println!("{text}");
    Ok(())
}

fn cmd_rename(vault_path: &Path, p: &Params) -> Result<()> {
    cmd_file_manager(vault_path, p, false)
}

fn cmd_move(vault_path: &Path, p: &Params) -> Result<()> {
    cmd_file_manager(vault_path, p, true)
}

fn cmd_delete(vault_path: &Path, p: &Params) -> Result<()> {
    let mut vault = Vault::open(vault_path)?;
    let key = resolve_file(&vault, val(p, "file"), val(p, "path"))
        .ok_or_else(|| anyhow::anyhow!("File not found."))?;
    if flag(p, "permanent") {
        vault.delete(&key)?;
        println!("Deleted permanently: {key}");
    } else {
        vault.trash(&key)?;
        println!("Moved to trash: {key}");
    }
    Ok(())
}

fn cmd_append(vault_path: &Path, p: &Params, prepend: bool) -> Result<()> {
    let mut engine = mnemosyne_engine::Engine::open(vault_path)?;
    let text = mnemosyne::text_edit::execute(&mut engine, p, prepend)?;
    println!("{text}");
    Ok(())
}

fn cmd_tasks(vault_path: &Path, p: &Params, single: bool) -> Result<()> {
    let mut engine = mnemosyne_engine::Engine::open(vault_path)?;
    let text = mnemosyne::tasks::execute(&mut engine, p, single)?;
    print!("{text}");
    if !text.is_empty() && !text.ends_with('\n') {
        println!();
    }
    Ok(())
}

fn cmd_index(vault_path: &Path) -> Result<()> {
    let vault = Vault::open(vault_path)?;
    let mut index = open_index(vault_path)?;
    index.rebuild(&vault)?;
    println!("indexed {} notes", vault.notes.len());
    Ok(())
}

fn watch(vault_path: &Path) -> Result<()> {
    use std::time::Duration;
    let (tx, rx) = std::sync::mpsc::channel();
    let mut watcher = notify::RecommendedWatcher::new(
        move |res| {
            let _ = tx.send(res);
        },
        notify::Config::default(),
    )?;
    watcher.watch(vault_path, notify::RecursiveMode::Recursive)?;
    println!("watching {} (Ctrl-C to stop)", vault_path.display());

    let mut index = open_index(vault_path)?;
    loop {
        let event = match rx.recv() {
            Ok(Ok(e)) => e,
            Ok(Err(_)) => continue,
            Err(_) => break,
        };
        let touches_md = event
            .paths
            .iter()
            .any(|p| p.extension().and_then(|s| s.to_str()) == Some("md"));
        if !touches_md {
            continue;
        }
        std::thread::sleep(Duration::from_millis(250));
        while rx.try_recv().is_ok() {}
        let vault = match Vault::open(vault_path) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("reopen failed: {e}");
                continue;
            }
        };
        if let Err(e) = index.rebuild_incremental(&vault) {
            eprintln!("reindex failed: {e}");
        } else {
            println!("reindexed {} notes", vault.notes.len());
        }
    }
    Ok(())
}
