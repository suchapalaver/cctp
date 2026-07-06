use alloy::primitives::{Address, address};
use cctp_rs::CctpV2Route;
use eyre::{Result, eyre};

use crate::chain::ChainArg;

pub(crate) const MAINNET_USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
pub(crate) const ETHEREUM_SEPOLIA_USDC: Address =
    address!("1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteConfig {
    route: CctpV2Route,
    from: ChainArg,
    to: ChainArg,
    default_usdc: Address,
}

impl RouteConfig {
    fn from_supported(route: SupportedRoute) -> Result<Self> {
        Ok(Self {
            route: CctpV2Route::new(route.source_chain(), route.destination_chain())?,
            from: route.from,
            to: route.to,
            default_usdc: route.default_usdc,
        })
    }

    pub(crate) fn source_chain_id(&self) -> u64 {
        self.from.chain_id()
    }

    pub(crate) fn destination_chain_id(&self) -> u64 {
        self.to.chain_id()
    }

    pub(crate) const fn cctp_route(&self) -> CctpV2Route {
        self.route
    }

    pub(crate) const fn from(&self) -> ChainArg {
        self.from
    }

    pub(crate) const fn to(&self) -> ChainArg {
        self.to
    }

    pub(crate) const fn source_label(&self) -> &'static str {
        self.from.display_label()
    }

    pub(crate) const fn destination_label(&self) -> &'static str {
        self.to.display_label()
    }

    pub(crate) const fn default_usdc(&self) -> Address {
        self.default_usdc
    }
}

impl std::fmt::Display for RouteConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} -> {}", self.source_label(), self.destination_label())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SupportedRoute {
    from: ChainArg,
    to: ChainArg,
    default_usdc: Address,
}

impl SupportedRoute {
    fn matches(self, from: ChainArg, to: ChainArg) -> bool {
        self.from == from && self.to == to
    }

    const fn source_chain(self) -> alloy_chains::NamedChain {
        self.from.named_chain()
    }

    const fn destination_chain(self) -> alloy_chains::NamedChain {
        self.to.named_chain()
    }

    fn cli_pair(self) -> String {
        format!("{} -> {}", self.from, self.to)
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct RouteCatalog;

pub(crate) const ROUTE_CATALOG: RouteCatalog = RouteCatalog;

const SUPPORTED_ROUTES: &[SupportedRoute] = &[
    SupportedRoute {
        from: ChainArg::Ethereum,
        to: ChainArg::HyperEvm,
        default_usdc: MAINNET_USDC,
    },
    SupportedRoute {
        from: ChainArg::EthereumSepolia,
        to: ChainArg::BaseSepolia,
        default_usdc: ETHEREUM_SEPOLIA_USDC,
    },
];

impl RouteCatalog {
    pub(crate) fn resolve(&self, from: ChainArg, to: ChainArg) -> Result<RouteConfig> {
        let supported = self
            .supported_routes()
            .iter()
            .copied()
            .find(|route| route.matches(from, to))
            .ok_or_else(|| {
                eyre!(
                    "unsupported route {from} -> {to}; supported routes: {}",
                    self.supported_routes_description()
                )
            })?;

        RouteConfig::from_supported(supported)
    }

    const fn supported_routes(&self) -> &'static [SupportedRoute] {
        SUPPORTED_ROUTES
    }

    fn supported_routes_description(&self) -> String {
        self.supported_routes()
            .iter()
            .map(|route| route.cli_pair())
            .collect::<Vec<_>>()
            .join(", ")
    }
}
