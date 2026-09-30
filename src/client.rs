use std::{collections::HashMap, str::FromStr, time::Duration};

use alloy::primitives::Address;
use anyhow::Result;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use crate::{
    constants::HYPEREVM_CHAIN_ID,
    types::protocols::{
        EulerUserInstance, LiquidLockerData, MmType, MorphoUserInstance, SiloUserInstance,
    },
};

#[derive(Debug, Clone, Hash, Serialize, Deserialize)]
pub struct MmMapType {
    pub holder: Address,
    #[serde(rename = "type")]
    pub mm_type: MmType,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WlpInfo {
    pub wlp: String,
    pub wlp_holders: Vec<Address>,
    pub euler: Vec<EulerUserInstance>,
    pub silo: Vec<SiloUserInstance>,
    pub morpho_address: Option<Address>,
    pub morpho: Vec<MorphoUserInstance>,
    #[serde(rename = "remapMMHolder")]
    pub remap_mm_holder: HashMap<Address, MmMapType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FullMarketInfo {
    pub lp_holders: Vec<Address>,
    pub liquid_locker_datas: Vec<LiquidLockerData>,
    pub wlp_info: Option<WlpInfo>,
}

#[derive(Debug, Clone)]
pub struct PendleClient {
    client: Client,
}

impl PendleClient {
    const PENDLE_API_URL: &str = "https://api-v2.pendle.finance/core/v1/statistics";

    /// Create a new Pendle API client with a bounded HTTP timeout.
    /// Falls back to a default client when the timed builder fails, so
    /// library construction never panics on TLS/runtime initialization errors.
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client }
    }

    /// Fallible constructor for callers that want an explicit error instead
    /// of the silent fallback used by `new()`.
    pub fn try_new() -> anyhow::Result<Self> {
        let client = Client::builder()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build HTTP client: {e}"))?;
        Ok(Self { client })
    }

    pub async fn query_token(&self, token: &Address) -> Result<Vec<Address>> {
        let url = format!(
            "{}/get-distinct-user-from-token?token={}",
            Self::PENDLE_API_URL,
            token.to_string().to_lowercase()
        );

        #[derive(Deserialize)]
        struct Response {
            users: Vec<String>,
        }

        // Validate HTTP status before parsing so transient API errors
        // surface as explicit errors instead of JSON decode failures.
        let http_res = self.client.get(&url).send().await?.error_for_status()?;
        let res: Response = http_res.json().await?;

        // Skip malformed addresses from the untrusted API instead of panicking
        // and aborting the whole batch fetch.
        let users = res
            .users
            .iter()
            .filter_map(|user| Address::from_str(user).ok())
            .collect();

        Ok(users)
    }

    pub async fn query_market_info(&self, market: &Address) -> Result<FullMarketInfo> {
        let url = format!(
            "{}/get-all-related-info-from-lp-and-wlp?chainId={}&marketAddress={}",
            Self::PENDLE_API_URL,
            HYPEREVM_CHAIN_ID,
            market.to_string().to_lowercase()
        );

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Response {
            distinct_users: Vec<Address>,
            liquid_locker_pools: Vec<LiquidLockerData>,
            wlp_distinct_users_response: Option<WlpDistinctUsersResponse>,
            #[serde(default)]
            wlp_holder_mappings: Vec<WlpHolderMapping>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WlpDistinctUsersResponse {
            wlp_address: String,
            wlp_users: Vec<String>,
            morpho_users: Vec<MorphoUserInstance>,
            euler_users: Vec<EulerUserInstance>,
            silo_users: Vec<SiloUserInstance>,
            morpho_configs: Vec<MorphoConfig>,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct MorphoConfig {
            morpho_address: String,
        }

        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct WlpHolderMapping {
            asset: String,
            holder: String,
            money_market: MmType,
        }

        let http_res = self.client.get(&url).send().await?.error_for_status()?;
        let res: Response = http_res.json().await?;

        // If no `wlpDistinctUsersResponse` in response, return simple response
        if res.wlp_distinct_users_response.is_none() {
            return Ok(FullMarketInfo {
                lp_holders: res.distinct_users,
                liquid_locker_datas: res.liquid_locker_pools,
                wlp_info: None,
            });
        }

        // Build `remapMMHolder` map
        let mut remap_mm_holder: HashMap<Address, MmMapType> = HashMap::new();
        for whm in res.wlp_holder_mappings {
            let asset = match Address::from_str(&whm.asset) {
                Ok(asset) => asset,
                Err(_) => continue,
            };

            let holder = match Address::from_str(&whm.holder) {
                Ok(holder) => holder,
                Err(_) => continue,
            };

            remap_mm_holder.insert(
                asset,
                MmMapType {
                    holder,
                    mm_type: whm.money_market,
                },
            );
        }

        // Extract Morpho address if available. The stored value is parsed
        // leniently: an invalid address yields None instead of panicking.
        let wlp_response = res
            .wlp_distinct_users_response
            .ok_or_else(|| anyhow::anyhow!("Missing WLP distinct users response"))?;
        let morpho_address = wlp_response
            .morpho_configs
            .first()
            .map(|config| config.morpho_address.to_lowercase());

        // Parse the optional Morpho address without panicking on bad API data.
        let morpho_address_parsed = morpho_address
            .as_deref()
            .and_then(|address| Address::from_str(address).ok());

        Ok(FullMarketInfo {
            lp_holders: res.distinct_users,
            liquid_locker_datas: res.liquid_locker_pools,
            wlp_info: Some(WlpInfo {
                wlp: wlp_response.wlp_address,
                // Skip malformed WLP holder entries from the untrusted API.
                wlp_holders: wlp_response
                    .wlp_users
                    .iter()
                    .filter_map(|user| Address::from_str(user).ok())
                    .collect(),
                morpho: wlp_response.morpho_users,
                euler: wlp_response.euler_users,
                morpho_address: morpho_address_parsed,
                silo: wlp_response.silo_users,
                remap_mm_holder,
            }),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;

    // Live-network integration test: requires external Pendle API access.
    #[tokio::test]
    #[ignore]
    async fn test_query_token() {
        let client = PendleClient::new();
        let result = client
            .query_token(&Address::from_str("0x57fc55dff8ceca86ee94a6bf255af2f0ed90eb9e").unwrap())
            .await
            .unwrap();
        println!("{:?}", result);
    }
}
