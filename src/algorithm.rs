//! Scout: sigs-first adaptive scan, streamed into a parallel full-fetch pool.
//!
//! The core insight: `getTransactionsForAddress` with `details=signatures`
//! returns 1000 rows per call at the same credit cost as a 100-row full
//! call. Sigs let us learn wallet density cheaply before committing to
//! the heavy full-tx downloads.
//!
//! Phases:
//!
//!   Phase 0 — 1 RTT, 2 parallel calls:
//!     - `sigs(asc,  1000)` → oldest 1000 sigs ("jackpot")
//!     - `sigs(desc, 1000)` → newest 1000 sigs ("tail", and slot_max)
//!     Together these pin both ends of the wallet's active range and
//!     give us 2 density anchors. For <2000-tx wallets, phase 0 already
//!     has every sig we need.
//!
//!   Phase 1 — scout the middle gap (busy wallets only):
//!     Partition `(last_jackpot_slot, first_tail_slot)` into N adaptive
//!     slices (clamped 4..=12) and fire `sigs(asc, 1000)` per slice in
//!     parallel. Each scout partition paginates if dense.
//!
//!   Phase 2 — full-mode fetch:
//!     Chunk the known sigs by slot into ~50-sig ranges, fire each as
//!     `full(asc, 100)` slot-filtered pages. Jackpot and tail chunks fire
//!     IMMEDIATELY after phase 0 — in parallel with phase 1. Each scout
//!     partition's chunks STREAM into the same full-fetch pool as soon
//!     as the partition returns (biased `tokio::select!`), so phase 1
//!     and phase 2 overlap end-to-end.
//!
//!   Dedup by transaction signature — the only key that correctly
//!   distinguishes same-slot same-balance transactions.

use anyhow::Result;
use futures::stream::{FuturesUnordered, StreamExt};
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::Semaphore;

use crate::rpc::{Filters, HeliusClient, SigResult, SlotFilter};
use crate::types::BalancePoint;

const MAX_CONCURRENT: usize = 100;
const SIG_LIMIT: u64 = 1000;
const FULL_LIMIT: u64 = 100;
const CHUNK_TARGET: usize = 50;

pub async fn compute(client: &HeliusClient, address: &str) -> Result<Vec<BalancePoint>> {
    let sem = Arc::new(Semaphore::new(MAX_CONCURRENT));
    let t0 = Instant::now();

    // ─── Phase 0: jackpot + tail in one RTT ───────────────────────────
    let (jackpot, tail_resp) = tokio::try_join!(
        client.fetch_signatures(address, "asc", SIG_LIMIT, None, None),
        client.fetch_signatures(address, "desc", SIG_LIMIT, None, None),
    )?;

    let slot_max = match tail_resp.data.first() {
        Some(s) => s.slot + 1,
        None => return Ok(vec![]),
    };
    if jackpot.data.is_empty() {
        return Ok(vec![]);
    }
    let jackpot_complete = jackpot.data.len() < SIG_LIMIT as usize;

    let mut jackpot_sorted = jackpot.data;
    jackpot_sorted.sort_by_key(|s| s.slot);
    let first_jackpot_slot = jackpot_sorted.first().unwrap().slot;
    let last_jackpot_slot = jackpot_sorted.last().unwrap().slot;

    // ─── Seed full-fetch pool with jackpot chunks ─────────────────────
    let mut full_fetches = FuturesUnordered::new();
    let jackpot_chunks = chunk_by_slot(&jackpot_sorted, CHUNK_TARGET);
    for (gte, lt) in jackpot_chunks {
        full_fetches.push(fetch_chunk(client, address, Arc::clone(&sem), gte, lt));
    }

    // ─── Build scout partitions + tail chunks (busy wallets only) ─────
    let mut scout_futures = FuturesUnordered::new();
    if !jackpot_complete {
        // Drop tail sigs whose slot is already covered by jackpot's last
        // chunk — jackpot's fetch_page pulls every tx in last_jackpot_slot,
        // so we'd double-fetch if we kept those in the tail chunks.
        let mut tail_sigs: Vec<SigResult> = tail_resp.data;
        tail_sigs.reverse();
        tail_sigs.retain(|s| s.slot > last_jackpot_slot);

        if !tail_sigs.is_empty() {
            let first_tail_slot = tail_sigs.first().unwrap().slot;
            let tail_len = tail_sigs.len();
            let tail_chunks = chunk_by_slot(&tail_sigs, CHUNK_TARGET);
            for (gte, lt) in tail_chunks {
                full_fetches.push(fetch_chunk(client, address, Arc::clone(&sem), gte, lt));
            }

            // Middle gap between jackpot and tail.
            let middle_start = last_jackpot_slot + 1;
            let middle_end = first_tail_slot;
            if middle_end > middle_start {
                let middle_span = middle_end - middle_start;
                let jackpot_span = (last_jackpot_slot - first_jackpot_slot + 1).max(1);
                let tail_span = slot_max.saturating_sub(first_tail_slot).max(1);
                let known_sigs = (jackpot_sorted.len() + tail_len) as f64;
                let known_span = (jackpot_span + tail_span) as f64;
                let density = known_sigs / known_span;
                let est = (density * middle_span as f64) as u64;
                let num_partitions = ((est / 800) + 1).clamp(4, 12);
                for (gte, lt) in make_partitions(middle_start, middle_end, num_partitions) {
                    scout_futures.push(scout_partition(
                        client,
                        address,
                        Arc::clone(&sem),
                        gte,
                        lt,
                    ));
                }
            }
        }
    }

    // ─── Streaming drain: scout completions feed into full_fetches ────
    let mut all = Vec::new();
    loop {
        tokio::select! {
            biased;
            Some(scout_res) = scout_futures.next(), if !scout_futures.is_empty() => {
                let mut sigs = scout_res?;
                sigs.sort_by_key(|s| s.slot);
                for (gte, lt) in chunk_by_slot(&sigs, CHUNK_TARGET) {
                    full_fetches.push(fetch_chunk(client, address, Arc::clone(&sem), gte, lt));
                }
            }
            Some(fetch_res) = full_fetches.next(), if !full_fetches.is_empty() => {
                all.extend(fetch_res?);
            }
            else => break,
        }
    }
    let _ = t0;  // elapsed is reported by caller

    // ─── Dedup by signature (the only truly unique per-tx key) ────────
    all.sort_by(|a, b| {
        a.slot
            .cmp(&b.slot)
            .then_with(|| a.signature.cmp(&b.signature))
    });
    all.dedup_by(|a, b| a.signature == b.signature);
    Ok(all)
}

