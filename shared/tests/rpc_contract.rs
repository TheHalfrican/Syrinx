//! Contract-layer test for the RPC client: stand up a fake WebSocket server
//! in-process and drive the real `EngineClient` RPC transport through it.
//!
//! Exercises the load-bearing pieces of RPC-PROTOCOL.md: the first-message
//! `Authenticate` handshake (§2.3), one method round-trip (§4), one
//! notification → `EngineEvent` (§6/§10), and one `-32000` error response whose
//! `error.message` survives verbatim into `EngineError` (§7.2). Uses only the
//! crate's public API, via the `SYRINX_RPC_ENDPOINT` discovery override (§2.2).

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use syrinx_shared::{EngineClient, EngineEvent};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn rpc_client_handshake_roundtrip_notification_and_error() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let token = "b9f1c0e2a7d34f5c8e1b6a09d2f47e3c";

    // A discovery file the client will read via the env override.
    let dir = std::env::temp_dir().join(format!("syrinx-rpc-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let disc = dir.join("rpc.json");
    std::fs::write(
        &disc,
        format!(
            r#"{{"protocol":1,"port":{port},"token":"{token}","pid":1,"url":"ws://127.0.0.1:{port}"}}"#
        ),
    )
    .unwrap();
    std::env::set_var("SYRINX_RPC_ENDPOINT", &disc);

    // The fake server: authenticate, push one notification, then answer
    // requests (a JSON-string result for ListModels, a -32000 for CreateProfile).
    let server = tokio::spawn(async move {
        let (tcp, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();

        let first = ws.next().await.unwrap().unwrap();
        let v: Value = serde_json::from_str(first.to_text().unwrap()).unwrap();
        assert_eq!(v["method"], "Authenticate");
        assert_eq!(v["params"][0], token);
        assert_eq!(v["id"], 0);
        ws.send(Message::Text(json!({"jsonrpc":"2.0","result":true,"id":0}).to_string()))
            .await
            .unwrap();

        // an id-less notification the client must decode into an EngineEvent
        ws.send(Message::Text(
            json!({"jsonrpc":"2.0","method":"GenerationProgress","params":[7,"synthesizing",0.25]})
                .to_string(),
        ))
        .await
        .unwrap();

        while let Some(Ok(msg)) = ws.next().await {
            if !msg.is_text() {
                continue;
            }
            let v: Value = serde_json::from_str(msg.to_text().unwrap()).unwrap();
            let id = v["id"].clone();
            let reply = match v["method"].as_str().unwrap() {
                "ListModels" => json!({"jsonrpc":"2.0","result":"[]","id":id}),
                "CreateProfile" => json!({
                    "jsonrpc":"2.0",
                    "error":{"code":-32000,"message":"UNIQUE constraint failed: profiles.name","data":{"type":"IntegrityError"}},
                    "id":id
                }),
                _ => json!({"jsonrpc":"2.0","result":null,"id":id}),
            };
            ws.send(Message::Text(reply.to_string())).await.unwrap();
        }
    });

    let client = EngineClient::connect_rpc().await.unwrap();
    let mut events = client.events();

    // method round-trip: positional params out, typed result back
    assert_eq!(client.list_models().await.unwrap(), "[]");

    // -32000 → EngineError whose Display carries the raw text verbatim, so the
    // app's profile_err_msg substring check keeps matching on this transport.
    let err = client.create_profile("{}").await.unwrap_err();
    assert!(err.to_string().contains("UNIQUE constraint failed: profiles.name"));

    // notification → EngineEvent
    match events.recv().await.unwrap() {
        EngineEvent::GenerationProgress { gen_id, state, pct } => {
            assert_eq!(gen_id, 7);
            assert_eq!(state, "synthesizing");
            assert_eq!(pct, 0.25);
        }
        other => panic!("unexpected event: {other:?}"),
    }

    drop(client);
    // Splitting the socket means dropping the client's write half does not
    // close it (the read pump still holds the other half), so the server's
    // read loop would block forever — abort it rather than await.
    server.abort();
    std::env::remove_var("SYRINX_RPC_ENDPOINT");
    let _ = std::fs::remove_dir_all(&dir);
}
