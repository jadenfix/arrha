use aether_indexer::{ChainIdentity, PersistentStore};
use aether_sdk::{types::ClientConfig, AetherClient};
use aether_types::Block;
use anyhow::{anyhow, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;

#[derive(Clone, Debug, Serialize)]
struct IndexerStatus {
    status: String,
    rpc_url: String,
    chain_id: Option<u64>,
    genesis_hash: Option<String>,
    chain_identity: Option<String>,
    finalized_slot: Option<u64>,
    finalized_hash: Option<String>,
    indexed_slot: Option<u64>,
    blocks_indexed: usize,
    next_slot_to_index: u64,
    caught_up: bool,
    last_success_unix_secs: Option<u64>,
    last_error: Option<String>,
}

impl IndexerStatus {
    fn new(rpc_url: String, blocks_indexed: usize, indexed_slot: Option<u64>) -> Self {
        Self {
            status: "starting".to_string(),
            rpc_url,
            chain_id: None,
            genesis_hash: None,
            chain_identity: None,
            finalized_slot: None,
            finalized_hash: None,
            indexed_slot,
            blocks_indexed,
            next_slot_to_index: indexed_slot.map_or(0, |slot| slot.saturating_add(1)),
            caught_up: false,
            last_success_unix_secs: None,
            last_error: None,
        }
    }
}

fn env_u64(name: &str, default: u64, min: u64, max: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (min..=max).contains(value))
        .unwrap_or(default)
}

fn unix_time_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn normalize_hash(value: &str, field: &str) -> Result<String> {
    let raw = value.strip_prefix("0x").unwrap_or(value);
    if raw.len() != 64 || !raw.chars().all(|character| character.is_ascii_hexdigit()) {
        return Err(anyhow!("{field} must be a 32-byte hex hash"));
    }
    Ok(format!("0x{}", raw.to_ascii_lowercase()))
}

async fn rpc_node_info(client: &AetherClient) -> Result<(u64, String, ChainIdentity)> {
    let info = client
        .call_rpc("aeth_getNodeInfo", Vec::new())
        .await
        .context("aeth_getNodeInfo request failed")?;
    let finalized_slot = info
        .get("finalizedSlot")
        .and_then(Value::as_u64)
        .context("aeth_getNodeInfo response has no numeric finalizedSlot")?;
    let finalized_hash = info
        .get("finalizedHash")
        .and_then(Value::as_str)
        .context("aeth_getNodeInfo response has no finalizedHash")?;
    let genesis_hash = info
        .get("genesisHash")
        .and_then(Value::as_str)
        .context("aeth_getNodeInfo response has no genesisHash")?;
    let chain_identity = info
        .get("chainIdentity")
        .and_then(Value::as_str)
        .context("aeth_getNodeInfo response has no chainIdentity")?;
    let chain_id = client
        .chain_id()
        .await
        .context("aeth_chainId request failed")?;
    if let Some(info_chain_id) = info.get("chainId").and_then(Value::as_u64) {
        if info_chain_id != chain_id {
            return Err(anyhow!(
                "node chain ID mismatch between aeth_chainId ({chain_id}) and aeth_getNodeInfo ({info_chain_id})"
            ));
        }
    }
    let identity = ChainIdentity {
        chain_id,
        genesis_hash: normalize_hash(genesis_hash, "genesisHash")?,
        chain_identity: normalize_hash(chain_identity, "chainIdentity")?,
    };
    Ok((
        finalized_slot,
        normalize_hash(finalized_hash, "finalizedHash")?,
        identity,
    ))
}

async fn verify_persisted_checkpoint(
    client: &AetherClient,
    store: &PersistentStore,
    finalized_slot: u64,
) -> Result<()> {
    let Some(checkpoint) = store
        .checkpoint()
        .context("read persisted index checkpoint")?
    else {
        return Ok(());
    };
    let checkpoint_slot = checkpoint.slot;
    if checkpoint_slot > finalized_slot {
        return Err(anyhow!(
            "index checkpoint slot {checkpoint_slot} is ahead of node finalized slot {finalized_slot}"
        ));
    }
    let remote = fetch_finalized_block(client, checkpoint_slot)
        .await?
        .ok_or_else(|| {
            anyhow!("node has no finalized block at index checkpoint slot {checkpoint_slot}")
        })?;
    let remote_hash = remote.hash();
    if remote_hash != checkpoint.hash {
        return Err(anyhow!(
            "index checkpoint mismatch at slot {checkpoint_slot}: stored {}, node reports {:?}",
            checkpoint.hash,
            remote_hash
        ));
    }
    Ok(())
}

