use alloy::providers::ProviderBuilder;
use anyhow::{Context, Error};
use url::Url;

use crate::{
    PendleBalanceFetcher, client::PendleClient, multicall::Multicall, types::provider::RpcProvider,
};

pub struct MissingProvider;

pub struct ProviderSet {
    provider: RpcProvider,
}

pub struct PendleBalanceFetcherBuilder<State> {
    state: State,
}

impl PendleBalanceFetcherBuilder<MissingProvider> {
    pub fn new() -> Self {
        Self {
            state: MissingProvider,
        }
    }

    pub fn rpc_url(
        self,
        rpc_url: impl Into<String>,
    ) -> Result<PendleBalanceFetcherBuilder<ProviderSet>, Error> {
        let url = rpc_url.into().parse::<Url>().context("Invalid RPC URL")?;
        // Reject non-HTTP(S) schemes (for example file or ftp URLs) so a
        // misconfigured endpoint cannot trigger SSRF-like local file access.
        if url.scheme() != "http" && url.scheme() != "https" {
            return Err(anyhow::anyhow!("RPC URL must use http or https").into());
        }
        let provider = ProviderBuilder::new().connect_http(url);

        Ok(PendleBalanceFetcherBuilder {
            state: ProviderSet { provider },
        })
    }

    pub fn rpc_provider(
        self,
        rpc_provider: RpcProvider,
    ) -> PendleBalanceFetcherBuilder<ProviderSet> {
        PendleBalanceFetcherBuilder {
            state: ProviderSet {
                provider: rpc_provider,
            },
        }
    }
}

impl PendleBalanceFetcherBuilder<ProviderSet> {
    pub fn build(self) -> Result<PendleBalanceFetcher, Error> {
        let multicall = Multicall::new(self.state.provider.clone());
        let client = PendleClient::new();

        Ok(PendleBalanceFetcher {
            rpc_provider: self.state.provider,
            multicall,
            client,
        })
    }
}
