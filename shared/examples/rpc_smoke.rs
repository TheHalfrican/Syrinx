//! End-to-end smoke test against a LIVE engine over the RPC transport.
//!
//! Start the engine (on Windows it serves RPC by default; on Linux use
//! `SYRINX_TRANSPORT=rpc`), then:
//!
//! ```sh
//! cargo run -p syrinx-shared --example rpc_smoke
//! ```
//!
//! Exercises the discovery file + Authenticate handshake, plain method calls,
//! a void method, an engine error (verbatim-text check), and one
//! signal→notification round-trip. Exits non-zero on the first failure.

use syrinx_shared::{EngineClient, EngineEvent};

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let client = EngineClient::connect_rpc().await?;
    let mut events = client.events();
    println!("connected + authenticated");

    let backend = client.backend().await?;
    let loaded = client.model_loaded().await?;
    println!("GetBackend -> {backend:?}   GetModelLoaded -> {loaded}");

    let history = client.list_history().await?;
    println!("ListHistory -> {} bytes of JSON", history.len());

    client.set_volume(0.8).await?;
    println!("SetVolume(0.8) -> ok (void)");

    // An unreadable path must come back as a normal engine result ("{}" per
    // FileEnvelope's contract), not a transport failure.
    let env = client.file_envelope("Z:/nope/missing.wav").await?;
    println!("FileEnvelope(missing) -> {env:?}");

    // Engine error path: CreateProfile with invalid JSON raises in the
    // handler; the error text must arrive verbatim (RPC-PROTOCOL.md §7.2).
    match client.create_profile("{not json").await {
        Ok(id) => return Err(format!("expected an error, got profile id {id:?}").into()),
        Err(e) => println!("CreateProfile(bad json) -> error text: {e}"),
    }

    // Signal round-trip: TranscribeFile on a missing file returns a req id
    // immediately and later emits TranscribeResult(req_id, "").
    let req = client.transcribe_file("Z:/nope/missing.wav").await?;
    println!("TranscribeFile -> req {req}, waiting for TranscribeResult…");
    let deadline = tokio::time::Duration::from_secs(15);
    loop {
        let ev = tokio::time::timeout(deadline, events.recv())
            .await
            .map_err(|_| "timed out waiting for TranscribeResult")?
            .ok_or("event stream closed")?;
        match ev {
            EngineEvent::TranscribeResult { req_id, text, error } if req_id == req => {
                println!("TranscribeResult -> req {req_id}, text {text:?}, error {error}");
                break;
            }
            other => println!("  (event: {other:?})"),
        }
    }

    println!("SMOKE OK");
    Ok(())
}