async fn fetch_finalized_block(client: &AetherClient, slot: u64) -> Result<Option<Block>> {
    let value = client
        .call_rpc(
            "aeth_getBlockByNumber",
            vec![json!(slot.to_string()), json!(true)],
        )
        .await
        .with_context(|| format!("failed to fetch finalized block at slot {slot}"))?;
    if value.is_null() {
        return Ok(None);
    }
    let block: Block = serde_json::from_value(value)
        .with_context(|| format!("node returned an invalid block at slot {slot}"))?;
    if block.header.slot != slot {
        return Err(anyhow!(
            "node returned block slot {} while requesting slot {slot}",
            block.header.slot
        ));
    }
    Ok(Some(block))
}

async fn ingest_loop(
    client: AetherClient,
    store: Arc<PersistentStore>,
    status: Arc<RwLock<IndexerStatus>>,
    poll_interval: Duration,
    retry_interval: Duration,
) -> Result<()> {
    let initial_count = store.block_count().context("read index block count")?;
    let latest_indexed_slot = if initial_count == 0 {
        None
    } else {
        Some(store.latest_slot().context("read latest indexed slot")?)
    };
    let mut next_slot = latest_indexed_slot.map_or(0, |slot| slot.saturating_add(1));
    let mut indexed_count = initial_count;
    {
        let mut current = status.write().await;
        current.blocks_indexed = initial_count;
        current.indexed_slot = latest_indexed_slot;
        current.next_slot_to_index = next_slot;
    }

    loop {
        let cycle = async {
            let (finalized_slot, finalized_hash, identity) = rpc_node_info(&client).await?;
            store
                .ensure_chain_identity(&identity)
                .context("verify index database chain identity")?;
            verify_persisted_checkpoint(&client, &store, finalized_slot)
                .await
                .context("verify index database checkpoint against node")?;
            {
                let mut current = status.write().await;
                current.status = "catching_up".to_string();
                current.chain_id = Some(identity.chain_id);
                current.genesis_hash = Some(identity.genesis_hash.clone());
                current.chain_identity = Some(identity.chain_identity.clone());
                current.finalized_slot = Some(finalized_slot);
                current.finalized_hash = Some(finalized_hash.clone());
                current.caught_up = false;
                current.last_error = None;
                current.last_success_unix_secs = Some(unix_time_secs());
            }

            while next_slot <= finalized_slot {
                let Some(block) = fetch_finalized_block(&client, next_slot).await? else {
                    if next_slot == finalized_slot {
                        if finalized_slot == 0
                            && finalized_hash.eq_ignore_ascii_case(
                                "0x0000000000000000000000000000000000000000000000000000000000000000",
                            )
                        {
                            next_slot = next_slot.saturating_add(1);
                            break;
                        }
                        return Err(anyhow!(
                            "node reports finalized hash {finalized_hash} at slot {finalized_slot}, but returned no block"
                        ));
                    }
                    next_slot += 1;
                    status.write().await.next_slot_to_index = next_slot;
                    continue;
                };
                if next_slot == finalized_slot {
                    let computed_hash = format!("{:?}", block.hash());
                    if !computed_hash.eq_ignore_ascii_case(&finalized_hash) {
                        return Err(anyhow!(
                            "finalized block hash mismatch at slot {finalized_slot}: node reports {finalized_hash}, block computes {computed_hash}"
                        ));
                    }
                }

                let store_for_write = store.clone();
                tokio::task::spawn_blocking(move || store_for_write.ingest(&block))
                    .await
                    .context("index write task panicked")??;

                indexed_count += 1;
                {
                    let mut current = status.write().await;
                    current.indexed_slot = Some(next_slot);
                    current.blocks_indexed = indexed_count;
                    current.last_success_unix_secs = Some(unix_time_secs());
                }
                println!("Indexed finalized block slot={next_slot} blocks={indexed_count}");

                if next_slot == finalized_slot {
                    next_slot = next_slot.saturating_add(1);
                    break;
                }
                next_slot += 1;
                status.write().await.next_slot_to_index = next_slot;
            }

            let mut current = status.write().await;
            current.next_slot_to_index = next_slot;
            current.caught_up = true;
            current.status = "caught_up".to_string();
            current.last_error = None;
            current.last_success_unix_secs = Some(unix_time_secs());
            Ok::<(), anyhow::Error>(())
        }
        .await;

        if let Err(error) = cycle {
            let detail = format!("{error:#}");
            eprintln!("Indexer ingestion failed; retrying: {detail}");
            let mut current = status.write().await;
            current.status = "degraded".to_string();
            current.caught_up = false;
            current.next_slot_to_index = next_slot;
            current.last_error = Some(detail);
            drop(current);
            tokio::time::sleep(retry_interval).await;
        } else {
            tokio::time::sleep(poll_interval).await;
        }
    }
}

