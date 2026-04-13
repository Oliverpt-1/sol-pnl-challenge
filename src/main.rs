mod algorithm;
mod rpc;
mod types;

use anyhow::Result;
use clap::Parser;
use serde::Serialize;
use std::time::Instant;

use rpc::HeliusClient;
use types::BalancePoint;

/// Compute a Solana wallet's SOL balance timeline using only Helius RPC.
/// Submission for Mert's Solana dev weekend competition.
#[derive(Parser)]
#[command(name = "sol-pnl-challenge")]
struct Args {
    /// Wallet address (base58).
    #[arg(short, long)]
    address: String,

    /// Helius RPC URL, including API key.
    #[arg(short, long)]
    rpc_url: String,

    /// Include the full per-tx trajectory in output.
    #[arg(short, long)]
    trajectory: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    address: String,
    latency_ms: f64,
    transaction_count: usize,
    pnl_lamports: i128,
    pnl_sol: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    trajectory: Option<Vec<TrajPoint>>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct TrajPoint {
    slot: u64,
    block_time: Option<i64>,
    post_sol: f64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = HeliusClient::new(args.rpc_url);

    // warm() runs concurrently with compute() — the first request to the
    // connection pool opens TCP+TLS+h2, everything else multiplexes on top.
    let start = Instant::now();
    let (_, balances) = tokio::try_join!(
        client.warm(),
        algorithm::compute(&client, &args.address),
    )?;
    let latency_ms = start.elapsed().as_secs_f64() * 1000.0;

    let pnl_lamports: i128 = balances.iter().map(BalancePoint::delta_lamports).sum();
    let pnl_sol = pnl_lamports as f64 / 1e9;

    let out = Output {
        address: args.address,
        latency_ms,
        transaction_count: balances.len(),
        pnl_lamports,
        pnl_sol,
        trajectory: if args.trajectory {
            Some(
                balances
                    .iter()
                    .map(|b| TrajPoint {
                        slot: b.slot,
                        block_time: b.block_time,
                        post_sol: b.post_sol(),
                    })
                    .collect(),
            )
        } else {
            None
        },
    };

    println!("{}", serde_json::to_string_pretty(&out)?);
    Ok(())
}
