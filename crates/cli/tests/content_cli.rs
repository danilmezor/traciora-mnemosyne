use std::{
    io::Write,
    process::{Command, Stdio},
    sync::atomic::{AtomicU64, Ordering},
};
static NEXT: AtomicU64 = AtomicU64::new(0);
#[test]
fn content_cli_authenticates_before_json_and_ignores_legacy_transport() {
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-cli-auth-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let policy = root.join("policy.json");
    let credentials = root.join("credentials.json");
    let store = root.join("store");
    std::fs::write(
        &policy,
        r#"{"grants":[{"tenant_id":"tenant","actor_id":"reader","permission":"read","scope":""}]}"#,
    )
    .unwrap();
    std::fs::write(&credentials,r#"{"credentials":[{"token":"secret","identity":{"tenant_id":"tenant","actor_id":"reader","kind":"agent"}}]}"#).unwrap();
    let binary = env!("CARGO_BIN_EXE_mnemosyne");
    let init = Command::new(binary)
        .args(["content-init", "--store"])
        .arg(&store)
        .arg("--policy")
        .arg(&policy)
        .output()
        .unwrap();
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stdout)
    );
    let invoke = |token: &str, request: &str| {
        let mut child = Command::new(binary)
            .args(["content", "--store"])
            .arg(&store)
            .arg("--credentials")
            .arg(&credentials)
            .env("MNEMOSYNE_CONTENT_TOKEN", token)
            .env("MNEMOSYNE_SOCKET", root.join("nonexistent-legacy.sock"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let _ = child.stdin.take().unwrap().write_all(request.as_bytes());
        child.wait_with_output().unwrap()
    };
    let result = invoke("wrong", r#"{"version":1,"operation":{"type":"head"}}"#);
    assert!(!result.status.success());
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["error"]["code"], "unauthenticated");
    let result = invoke("secret", r#"{"version":1,"operation":{"type":"head"}}"#);
    assert!(result.status.success());
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["result"], "r0");
    let result = invoke(
        "secret",
        r#"{"version":1,"identity":{"kind":"human"},"operation":{"type":"head"}}"#,
    );
    assert!(!result.status.success());
    let json: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
    assert_eq!(json["error"]["code"], "invalid_request");
    std::fs::remove_dir_all(root).unwrap();
}

/// Every file of `dir`, by path below it, with its bytes; folders as `None`.
fn tree(dir: &std::path::Path) -> std::collections::BTreeMap<String, Option<Vec<u8>>> {
    fn walk(
        root: &std::path::Path,
        dir: &std::path::Path,
        out: &mut std::collections::BTreeMap<String, Option<Vec<u8>>>,
    ) {
        for child in std::fs::read_dir(dir).unwrap() {
            let path = child.unwrap().path();
            let name = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            if path.is_dir() {
                out.insert(name, None);
                walk(root, &path, out);
            } else {
                out.insert(name, Some(std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(dir, dir, &mut out);
    out
}

#[test]
fn content_export_cli_uses_base64() {
    use mnemosyne_content::{auth::TrustedIdentity, onboarding::ExportWriter, Service};
    let root = std::env::temp_dir().join(format!(
        "mnemosyne-cli-export-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).unwrap();
    let root = std::fs::canonicalize(&root).unwrap();
    let policy = root.join("policy.json");
    let credentials = root.join("credentials.json");
    let store = root.join("store");
    std::fs::write(
        &policy,
        r#"{"grants":[{"tenant_id":"t","actor_id":"w","permission":"read","scope":""},
                      {"tenant_id":"t","actor_id":"w","permission":"edit","scope":""}]}"#,
    )
    .unwrap();
    std::fs::write(
        &credentials,
        r#"{"credentials":[{"token":"secret","identity":{"tenant_id":"t","actor_id":"w","kind":"human"}}]}"#,
    )
    .unwrap();
    let binary = env!("CARGO_BIN_EXE_mnemosyne");
    let init = Command::new(binary)
        .args(["content-init", "--store"])
        .arg(&store)
        .arg("--policy")
        .arg(&policy)
        .output()
        .unwrap();
    assert!(init.status.success());
    // A small fixture tree, and enough notes for a second page.
    let mut changes = vec![
        serde_json::json!({"type": "create_folder", "path": "docs"}),
        serde_json::json!({"type": "put", "path": "docs/a.md", "content": "# Привет\r\n"}),
        serde_json::json!({"type": "put_binary", "path": "z.bin", "bytes": [255, 0, 128]}),
        serde_json::json!({"type": "put_binary", "path": "img/photo.png",
                           "bytes": [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0, 0xff]}),
    ];
    for i in 0..40 {
        changes.push(
            serde_json::json!({"type": "put", "path": format!("notes/n{i:02}.md"),
                                        "content": format!("note {i}")}),
        );
    }
    let request = serde_json::json!({"version": 2, "idempotency_key": "seed",
        "operation": {"type": "edit", "base_revision": "r0", "changes": changes}});
    let mut child = Command::new(binary)
        .args(["content", "--store"])
        .arg(&store)
        .arg("--credentials")
        .arg(&credentials)
        .env("MNEMOSYNE_CONTENT_TOKEN", "secret")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(request.to_string().as_bytes())
        .unwrap();
    let seeded = child.wait_with_output().unwrap();
    assert!(
        seeded.status.success(),
        "{}",
        String::from_utf8_lossy(&seeded.stdout)
    );
    let destination = root.join("cli-export");
    let export = Command::new(binary)
        .args(["content-export", "--store"])
        .arg(&store)
        .arg("--credentials")
        .arg(&credentials)
        .arg("--destination")
        .arg(&destination)
        .env("MNEMOSYNE_CONTENT_TOKEN", "secret")
        .output()
        .unwrap();
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stdout)
    );
    let report: serde_json::Value = serde_json::from_slice(&export.stdout).unwrap();
    assert_eq!(report["result"]["entries"], 44, "{report}");
    // Array pages through the same writer give the same bytes.
    let service = Service::open(&store).unwrap();
    let who = TrustedIdentity {
        tenant_id: "t".into(),
        actor_id: "w".into(),
        kind: mnemosyne_content::auth::ActorKind::Human,
        on_behalf_of: None,
    };
    let mut page = service
        .export_snapshot(&who, "", None, 0, 32, None)
        .unwrap();
    assert!(page.next_offset.is_some());
    let expected = root.join("array-export");
    let mut writer = ExportWriter::new(&expected, &page.revision_id).unwrap();
    loop {
        assert!(page.files.iter().all(|f| f.bytes_base64.is_none()));
        writer.append(&page).unwrap();
        let Some(offset) = page.next_offset else {
            break;
        };
        page = service
            .export_snapshot(&who, "", Some(&page.revision_id), offset, 32, None)
            .unwrap();
    }
    writer.finish().unwrap();
    let cli = tree(&destination);
    assert_eq!(cli, tree(&expected));
    assert_eq!(cli["current/z.bin"], Some(vec![255, 0, 128]));
    assert_eq!(
        cli["original/docs/a.md"],
        Some("# Привет\r\n".as_bytes().to_vec())
    );
    std::fs::remove_dir_all(root).unwrap();
}

// ---- content-grant and the serve lock ----

struct GrantFixture {
    root: std::path::PathBuf,
    store: std::path::PathBuf,
    credentials: std::path::PathBuf,
}
impl GrantFixture {
    fn new(initialise: bool) -> Self {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!(
            "mnemosyne-cli-grant-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
        let policy = root.join("policy.json");
        let credentials = root.join("credentials.json");
        let store = root.join("store");
        std::fs::write(
            &policy,
            r#"{"grants":[{"tenant_id":"t","actor_id":"admin","permission":"admin","scope":""}]}"#,
        )
        .unwrap();
        std::fs::write(
            &credentials,
            r#"{"credentials":[
                {"token":"admin-token","identity":{"tenant_id":"t","actor_id":"admin","kind":"human"}},
                {"token":"new-token","identity":{"tenant_id":"t","actor_id":"newcomer","kind":"agent"}}]}"#,
        )
        .unwrap();
        if initialise {
            let init = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
                .args(["content-init", "--store"])
                .arg(&store)
                .arg("--policy")
                .arg(&policy)
                .output()
                .unwrap();
            assert!(init.status.success());
        }
        Self {
            root,
            store,
            credentials,
        }
    }
    fn grant(&self, extra: &[&str]) -> (bool, serde_json::Value) {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mnemosyne"));
        command.args(["content-grant", "--store"]).arg(&self.store);
        let mut args = vec![
            "--tenant",
            "t",
            "--actor",
            "newcomer",
            "--permission",
            "read",
            "--scope",
            "",
        ];
        args.extend_from_slice(extra);
        let out = command.args(args).output().unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout)
            .unwrap_or_else(|_| panic!("{}", String::from_utf8_lossy(&out.stdout)));
        (out.status.success(), json)
    }
    fn raw_grant(&self, args: &[&str]) -> (bool, serde_json::Value) {
        let out = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .arg("content-grant")
            .args(args)
            .output()
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        (out.status.success(), json)
    }
    fn serve(&self, socket: &str) -> std::process::Child {
        let socket = self.root.join(socket);
        let child = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
            .args(["content-serve", "--store"])
            .arg(&self.store)
            .arg("--credentials")
            .arg(&self.credentials)
            .arg("--socket")
            .arg(&socket)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        while !socket.exists() {
            assert!(std::time::Instant::now() < deadline, "serve did not listen");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        child
    }
    /// One request through `content`: over the socket when given, else the store.
    fn content(&self, socket: Option<&str>, token: &str, request: &str) -> serde_json::Value {
        let mut command = Command::new(env!("CARGO_BIN_EXE_mnemosyne"));
        command.arg("content");
        match socket {
            Some(socket) => command.arg("--socket").arg(self.root.join(socket)),
            None => command
                .arg("--store")
                .arg(&self.store)
                .arg("--credentials")
                .arg(&self.credentials),
        };
        let mut child = command
            .env("MNEMOSYNE_CONTENT_TOKEN", token)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(request.as_bytes())
            .unwrap();
        serde_json::from_slice(&child.wait_with_output().unwrap().stdout).unwrap()
    }
}
impl Drop for GrantFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}
fn signal(child: &std::process::Child, signal: i32) {
    assert_eq!(unsafe { libc::kill(child.id() as i32, signal) }, 0);
}

#[test]
fn content_grant_adds_to_an_initialised_store_only() {
    let f = GrantFixture::new(false);
    let (ok, json) = f.grant(&[]);
    assert!(!ok);
    assert_eq!(json["error"]["code"], "not_initialized", "{json}");
    assert!(!f.store.exists());
    let f = GrantFixture::new(true);
    let (ok, json) = f.grant(&[]);
    assert!(ok, "{json}");
    assert_eq!(json["version"], 2);
    let result = &json["result"];
    assert_eq!(
        (
            &result["tenant_id"],
            &result["actor_id"],
            &result["permission"],
            &result["scope"],
            &result["added"]
        ),
        (
            &"t".into(),
            &"newcomer".into(),
            &"read".into(),
            &"".into(),
            &true.into()
        )
    );
    let policy = f.content(
        None,
        "admin-token",
        r#"{"version":2,"operation":{"type":"policy_get"}}"#,
    );
    assert_eq!(
        policy["result"]["digest"], result["policy_digest"],
        "{policy}"
    );
    assert!(policy["result"]["policy"]["grants"]
        .as_array()
        .unwrap()
        .iter()
        .any(|g| g["actor_id"] == "newcomer" && g["permission"] == "read"));
    let (ok, again) = f.grant(&["--while-serving", "false"]);
    assert!(ok);
    assert_eq!(again["result"]["added"], false);
    assert_eq!(again["result"]["policy_digest"], result["policy_digest"]);
}

#[test]
fn content_grant_refuses_while_serving() {
    let f = GrantFixture::new(true);
    let mut serve = f.serve("a.sock");
    let head = r#"{"version":2,"operation":{"type":"head"}}"#;
    let before = f.content(Some("a.sock"), "new-token", head);
    assert_eq!(before["error"]["code"], "forbidden", "{before}");
    let (ok, json) = f.grant(&[]);
    assert!(!ok);
    assert_eq!(json["error"]["code"], "store_in_use", "{json}");
    assert!(
        json["error"]["message"]
            .as_str()
            .unwrap()
            .contains(&serve.id().to_string()),
        "{json}"
    );
    let (ok, json) = f.grant(&["--while-serving", "true"]);
    assert!(ok, "{json}");
    assert_eq!(json["result"]["added"], true);
    // The running service answers the new grantee without a restart.
    let after = f.content(Some("a.sock"), "new-token", head);
    assert_eq!(after["result"], "r0", "{after}");
    // A second service on the same store refuses, naming the first.
    let second = Command::new(env!("CARGO_BIN_EXE_mnemosyne"))
        .args(["content-serve", "--store"])
        .arg(&f.store)
        .arg("--credentials")
        .arg(&f.credentials)
        .arg("--socket")
        .arg(f.root.join("b.sock"))
        .output()
        .unwrap();
    assert!(!second.status.success());
    let json: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(json["error"]["code"], "store_in_use", "{json}");
    assert!(json["error"]["message"]
        .as_str()
        .unwrap()
        .contains(&serve.id().to_string()));
    assert!(!f.root.join("b.sock").exists());
    // SIGTERM releases the lock.
    signal(&serve, libc::SIGTERM);
    assert!(serve.wait().unwrap().success());
    let (ok, json) = f.grant(&[]);
    assert!(ok, "{json}");
    assert_eq!(json["result"]["added"], false);
}

#[test]
fn content_grant_reclaims_a_dead_servers_lock() {
    let f = GrantFixture::new(true);
    let mut serve = f.serve("a.sock");
    let dead = serve.id();
    signal(&serve, libc::SIGKILL);
    let _ = serve.wait();
    // The lock file still names the dead pid; nothing holds it.
    let held = std::fs::read_to_string(f.store.join("serve.lock")).unwrap();
    assert!(held.contains(&dead.to_string()), "{held}");
    let (ok, json) = f.grant(&[]);
    assert!(ok, "{json}");
    let mut serve = f.serve("b.sock");
    let held = std::fs::read_to_string(f.store.join("serve.lock")).unwrap();
    assert!(held.contains(&serve.id().to_string()), "{held}");
    assert_eq!(
        f.content(
            Some("b.sock"),
            "new-token",
            r#"{"version":2,"operation":{"type":"head"}}"#
        )["result"],
        "r0"
    );
    signal(&serve, libc::SIGTERM);
    assert!(serve.wait().unwrap().success());
}

#[test]
fn content_grant_bad_options() {
    let f = GrantFixture::new(true);
    let store = f.store.to_str().unwrap();
    let base = |permission: &'static str| {
        vec![
            "--store",
            store,
            "--tenant",
            "t",
            "--actor",
            "a",
            "--permission",
            permission,
            "--scope",
            "",
        ]
    };
    for args in [
        base("owner"),
        vec![
            "--store",
            store,
            "--tenant",
            "t",
            "--permission",
            "read",
            "--scope",
            "",
        ],
        [base("read"), vec!["--while-serving", "yes"]].concat(),
        [base("read"), vec!["--socket", "x"]].concat(),
        [base("read"), vec!["--tenant", "u"]].concat(),
    ] {
        let (ok, json) = f.raw_grant(&args);
        assert!(!ok, "{args:?}");
        assert_eq!(json["error"]["code"], "invalid_request", "{args:?} {json}");
    }
    let (ok, json) = f.raw_grant(&[base("read"), vec!["--while-serving", "true"]].concat());
    assert!(ok, "{json}");
    assert_eq!(json["result"]["added"], true);
}
