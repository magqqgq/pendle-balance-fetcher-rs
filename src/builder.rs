use alloy::providers::ProviderBuilder;
use anyhow::{Context, Error};
use url::Url;

use crate::{
    PendleBalanceFetcher,
    client::PendleClient,
    multicall::Multicall,
    types::{PoolConfig, provider::RpcProvider},
};

pub struct MissingProvider;

pub struct ProviderSet {
    provider: RpcProvider,
}

pub struct PendleBalanceFetcherBuilder<State> {
    state: State,
    pool_config: PoolConfig,
}

impl PendleBalanceFetcherBuilder<MissingProvider> {
    pub fn new(pool_config: PoolConfig) -> Self {
        Self {
            state: MissingProvider,
            pool_config,
        }
    }

    pub fn rpc_url(
        self,
        rpc_url: impl Into<String>,
    ) -> Result<PendleBalanceFetcherBuilder<ProviderSet>, Error> {
        let url = rpc_url.into().parse::<Url>().context("Invalid RPC URL")?;
        let provider = ProviderBuilder::new().connect_http(url);

        Ok(PendleBalanceFetcherBuilder {
            state: ProviderSet { provider },
            pool_config: self.pool_config,
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
            pool_config: self.pool_config,
        }
    }
}

impl<S> PendleBalanceFetcherBuilder<S> {
    pub fn pool_config(mut self, pool_config: PoolConfig) -> Self {
        self.pool_config = pool_config;
        self
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
            pool_config: self.pool_config,
        })
    }
}
