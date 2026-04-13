//! Thin Helius RPC client tuned for latency.
//!
//! - HTTP/2 multiplexed over one TLS connection (large h2 windows to avoid
//!   flow-control stalls on parallel fat responses).
//! - gzip + brotli on (JSON-RPC compresses 5-10x).
//! - TCP_NODELAY, generous idle pool.
//! - Retries: 5 tries with exponential backoff on 429 / 5xx / -32429.

use anyhow::{bail, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::types::BalancePoint;

// --- Request types ---

#[derive(Serialize)]
struct RpcRequest {
    jsonrpc: &'static str,
    id: u64,
    method: &'static str,
    params: Vec<serde_json::Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct FullFetchOptions {
    transaction_details: &'static str,
    encoding: &'static str,
    sort_order: &'static str,
    limit: u64,
    max_supported_transaction_version: u64,
    commitment: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pagination_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filters: Option<Filters>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct SigFetchOptions {
    transaction_details: &'static str,
    sort_order: &'static str,
    limit: u64,
    commitment: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pagination_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    filters: Option<Filters>,
}

#[derive(Serialize, Clone)]
pub struct Filters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slot: Option<SlotFilter>,
}

#[derive(Serialize, Clone)]
pub struct SlotFilter {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gte: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lt: Option<u64>,
}

// --- Response types ---

#[derive(Deserialize)]
struct RpcResponse<T> {
    result: Option<T>,
    error: Option<serde_json::Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PageResult {
    pub data: Vec<TxResult>,
    pub pagination_token: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SigPageResult {
    pub data: Vec<SigResult>,
    pub pagination_token: Option<String>,
}

#[derive(Deserialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SigResult {
    pub slot: u64,
    #[allow(dead_code)]
    pub block_time: Option<i64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TxResult {
    pub slot: u64,
    pub block_time: Option<i64>,
    transaction: TxBody,
    meta: Option<TxMeta>,
}

#[derive(Deserialize)]
struct TxBody {
    #[serde(default)]
    signatures: Vec<String>,
    message: TxMessage,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TxMessage {
    account_keys: Vec<AccountKey>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AccountKey {
    Parsed { pubkey: String },
    Plain(String),
}

impl AccountKey {
    fn pubkey(&self) -> &str {
        match self {
            AccountKey::Parsed { pubkey } => pubkey,
            AccountKey::Plain(s) => s,
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct TxMeta {
    pre_balances: Vec<u64>,
    post_balances: Vec<u64>,
    #[serde(default)]
    loaded_addresses: Option<LoadedAddresses>,
}

/// v0 transactions may reference accounts through Address Lookup Tables.
/// pre/post_balances is indexed over the combined order
/// `[static_account_keys..., loaded.writable..., loaded.readonly...]`.
#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct LoadedAddresses {
    #[serde(default)]
    writable: Vec<String>,
    #[serde(default)]
    readonly: Vec<String>,
}

// --- Client ---

pub struct HeliusClient {
    client: Client,
    rpc_url: String,
}

impl HeliusClient {
    pub fn new(rpc_url: String) -> Self {
        let client = Client::builder()
            .http2_adaptive_window(true)
            .http2_initial_stream_window_size(2 * 1024 * 1024)
            .http2_initial_connection_window_size(16 * 1024 * 1024)
            .tcp_nodelay(true)
            .pool_max_idle_per_host(100)
            .pool_idle_timeout(Duration::from_secs(90))
            .timeout(Duration::from_secs(30))
            .gzip(true)
            .brotli(true)
            .build()
            .expect("failed to build HTTP client");
        Self { client, rpc_url }
    }

    /// Opens TCP+TLS+h2 and warms the server's multi-stream scheduler so
    /// the first real parallel fanout doesn't pay cold-path latency.
    /// Designed to run concurrently with the first real fetch via
    /// `tokio::try_join!` — see main.rs.
    pub async fn warm(&self) -> Result<()> {
        self.get_slot().await?;
        let _ = tokio::join!(self.get_slot(), self.get_slot(), self.get_slot());
        Ok(())
    }

    async fn get_slot(&self) -> Result<u64> {
        #[derive(Serialize)]
        struct SlotReq {
            jsonrpc: &'static str,
            id: u64,
            method: &'static str,
        }
        #[derive(Deserialize)]
        struct SlotResp {
            result: Option<u64>,
        }
        let req = SlotReq { jsonrpc: "2.0", id: 1, method: "getSlot" };
        let resp: SlotResp = self
            .client
            .post(&self.rpc_url)
            .json(&req)
            .send()
            .await?
            .json()
            .await?;
        resp.result.ok_or_else(|| anyhow::anyhow!("getSlot: empty result"))
    }

    /// Full-mode page: up to `limit` transactions with meta (pre/post balances).
    pub async fn fetch_page(
        &self,
        address: &str,
        sort_order: &'static str,
        limit: u64,
        pagination_token: Option<String>,
        filters: Option<Filters>,
    ) -> Result<PageResult> {
        let options = FullFetchOptions {
            transaction_details: "full",
            encoding: "jsonParsed",
            sort_order,
            limit,
            max_supported_transaction_version: 0,
            commitment: "confirmed",
            pagination_token,
            filters,
        };
        let params = vec![
            serde_json::Value::String(address.to_string()),
            serde_json::to_value(options)?,
        ];
        self.call("getTransactionsForAddress", params).await
    }

    /// Signatures-mode page: up to `limit` (slot, blockTime) rows. ~10x
    /// smaller responses than full-mode for the same credit cost.
    pub async fn fetch_signatures(
        &self,
        address: &str,
        sort_order: &'static str,
        limit: u64,
        pagination_token: Option<String>,
        filters: Option<Filters>,
    ) -> Result<SigPageResult> {
        let options = SigFetchOptions {
            transaction_details: "signatures",
            sort_order,
            limit,
            commitment: "confirmed",
            pagination_token,
            filters,
        };
        let params = vec![
            serde_json::Value::String(address.to_string()),
            serde_json::to_value(options)?,
        ];
        self.call("getTransactionsForAddress", params).await
    }

    /// Shared JSON-RPC call path with retry on 429 / 5xx / RPC -32429.
    async fn call<T: for<'de> Deserialize<'de> + Default>(
        &self,
        method: &'static str,
        params: Vec<serde_json::Value>,
    ) -> Result<T> {
        let request = RpcRequest { jsonrpc: "2.0", id: 1, method, params };
        for attempt in 0..5u32 {
            let http = self.client.post(&self.rpc_url).json(&request).send().await?;
            let status = http.status();
            if status == 429 || status.is_server_error() {
                if attempt < 4 {
                    let ms = 100 * 2u64.pow(attempt);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    continue;
                }
                bail!("HTTP {status} after {attempt} retries");
            }
            let resp: RpcResponse<T> = match http.json().await {
                Ok(r) => r,
                Err(_) if attempt < 4 => {
                    let ms = 300 * 2u64.pow(attempt);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    continue;
                }
                Err(e) => bail!("JSON decode error: {e}"),
            };
            if let Some(err) = resp.error {
                let code = err.get("code").and_then(|c| c.as_i64()).unwrap_or(0);
                if code == -32429 && attempt < 4 {
                    let ms = 150 * 2u64.pow(attempt);
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                    continue;
                }
                bail!("RPC error: {err}");
            }
            return Ok(resp.result.unwrap_or_default());
        }
        bail!("RPC request failed after retries")
    }
}

impl Default for PageResult {
    fn default() -> Self {
        Self { data: vec![], pagination_token: None }
    }
}

impl Default for SigPageResult {
    fn default() -> Self {
        Self { data: vec![], pagination_token: None }
    }
}

impl TxResult {
    /// Extract the wallet's pre/post lamports from this tx. Returns None
    /// if the wallet isn't referenced. Handles v0 Address Lookup Tables
    /// via `meta.loadedAddresses` as a fallback if the wallet isn't in
    /// the static accountKeys — pre/post_balances is indexed over the
    /// combined list `[static..., loaded.writable..., loaded.readonly...]`.
    pub fn extract_balance(&self, address: &str) -> Option<BalancePoint> {
        let meta = self.meta.as_ref()?;
        let static_keys = &self.transaction.message.account_keys;

        let idx = if let Some(i) = static_keys.iter().position(|k| k.pubkey() == address) {
            i
        } else if let Some(loaded) = &meta.loaded_addresses {
            let base = static_keys.len();
            if let Some(j) = loaded.writable.iter().position(|s| s == address) {
                base + j
            } else if let Some(j) = loaded.readonly.iter().position(|s| s == address) {
                base + loaded.writable.len() + j
            } else {
                return None;
            }
        } else {
            return None;
        };

        if idx >= meta.pre_balances.len() || idx >= meta.post_balances.len() {
            return None;
        }

        Some(BalancePoint {
            slot: self.slot,
            block_time: self.block_time,
            pre_lamports: meta.pre_balances[idx],
            post_lamports: meta.post_balances[idx],
            signature: self
                .transaction
                .signatures
                .first()
                .cloned()
                .unwrap_or_default(),
        })
    }
}
