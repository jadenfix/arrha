use aether_indexer::PersistentStore;
use aether_sdk::AetherClient;
use aether_types::H256;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::{Arc, RwLock};
use std::time::Duration;
use warp::{http::StatusCode, Filter};

fn response(value: Result<Option<Value>>) -> impl warp::Reply {
    let (body, status) = match value {
        Ok(Some(value)) => (value, StatusCode::OK),
        Ok(None) => (json!({"error":"not found"}), StatusCode::NOT_FOUND),
        Err(error) => (
            json!({"error":error.to_string()}),
            StatusCode::INTERNAL_SERVER_ERROR,
        ),
    };
    warp::reply::with_status(warp::reply::json(&body), status)
}

async fn synchronize(client: &AetherClient, store: &PersistentStore) -> Result<()> {
    store.bind_chain(client.get_chain_id().await?)?;
    let finalized = client.get_finalized_slot().await?;
    let next = store.next_slot()?;
    // Bounded catch-up keeps the query server responsive.
    for slot in next..=finalized.min(next.saturating_add(63)) {
        let block = client.get_full_block_by_number(slot).await?;
        anyhow::ensure!(
            block.is_some() || slot < finalized,
            "finalized head block unavailable; retrying without cursor advancement"
        );
        let mut receipts = Vec::new();
        if let Some(block) = &block {
            for tx in &block.transactions {
                receipts.push(
                    client
                        .get_transaction_receipt(tx.hash())
                        .await?
                        .context("finalized transaction receipt missing")?,
                );
            }
        }
        // A slot with no elected block is legitimate only below the node's
        // finalized cursor. Errors never become empty blocks or advance state.
        store.commit_finalized_slot(slot, block.as_ref(), &receipts)?;
    }
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let port: u16 = std::env::var("INDEXER_PORT")
        .unwrap_or_else(|_| "8081".into())
        .parse()
        .context("invalid INDEXER_PORT")?;
    let db = std::env::var("INDEXER_DB_PATH").unwrap_or_else(|_| "./data/indexer".into());
    let endpoint =
        std::env::var("INDEXER_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8545".into());
    let store = Arc::new(PersistentStore::open(db)?);
    let error = Arc::new(RwLock::new(Some(
        "initial synchronization pending".to_string(),
    )));
    let client = AetherClient::new(endpoint);
    let sync_store = store.clone();
    let sync_error = error.clone();
    let ingestion = tokio::spawn(async move {
        loop {
            let result = synchronize(&client, &sync_store).await;
            *sync_error.write().expect("sync error lock") = result.err().map(|e| e.to_string());
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    });
    let state = warp::any().map(move || store.clone());
    let status_error = error.clone();
    let status=warp::get().and(warp::path("status")).and(warp::path::end()).and(state.clone()).map(move|s:Arc<PersistentStore>| {
        let last_error=status_error.read().expect("sync error lock").clone();
        response((||Ok(Some(json!({"blocks_indexed":s.block_count()?,"latest_slot":s.latest_slot()?,"next_slot":s.next_slot()?,"synchronization_error":last_error}))))())
    });
    let block = warp::get()
        .and(warp::path("block"))
        .and(warp::path::param::<u64>())
        .and(warp::path::end())
        .and(state.clone())
        .map(|slot, s: Arc<PersistentStore>| {
            response(
                s.get_block(slot)
                    .and_then(|v| v.map(serde_json::to_value).transpose().map_err(Into::into)),
            )
        });
    let tx = warp::get()
        .and(warp::path("tx"))
        .and(warp::path::param::<String>())
        .and(warp::path::end())
        .and(state)
        .map(|hash: String, s: Arc<PersistentStore>| {
            let parsed = hex::decode(hash.trim_start_matches("0x"))
                .ok()
                .and_then(|raw| H256::from_slice(&raw).ok());
            match parsed {
                Some(hash) => {
                    let (value, status) = match s.get_receipt(&hash) {
                        Ok(Some(v)) => (json!(v), StatusCode::OK),
                        Ok(None) => (json!({"error":"not found"}), StatusCode::NOT_FOUND),
                        Err(e) => (
                            json!({"error":e.to_string()}),
                            StatusCode::INTERNAL_SERVER_ERROR,
                        ),
                    };
                    warp::reply::with_status(warp::reply::json(&value), status)
                }
                None => warp::reply::with_status(
                    warp::reply::json(
                        &json!({"error":"expected 32-byte hexadecimal transaction hash"}),
                    ),
                    StatusCode::BAD_REQUEST,
                ),
            }
        });
    let routes = status.or(block).or(tx);
    let (_, server) =
        warp::serve(routes).bind_with_graceful_shutdown(([127, 0, 0, 1], port), async {
            let _ = tokio::signal::ctrl_c().await;
        });
    server.await;
    ingestion.abort();
    let _ = ingestion.await;
    Ok(())
}
