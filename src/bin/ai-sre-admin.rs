//! Local operator client for reviewing repair artifacts and explicitly reopening validation eligibility.
//!
//! The server authenticates the Unix peer; this client carries no credentials, grants no approval,
//! and never opens a PR or dispatches validation. Recovery reasons must not contain secrets.

use std::{io, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> io::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let (socket, request) = match arguments.as_slice() {
        [operation, socket, handoff] if operation == "acknowledge" => (
            socket,
            serde_json::json!({"operation":"acknowledge","handoff_digest":handoff}),
        ),
        [operation, socket, artifact] if operation == "inspect" => (
            socket,
            serde_json::json!({"operation":"inspect","artifact":artifact}),
        ),
        [operation, socket, artifact, expected, reason] if operation == "recover" => (
            socket,
            serde_json::json!({"operation":"recover","artifact":artifact,"expected_request_digest":expected,"reason":reason}),
        ),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "usage: ai-sre-admin inspect SOCKET ARTIFACT | recover SOCKET ARTIFACT EXPECTED_REQUEST_DIGEST 'REASON' | acknowledge SOCKET HANDOFF_DIGEST",
            ));
        }
    };
    let bytes = serde_json::to_vec(&request)?;
    if bytes.len() > 4096 {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    let response = tokio::time::timeout(Duration::from_secs(15), async {
        let mut stream = UnixStream::connect(socket).await?;
        stream.write_u32(bytes.len() as u32).await?;
        stream.write_all(&bytes).await?;
        let size = stream.read_u32().await? as usize;
        if size == 0 || size > 4 * 1024 * 1024 {
            return Err(io::Error::from(io::ErrorKind::InvalidData));
        }
        let mut response = vec![0; size];
        stream.read_exact(&mut response).await?;
        let mut trailing = [0; 1];
        if stream.read(&mut trailing).await? != 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        serde_json::from_slice::<serde_json::Value>(&response).map_err(io::Error::from)
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            "reply unknown; inspect history before retrying",
        )
    })??;
    // JSON escaping keeps patch/reason bytes from becoming terminal control sequences.
    println!("{}", serde_json::to_string_pretty(&response)?);
    if request["operation"] == "acknowledge" && response["acknowledged"] != true {
        return Err(io::Error::other(
            "handoff pickup not confirmed; inspect before retrying",
        ));
    }
    if request["operation"] == "recover" && response["recovered"] != true {
        return Err(io::Error::other(
            "recovery not confirmed; inspect the current attempt before retrying",
        ));
    }
    Ok(())
}