async fn run_query_api(
    store: Arc<PersistentStore>,
    status: Arc<RwLock<IndexerStatus>>,
    bind_ip: IpAddr,
    port: u16,
) -> Result<()> {
    use warp::Filter;

    let status_filter = warp::any().map(move || status.clone());
    let store_filter = warp::any().map(move || store.clone());

    let status_route = warp::path("status")
        .and(warp::get())
        .and(warp::path::end())
        .and(status_filter)
        .and_then(|status: Arc<RwLock<IndexerStatus>>| async move {
            let current = status.read().await.clone();
            let code = if current.status == "caught_up" && current.last_error.is_none() {
                warp::http::StatusCode::OK
            } else {
                warp::http::StatusCode::SERVICE_UNAVAILABLE
            };
            Ok::<_, std::convert::Infallible>(warp::reply::with_status(
                warp::reply::json(&current),
                code,
            ))
        });

    let block_route = warp::path("block")
        .and(warp::path::param::<u64>())
        .and(warp::get())
        .and(warp::path::end())
        .and(store_filter)
        .and_then(|slot: u64, store: Arc<PersistentStore>| async move {
            let result = tokio::task::spawn_blocking(move || store.get_block(slot)).await;
            let reply = match result {
                Ok(Ok(Some(block))) => warp::reply::with_status(
                    warp::reply::json(&json!(block)),
                    warp::http::StatusCode::OK,
                ),
                Ok(Ok(None)) => warp::reply::with_status(
                    warp::reply::json(&Value::Null),
                    warp::http::StatusCode::NOT_FOUND,
                ),
                Ok(Err(error)) => warp::reply::with_status(
                    warp::reply::json(&json!({"error": format!("{error:#}")})),
                    warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                ),
                Err(error) => warp::reply::with_status(
                    warp::reply::json(
                        &json!({"error": format!("block query task failed: {error}")}),
                    ),
                    warp::http::StatusCode::INTERNAL_SERVER_ERROR,
                ),
            };
            Ok::<_, std::convert::Infallible>(reply)
        });

    let routes = status_route.or(block_route);
    println!("Indexer query API listening on http://{bind_ip}:{port}");
    warp::serve(routes).run((bind_ip, port)).await;
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let rpc_url =
        std::env::var("AETHER_RPC_URL").unwrap_or_else(|_| "http://127.0.0.1:8545".to_string());
    let db_path = std::env::var_os("INDEXER_DB_PATH")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/data/indexer"));
    let query_port = env_u64("INDEXER_PORT", 8081, 1, u16::MAX as u64) as u16;
    let request_timeout_secs = env_u64("INDEXER_RPC_TIMEOUT_SECS", 3, 1, 30);
    let poll_interval =
        Duration::from_millis(env_u64("INDEXER_POLL_INTERVAL_MS", 1_000, 250, 60_000));
    let retry_interval =
        Duration::from_millis(env_u64("INDEXER_RETRY_INTERVAL_MS", 2_000, 250, 60_000));
    let bind_ip = std::env::var("INDEXER_BIND_IP")
        .ok()
        .and_then(|value| value.parse::<IpAddr>().ok())
        .unwrap_or(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));

    let store = Arc::new(
        PersistentStore::open(&db_path)
            .with_context(|| format!("open indexer database at {}", db_path.display()))?,
    );
    let client = AetherClient::with_config(
        rpc_url.clone(),
        ClientConfig {
            request_timeout_secs,
            ..ClientConfig::default()
        },
    );
    let count = store.block_count().context("read index block count")?;
    let indexed_slot = if count == 0 {
        None
    } else {
        Some(store.latest_slot().context("read latest indexed slot")?)
    };
    let status = Arc::new(RwLock::new(IndexerStatus::new(
        rpc_url,
        count,
        indexed_slot,
    )));

    let mut ingestion = tokio::spawn(ingest_loop(
        client,
        store.clone(),
        status.clone(),
        poll_interval,
        retry_interval,
    ));
    // Keep Warp's server future on this task. Warp 0.4's future is not
    // necessarily `Send`, so spawning it would impose an unnecessary bound.
    let api = run_query_api(store, status, bind_ip, query_port);
    tokio::pin!(api);

    tokio::select! {
        result = &mut ingestion => result.context("indexer ingestion task panicked")??,
        result = &mut api => {
            ingestion.abort();
            let _ = ingestion.await;
            result?;
        },
    }
    Ok(())
}
