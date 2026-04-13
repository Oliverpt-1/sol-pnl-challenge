# sol-pnl-challenge

Lowest-latency algorithm for computing a Solana wallet's SOL balance
timeline using only Helius RPC, for Mert's Solana dev weekend
competition. Implementation in Rust.

## Quick start

```bash
cargo run --release -- \
  --address <WALLET_ADDRESS> \
  --rpc-url "https://mainnet.helius-rpc.com/?api-key=$HELIUS_API_KEY"
```

Output is JSON on stdout:

```json
{
  "address": "vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg",
  "latencyMs": 514.16,
  "transactionCount": 3990,
  "pnlLamports": -12345678,
  "pnlSol": -0.012345678
}
```

Pass `--trajectory` to include the full per-tx `[slot, post_sol]`
series in the output.

## Benchmarks

Cold-start (timer starts at process launch, includes TCP+TLS+h2
handshake). 5 runs each, local machine → Helius US, ~50ms RTT. All
20 runs returned exact transaction counts.

| wallet type | address | txs | best | avg (5 runs) |
|---|---|---|---|---|
| sparse           | `CKs1E69a2e9TmH4mKKLrXFF8kD3ZnwKjoEuXa6sz9WqX`   |  248 | **256ms** | 303ms |
| busy             | `vines1vzrYbzLMRdu58ou5XTby4qAqVRLmqo36NKPTg`    | 3990 | **479ms** | 601ms |
| periodic         | `6p2Knnjzh54tesHHf5GPDhHZCzrfvLWf89RfnEQBEivE`   | 3359 | **400ms** | 448ms |
| periodic (bursts)| `HMBKn2hPdLLEadXxKxM2bfeqHeXxqtVMpB8e9yJDTc5z`   | 1131 | **319ms** | 563ms |

## Algorithm

**Scout** — sigs-first adaptive scan, streamed into a parallel full-fetch pool.

`getTransactionsForAddress` with `details=signatures` returns **1000**
rows per call for the same credit cost as a 100-row full call. Sigs let
us learn wallet density cheaply before committing to the heavy full-tx
downloads. Everything else is about overlapping work so nothing waits.

### Phase 0 — 1 RTT, 2 parallel calls

- `sigs(asc,  1000)` — oldest 1000 sigs ("jackpot")
- `sigs(desc, 1000)` — newest 1000 sigs ("tail", and `slot_max`)

Together they pin both ends of the wallet's active range and give us
two density anchors. For wallets with fewer than 2000 txs, phase 0
already has every sig we need.

Phase 0 runs **concurrently with `warm()`** via `tokio::try_join!`.
The first request to touch the connection pool opens TCP+TLS+h2; the
others multiplex on top. No cold-start wait.

### Phase 1 — scout the middle gap (busy wallets only)

For wallets with >2000 txs, partition the slot gap between jackpot's
last slot and tail's first slot into N adaptive slices (clamped 4..=12,
scaled from the density anchors) and fire `sigs(asc, 1000)` per slice
in parallel. Each scout partition paginates if dense.

Tail sigs whose slot is `≤ last_jackpot_slot` are filtered out — a
full-mode fetch on jackpot's last chunk already pulls every tx in that
slot, so the ranges stay disjoint.

### Phase 2 — streamed full-mode fetch

Chunk the sigs by slot into ~50-sig ranges, fire each as a
slot-filtered `full(asc, 100)` page. Jackpot and tail chunks fire
**immediately after phase 0**, in parallel with phase 1. Each scout
partition's chunks **stream** into the same full-fetch pool as soon
as the partition returns, via a biased `tokio::select!`:

```rust
loop {
    tokio::select! {
        biased;
        Some(scout_res) = scout_futures.next(), if !scout_futures.is_empty() => {
            // push new chunks from this partition into full_fetches
        }
        Some(fetch_res) = full_fetches.next(), if !full_fetches.is_empty() => {
            // drain one completed chunk into the result
        }
        else => break,
    }
}
```

So phase 1 (scouting) and phase 2 (full fetching) overlap end-to-end.
The critical path collapses from `phase0 + phase1 + phase2` to
`phase0 + max(phase1, phase2_jackpot_tail) + phase2_scout_remainder`.

### Dedup by signature

After all chunks drain, dedup by `BalancePoint.signature`. **Not** by
`(slot, block_time, post_lamports)` — that key silently collapses
legitimate distinct transactions on wallets that receive multiple
identical-amount transfers in the same slot (automated rewards,
drip bots). A signature is the only per-tx unique identifier.

### Correctness notes

- **v0 Address Lookup Tables:** `TxResult::extract_balance` first
  searches `transaction.message.accountKeys`; if the wallet isn't
  there, falls back to `meta.loadedAddresses.{writable, readonly}`
  with the correct balance-array offset. On our test wallets
  `encoding: "jsonParsed"` already merges ALT addresses into
  accountKeys, but the fallback protects against encoding changes.
- **Dense slots are atomic:** `chunk_by_slot` never splits a single
  slot across chunks (pagination tokens within a slot are opaque and
  server-validated, so there's no safe way to stitch).

## Runtime config

- HTTP/2 over one TLS connection, large h2 windows (2MB stream /
  16MB connection) to avoid flow-control stalls on parallel fat
  responses.
- `gzip` + `brotli` decompression (responses are 5-10x compressible).
- `TCP_NODELAY` on, idle pool 100 per host.
- Global concurrency capped at 100 (semaphore) — well under Helius
  Developer tier limits, leaves headroom for retries.
- 5-try exponential backoff on HTTP 429 / 5xx / RPC `-32429`.
- Retries reuse the serialized request body — no re-walking.
- Release profile: `lto="fat"`, `codegen-units=1`, `panic=abort`,
  `strip=true`.

## Files

```
src/
  main.rs     CLI, cold-start timing, JSON output
  rpc.rs      Helius HTTP/2 client + TxResult decode (incl ALT fallback)
  scout.rs    The algorithm
  types.rs    BalancePoint
```

No database, no cache, no indexing, no persistence between calls.
Every run is cold.
