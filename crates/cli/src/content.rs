//! Trusted-host local clients and lifecycle for the governed content API.
use mnemosyne_content::{
    api::{ApiResult, Initialization, Request, Response},
    auth::{Credentials, Grant, Permission, Policy},
    ContentError, Service,
};
use std::{io::Read, path::PathBuf};

pub fn run(args: &[String]) -> Option<i32> {
    let command = args.first()?.as_str();
    if !matches!(
        command,
        "content"
            | "content-init"
            | "content-grant"
            | "content-serve"
            | "content-backup"
            | "content-restore"
            | "content-retention"
            | "content-import-preview"
            | "content-export"
    ) {
        return None;
    }
    let response = handle(command, &args[1..]).unwrap_or_else(Response::failure);
    let status = i32::from(response.error.is_some());
    match serde_json::to_string(&response) {
        Ok(json) => println!("{json}"),
        Err(_) => {
            println!("{{\"version\":2,\"error\":{{\"code\":\"internal\",\"message\":\"Serialization failed\"}}}}");
            return Some(1);
        }
    }
    Some(status)
}
fn invalid(message: &str) -> ContentError {
    ContentError::new("invalid_request", message)
}
fn success() -> Response {
    Response {
        version: 2,
        result: Some(ApiResult::Initialized(Initialization { initialized: true })),
        error: None,
    }
}
fn handle(command: &str, args: &[String]) -> Result<Response, ContentError> {
    let mut options = std::collections::BTreeMap::new();
    let mut iter = args.iter();
    while let Some(option) = iter.next() {
        let value = iter.next().ok_or_else(|| invalid("Missing option value"))?;
        let valid = match command {
            "content" => matches!(option.as_str(), "--store" | "--credentials" | "--socket"),
            "content-import-preview" => matches!(
                option.as_str(),
                "--store"
                    | "--credentials"
                    | "--socket"
                    | "--source"
                    | "--base-revision"
                    | "--offset"
                    | "--batch-size"
                    | "--conflict-policy"
            ),
            "content-export" => matches!(
                option.as_str(),
                "--store" | "--credentials" | "--socket" | "--destination" | "--scope"
            ),
            "content-init" => matches!(option.as_str(), "--store" | "--policy"),
            "content-grant" => matches!(
                option.as_str(),
                "--store" | "--tenant" | "--actor" | "--permission" | "--scope" | "--while-serving"
            ),
            "content-serve" => matches!(option.as_str(), "--store" | "--credentials" | "--socket"),
            "content-backup" | "content-restore" => {
                matches!(option.as_str(), "--store" | "--backup")
            }
            "content-retention" => matches!(
                option.as_str(),
                "--store"
                    | "--tenant"
                    | "--keep-revisions"
                    | "--mode"
                    | "--expected-plan"
                    | "--expire-pruned-edit-replays"
            ),
            _ => false,
        };
        if !valid
            || options
                .insert(option.as_str(), PathBuf::from(value))
                .is_some()
        {
            return Err(invalid("Unknown or repeated option"));
        }
    }
    let required = |name: &str| {
        options
            .get(name)
            .ok_or_else(|| invalid(&format!("{name} is required")))
    };
    if command == "content-retention" {
        let value = |name: &str| -> Result<&str, ContentError> {
            required(name)?
                .to_str()
                .ok_or_else(|| invalid("Options must be UTF-8"))
        };
        let mode = options
            .get("--mode")
            .map(|v| v.to_str().unwrap_or(""))
            .unwrap_or("dry-run");
        if !matches!(mode, "dry-run" | "apply") {
            return Err(invalid("--mode must be dry-run or apply"));
        }
        let keep_revisions = value("--keep-revisions")?
            .parse::<usize>()
            .ok()
            .filter(|n| *n > 0)
            .ok_or_else(|| invalid("--keep-revisions must be a positive integer"))?;
        let expire_pruned_edit_replays = match options
            .get("--expire-pruned-edit-replays")
            .map(|v| v.to_str().unwrap_or(""))
        {
            None | Some("false") => false,
            Some("true") => true,
            _ => {
                return Err(invalid(
                    "--expire-pruned-edit-replays must be true or false",
                ))
            }
        };
        let policy = mnemosyne_content::retention::RetentionPolicy {
            tenant_id: value("--tenant")?.to_owned(),
            keep_revisions,
            expire_pruned_edit_replays,
        };
        let store = required("--store")?;
        let report = if mode == "apply" {
            mnemosyne_content::retention::apply(store, policy, value("--expected-plan")?)?
        } else {
            if options.contains_key("--expected-plan") {
                return Err(invalid("--expected-plan requires --mode apply"));
            }
            mnemosyne_content::retention::dry_run(store, policy)?
        };
        return Ok(Response {
            version: 2,
            result: Some(ApiResult::Retention(report)),
            error: None,
        });
    }
    if command == "content-backup" || command == "content-restore" {
        let store = required("--store")?;
        let backup = required("--backup")?;
        let report = if command == "content-backup" {
            mnemosyne_content::backup::create(store, backup)?
        } else {
            mnemosyne_content::backup::restore(backup, store)?
        };
        return Ok(Response {
            version: 2,
            result: Some(ApiResult::Backup(report)),
            error: None,
        });
    }
    if command == "content-grant" {
        let value = |name: &str| -> Result<String, ContentError> {
            required(name)?
                .to_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid("Options must be UTF-8"))
        };
        let permission = match value("--permission")?.as_str() {
            "read" => Permission::Read,
            "propose" => Permission::Propose,
            "edit" => Permission::Edit,
            "approve" => Permission::Approve,
            "admin" => Permission::Admin,
            _ => {
                return Err(invalid(
                    "--permission must be read, propose, edit, approve or admin",
                ))
            }
        };
        let while_serving = match options.get("--while-serving").map(|v| v.to_str()) {
            None | Some(Some("false")) => false,
            Some(Some("true")) => true,
            _ => return Err(invalid("--while-serving must be true or false")),
        };
        let grant = Grant {
            tenant_id: value("--tenant")?,
            actor_id: value("--actor")?,
            permission,
            scope: value("--scope")?,
        };
        let report =
            mnemosyne_content::grant::add_grant(required("--store")?, grant, while_serving)?;
        return Ok(Response {
            version: 2,
            result: Some(ApiResult::Grant(report)),
            error: None,
        });
    }
    #[cfg(unix)]
    if command == "content-serve" {
        let store = required("--store")?;
        // Held until this process exits; a second service is store_in_use.
        let lock = mnemosyne_content::grant::ServeLock::acquire(store)?;
        let server = mnemosyne_content::transport::Server::bind(
            store,
            required("--credentials")?,
            required("--socket")?,
            lock,
        )?;
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let signal_stop = stop.clone();
        ctrlc::set_handler(move || signal_stop.store(true, std::sync::atomic::Ordering::SeqCst))
            .map_err(|_| ContentError::new("configuration", "Cannot install shutdown handler"))?;
        eprintln!("Content listening: {}", required("--socket")?.display());
        server.run(stop)?;
        return Ok(success());
    }
    if command == "content-init" {
        let bytes = std::fs::read(required("--policy")?)
            .map_err(|_| ContentError::new("configuration", "Cannot read policy"))?;
        let policy: Policy = serde_json::from_slice(&bytes)
            .map_err(|_| ContentError::new("configuration", "Invalid policy"))?;
        Service::open(required("--store")?)?.bootstrap_policy(policy)?;
        return Ok(success());
    }
    if !matches!(
        command,
        "content" | "content-import-preview" | "content-export"
    ) {
        return Err(ContentError::new(
            "unsupported_platform",
            "Local socket service requires Unix",
        ));
    }
    if options.contains_key("--socket")
        && (options.contains_key("--store") || options.contains_key("--credentials"))
    {
        return Err(invalid(
            "--socket cannot be combined with --store or --credentials",
        ));
    }
    let token = std::env::var("MNEMOSYNE_CONTENT_TOKEN").unwrap_or_default();
    let identity = if options.contains_key("--socket") {
        None
    } else {
        let bytes = std::fs::read(required("--credentials")?)
            .map_err(|_| ContentError::new("configuration", "Cannot read credentials"))?;
        let credentials: Credentials = serde_json::from_slice(&bytes)
            .map_err(|_| ContentError::new("configuration", "Invalid credentials"))?;
        Some(credentials.authenticate(&token)?)
    };
    let request: Request = if command == "content-import-preview" {
        let value = |name: &str| options.get(name).and_then(|p| p.to_str());
        let integer = |name: &str, default: usize| -> Result<usize, ContentError> {
            value(name)
                .map(|v| v.parse().map_err(|_| invalid("Invalid integer option")))
                .unwrap_or(Ok(default))
        };
        let conflict_policy = match value("--conflict-policy").unwrap_or("error") {
            "error" => mnemosyne_content::onboarding::ConflictPolicy::Error,
            "skip" => mnemosyne_content::onboarding::ConflictPolicy::Skip,
            "overwrite" => mnemosyne_content::onboarding::ConflictPolicy::Overwrite,
            _ => {
                return Err(invalid(
                    "--conflict-policy must be error, skip or overwrite",
                ))
            }
        };
        let offset = integer("--offset", 0)?;
        let (files, next_offset) = mnemosyne_content::onboarding::scan_directory_batch(
            required("--source")?,
            offset,
            integer("--batch-size", 32)?,
        )?;
        Request {
            version: 2,
            idempotency_key: None,
            operation: mnemosyne_content::api::Operation::ImportPreview {
                base_revision: required("--base-revision")?
                    .to_str()
                    .ok_or_else(|| invalid("Base must be UTF-8"))?
                    .into(),
                files,
                offset,
                next_offset,
                conflict_policy,
            },
        }
    } else if command == "content-export" {
        Request {
            version: 2,
            idempotency_key: None,
            operation: mnemosyne_content::api::Operation::ExportSnapshot {
                scope: options
                    .get("--scope")
                    .and_then(|p| p.to_str())
                    .unwrap_or("")
                    .into(),
                revision: None,
                offset: 0,
                limit: 32,
                expected_policy_digest: None,
                // About 43 MiB of JSON per full page instead of about 110;
                // the writer decodes it.
                encoding: Some("base64".into()),
            },
        }
    } else {
        let mut input = String::new();
        std::io::stdin()
            .take(128 * 1024 * 1024 + 1)
            .read_to_string(&mut input)
            .map_err(|_| invalid("Cannot read request"))?;
        if input.len() > 128 * 1024 * 1024 {
            return Err(invalid("Request too large"));
        }
        serde_json::from_str(&input).map_err(|_| invalid("Invalid request JSON"))?
    };
    let call = |request: Request| -> Result<Response, ContentError> {
        if let Some(socket) = options.get("--socket") {
            #[cfg(unix)]
            {
                mnemosyne_content::transport::call(socket, token.clone(), request)
            }
            #[cfg(not(unix))]
            {
                let _ = socket;
                Err(invalid("Socket client requires Unix"))
            }
        } else {
            Ok(Service::open(required("--store")?)?.execute(
                identity
                    .as_ref()
                    .ok_or_else(|| invalid("Missing identity"))?,
                request,
            ))
        }
    };
    let response = call(request)?;
    if command == "content-export" && response.error.is_none() {
        let Some(ApiResult::ExportSnapshot(mut page)) = response.result else {
            return Err(invalid("Unexpected export response"));
        };
        let mut writer = mnemosyne_content::onboarding::ExportWriter::new(
            required("--destination")?,
            &page.revision_id,
        )?;
        loop {
            writer.append(&page)?;
            let Some(offset) = page.next_offset else {
                break;
            };
            let response = call(Request {
                version: 2,
                idempotency_key: None,
                operation: mnemosyne_content::api::Operation::ExportSnapshot {
                    scope: options
                        .get("--scope")
                        .and_then(|p| p.to_str())
                        .unwrap_or("")
                        .into(),
                    revision: Some(page.revision_id.clone()),
                    offset,
                    limit: 32,
                    expected_policy_digest: Some(page.policy_digest.clone()),
                    encoding: Some("base64".into()),
                },
            })?;
            if let Some(error) = response.error {
                return Err(error);
            }
            let Some(ApiResult::ExportSnapshot(next_page)) = response.result else {
                return Err(invalid("Unexpected export response"));
            };
            page = next_page;
        }
        return Ok(Response {
            version: 2,
            result: Some(ApiResult::ExportReport(writer.finish()?)),
            error: None,
        });
    }
    Ok(response)
}
