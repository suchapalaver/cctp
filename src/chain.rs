use alloy_chains::NamedChain;
use clap::ValueEnum;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum ChainArg {
    #[value(name = "ethereum")]
    #[serde(rename = "ethereum")]
    Ethereum,
    #[value(name = "hyperevm", alias = "hyper-evm", alias = "hyperliquid")]
    #[serde(rename = "hyperevm", alias = "hyper-evm", alias = "hyperliquid")]
    HyperEvm,
    #[value(name = "ethereum-sepolia", alias = "sepolia")]
    #[serde(rename = "ethereum-sepolia", alias = "sepolia")]
    EthereumSepolia,
    #[value(name = "base-sepolia")]
    #[serde(rename = "base-sepolia")]
    BaseSepolia,
}

impl std::fmt::Display for ChainArg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ethereum => f.write_str("ethereum"),
            Self::HyperEvm => f.write_str("hyperevm"),
            Self::EthereumSepolia => f.write_str("ethereum-sepolia"),
            Self::BaseSepolia => f.write_str("base-sepolia"),
        }
    }
}

impl ChainArg {
    pub(crate) const fn named_chain(self) -> NamedChain {
        match self {
            Self::Ethereum => NamedChain::Mainnet,
            Self::HyperEvm => NamedChain::Hyperliquid,
            Self::EthereumSepolia => NamedChain::Sepolia,
            Self::BaseSepolia => NamedChain::BaseSepolia,
        }
    }

    pub(crate) const fn display_label(self) -> &'static str {
        match self {
            Self::Ethereum => "Ethereum mainnet",
            Self::HyperEvm => "HyperEVM",
            Self::EthereumSepolia => "Ethereum Sepolia testnet",
            Self::BaseSepolia => "Base Sepolia testnet",
        }
    }

    pub(crate) fn chain_id(self) -> u64 {
        u64::from(self.named_chain())
    }
}
