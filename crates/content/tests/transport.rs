#![cfg(unix)]
use mnemosyne_content::{
    api::{Operation, Request, Response},
    auth::{ActorKind, Grant, Permission, Policy, TrustedIdentity},
    transport::{Envelope, Server},
    Service,
};
use std::{
    io::{Read, Write},
    os::unix::{fs::PermissionsExt, net::UnixStream},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::{Duration, SystemTime},
};

#[test]
fn accepted_socket_waits_for_delayed_frame_instead_of_inheriting_nonblocking_listener() {
    let root = std::env::temp_dir().join(format!(
        "mct-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
            % 1_000_000
    ));
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let who = TrustedIdentity {
        tenant_id: "tenant".into(),
        actor_id: "human".into(),
        kind: ActorKind::Human,
        on_behalf_of: None,
    };
    Service::open(root.join("store"))
        .unwrap()
        .bootstrap_policy(Policy {
            tenants: Default::default(),
            grants: vec![Grant {
                tenant_id: who.tenant_id.clone(),
                actor_id: who.actor_id.clone(),
                permission: Permission::Read,
                scope: String::new(),
            }],
        })
        .unwrap();
    let credentials = root.join("credentials.json");
    std::fs::write(
        &credentials,
        serde_json::to_vec(
            &serde_json::json!({"credentials":[{"token":"test-token","identity":who}]}),
        )
        .unwrap(),
    )
    .unwrap();
    let socket = root.join("content.sock");
    let lock = mnemosyne_content::grant::ServeLock::try_acquire(&root.join("store"))
        .unwrap()
        .unwrap();
    let server = Server::bind(&root.join("store"), &credentials, &socket, lock).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let server_stop = stop.clone();
    let thread = std::thread::spawn(move || server.run(server_stop));
    let result = (|| -> Result<Response, Box<dyn std::error::Error>> {
        let mut stream = UnixStream::connect(&socket)?;
        stream.set_read_timeout(Some(Duration::from_secs(4)))?;
        std::thread::sleep(Duration::from_millis(150));
        let body = serde_json::to_vec(&Envelope {
            token: "test-token".into(),
            request: Request {
                version: 2,
                idempotency_key: None,
                operation: Operation::Head,
            },
        })?;
        stream.write_all(&(body.len() as u32).to_be_bytes())?;
        stream.write_all(&body)?;
        let mut header = [0; 4];
        stream.read_exact(&mut header)?;
        let length = u32::from_be_bytes(header) as usize;
        if length > 4096 {
            return Err("unexpected response length".into());
        }
        let mut bytes = vec![0; length];
        stream.read_exact(&mut bytes)?;
        Ok(serde_json::from_slice(&bytes)?)
    })();
    stop.store(true, Ordering::SeqCst);
    thread.join().unwrap().unwrap();
    std::fs::remove_dir_all(&root).unwrap();
    let response = serde_json::to_value(result.unwrap()).unwrap();
    assert_eq!(response["result"], "r0");
    assert!(response.get("error").is_none());
}
