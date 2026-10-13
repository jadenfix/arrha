use aether_sdk::AetherClient;
use aether_types::{Address, Block, VrfProof, H256};
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

async fn endpoint(result: Value) -> (AetherClient, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0u8; 16384];
        let count = stream.read(&mut request).await.unwrap();
        assert!(count > 0);
        let body = json!({"jsonrpc":"2.0", "id":1, "result":result}).to_string();
        let response = format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body);
        stream.write_all(response.as_bytes()).await.unwrap();
    });
    (AetherClient::new(url), server)
}

#[tokio::test]
async fn null_block_is_absence_not_transport_failure() {
    let (client, server) = endpoint(Value::Null).await;
    assert!(client.get_full_block_by_number(42).await.unwrap().is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn null_receipt_is_absence_not_transport_failure() {
    let (client, server) = endpoint(Value::Null).await;
    assert!(client
        .get_transaction_receipt(H256::zero())
        .await
        .unwrap()
        .is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn canonical_nested_block_decodes_without_fabricated_summary_fields() {
    let block = Block::new(
        42,
        H256::zero(),
        Address::from([1; 20]),
        VrfProof {
            output: [0; 32],
            proof: vec![],
        },
        vec![],
    );
    let (client, server) = endpoint(serde_json::to_value(&block).unwrap()).await;
    let fetched = client.get_full_block_by_number(42).await.unwrap().unwrap();
    assert_eq!(fetched.hash(), block.hash());
    assert_eq!(fetched.header.slot, 42);
    server.await.unwrap();
}

#[tokio::test]
async fn public_block_summary_uses_the_real_header_wire_shape() {
    let block = Block::new(
        42,
        H256::zero(),
        Address::from([1; 20]),
        VrfProof {
            output: [0; 32],
            proof: vec![],
        },
        vec![],
    );
    let (client, server) = endpoint(serde_json::to_value(&block).unwrap()).await;
    let summary = client
        .get_block_by_number(Some(42), true)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(summary.hash, block.hash());
    assert_eq!(summary.slot, 42);
    server.await.unwrap();
}