async fn scout_partition(
    client: &HeliusClient,
    address: &str,
    sem: Arc<Semaphore>,
    gte: u64,
    lt: u64,
) -> Result<Vec<SigResult>> {
    let mut sigs = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let _permit = sem.acquire().await?;
        let page = client
            .fetch_signatures(
                address,
                "asc",
                SIG_LIMIT,
                token,
                Some(Filters { slot: Some(SlotFilter { gte: Some(gte), lt: Some(lt) }) }),
            )
            .await?;
        let done = page.data.len() < SIG_LIMIT as usize;
        sigs.extend(page.data);
        if done { break; }
        token = page.pagination_token;
        if token.is_none() { break; }
    }
    Ok(sigs)
}

async fn fetch_chunk(
    client: &HeliusClient,
    address: &str,
    sem: Arc<Semaphore>,
    gte: u64,
    lt: u64,
) -> Result<Vec<BalancePoint>> {
    let mut balances = Vec::new();
    let mut token: Option<String> = None;
    loop {
        let _permit = sem.acquire().await?;
        let page = client
            .fetch_page(
                address,
                "asc",
                FULL_LIMIT,
                token,
                Some(Filters { slot: Some(SlotFilter { gte: Some(gte), lt: Some(lt) }) }),
            )
            .await?;
        let done = page.data.len() < FULL_LIMIT as usize;
        balances.extend(page.data.iter().filter_map(|tx| tx.extract_balance(address)));
        if done { break; }
        token = page.pagination_token;
        if token.is_none() { break; }
    }
    Ok(balances)
}

fn make_partitions(start: u64, end: u64, count: u64) -> Vec<(u64, u64)> {
    let total = end.saturating_sub(start);
    let width = total / count.max(1);
    if width == 0 {
        return vec![(start, end)];
    }
    (0..count)
        .map(|i| {
            let gte = start + i * width;
            let lt = if i == count - 1 { end } else { start + (i + 1) * width };
            (gte, lt)
        })
        .collect()
}

/// Group sigs into contiguous slot ranges of roughly `target` sigs each.
/// Never splits a single slot across chunks (a slot with N txs must be
/// fetched atomically — pagination tokens inside a slot are opaque).
fn chunk_by_slot(sigs: &[SigResult], target: usize) -> Vec<(u64, u64)> {
    if sigs.is_empty() {
        return vec![];
    }
    let mut chunks = Vec::new();
    let mut chunk_start = sigs[0].slot;
    let mut count = 0usize;
    for (i, sig) in sigs.iter().enumerate() {
        count += 1;
        if count >= target {
            let next_slot = sigs.get(i + 1).map(|s| s.slot);
            match next_slot {
                Some(ns) if ns == sig.slot => continue,
                Some(ns) => {
                    chunks.push((chunk_start, sig.slot + 1));
                    chunk_start = ns;
                    count = 0;
                }
                None => {
                    chunks.push((chunk_start, sig.slot + 1));
                }
            }
        } else if i == sigs.len() - 1 {
            chunks.push((chunk_start, sig.slot + 1));
        }
    }
    chunks
}
