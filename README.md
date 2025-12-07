# Pendle Balance Fetcher

A library for fetching Pendle generic balances for a specific pool configuration.

## Usage

```rust
let fetcher = PendleBalanceFetcherBuilder::new(pool_config)
    .rpc_url("https://api.pendle.finance/rpc")
    .build()
    .unwrap();
```
