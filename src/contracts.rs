use alloy::sol;

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    Multicall,
    "abis/Multicall.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    PendleYieldToken,
    "abis/PendleYieldToken.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    PendleMarket,
    "abis/PendleMarket.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    PendleYieldContractFactory,
    "abis/PendleYieldContractFactory.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    PendleOracle,
    "abis/PendleOracle.json"
);

sol!(
    #[allow(missing_docs)]
    #[sol(rpc)]
    MorphoBlue,
    "abis/MorphoBlue.json"
);

sol! {
    #[allow(missing_docs)]
    #[sol(rpc)]
    contract SyToken {
        function exchangeRate() external view returns (uint256);
    }
}
