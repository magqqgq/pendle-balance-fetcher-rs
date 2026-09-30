# Pendle Balance Fetcher

A library for fetching Pendle generic balances for a specific pool configuration.

## Usage

```rust
use pendle::PendleBalanceFetcher;
use pendle::types::{PoolConfig, PoolType};

let fetcher = PendleBalanceFetcher::builder()
    .rpc_url(std::env::var("RPC_URL")?)?
    .build()?;
let snapshots = fetcher
    .fetch_user_balance_snapshot_batch(&pool_config, &[block_number], PoolType::Shares)
    .await?;
```

Notes:

- `rpc_url` expects an HTTP(S) Ethereum JSON-RPC endpoint (not the Pendle
  REST API) and returns a `Result`; invalid schemes are rejected.
- Live-network integration tests are marked `#[ignore]` and require `RPC_URL`
  to be set. Run them explicitly with
  `cargo test -- --ignored`.
