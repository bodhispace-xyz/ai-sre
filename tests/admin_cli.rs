//! Exercises operator CLI exit status against a local framed server without granting recovery authority.

use std::{
    io::{Read, Write},
    os::unix::net::UnixListener,
    process::Command,
};

#[test]
fn acknowledgement_exit_status_requires_explicit_confirmation() {
    // Given a bounded framed server that either accepts or refuses an exact handoff pickup.
    for confirmed in [false, true] {
        let root = std::env::temp_dir().join(format!("ack-cli-{}-{confirmed}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let socket = root.join("admin.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            let mut length = [0; 4];
            stream.read_exact(&mut length).unwrap();
            let size = u32::from_be_bytes(length) as usize;
            assert!(size <= 4096);
            let mut request = vec![0; size];
            stream.read_exact(&mut request).unwrap();
            let request: serde_json::Value = serde_json::from_slice(&request).unwrap();
            assert_eq!(
                request,
                serde_json::json!({"operation":"acknowledge","handoff_digest":"exact-handoff"})
            );
            let response = serde_json::to_vec(&serde_json::json!({"acknowledged": confirmed, "approved":false, "dispatched":false})).unwrap();
            stream
                .write_all(&(response.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&response).unwrap();
        });
        // When the actual operator CLI asks to acknowledge, without sending an identity or approval.
        let output = Command::new(env!("CARGO_BIN_EXE_ai-sre-admin"))
            .args(["acknowledge", socket.to_str().unwrap(), "exact-handoff"])
            .output()
            .unwrap();
        server.join().unwrap();
        // Then only explicit confirmation is a successful command and neither outcome claims approval.
        assert_eq!(output.status.success(), confirmed);
        let reply: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(reply["approved"], false);
        std::fs::remove_file(socket).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}

#[test]
fn rejected_recovery_is_not_a_successful_cli_exit() {
    // Given a reachable server that declines a stale recovery request.
    let root = std::env::temp_dir().join(format!("admin-cli-{}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let socket = root.join("admin.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let mut length = [0; 4];
        stream.read_exact(&mut length).unwrap();
        let mut request = vec![0; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut request).unwrap();
        let response = br#"{"recovered":false,"dispatched":false,"readiness":"not_assessed"}"#;
        stream
            .write_all(&(response.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(response).unwrap();
    });
    // When the actual CLI requests recovery.
    let output = Command::new(env!("CARGO_BIN_EXE_ai-sre-admin"))
        .args([
            "recover",
            socket.to_str().unwrap(),
            "candidate",
            "stale",
            "operator inspected",
        ])
        .output()
        .unwrap();
    server.join().unwrap();
    std::fs::remove_file(socket).unwrap();
    std::fs::remove_dir(root).unwrap();
    // Then scripts see an unsuccessful outcome while stdout retains the structured response.
    assert!(!output.status.success());
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()["recovered"],
        false
    );
}
