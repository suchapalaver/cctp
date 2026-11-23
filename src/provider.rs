use alloy::{
    primitives::Address,
    providers::{DynProvider, Provider, ProviderBuilder},
    signers::trezor::TrezorSigner,
};
use async_trait::async_trait;
use cctp_rs::{CctpV2Bridge, DomainId, TransferMode};
use eyre::{Result, WrapErr, bail, eyre};
use url::Url;

use crate::chain::ChainArg;
use crate::config::{
    BASE_SEPOLIA_RPC_ENV, BridgeConfig, BridgeConfigFile, ConfigValueSource, ETHEREUM_RPC_ENV,
    ETHEREUM_SEPOLIA_RPC_ENV, EnvSource, HYPEREVM_RPC_ENV, sourced_required_cli_env_file,
};
use crate::routes::RouteConfig;
use crate::wallet::{RelaySignerRuntime, TrezorWalletService, WalletService};
use crate::{BridgeArgs, BridgeRuntime, CctpBridgeRuntime};

#[derive(Clone, Debug)]
pub(crate) struct RpcEndpoints {
    pub(crate) source: Url,
    pub(crate) destination: Url,
}

impl RpcEndpoints {
    pub(crate) fn from_resolved(endpoints: &ResolvedChainRpcEndpoints) -> Self {
        Self {
            source: endpoints.source.url.clone(),
            destination: endpoints.destination.url.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct ChainEndpointCatalog;

pub(crate) const CHAIN_ENDPOINT_CATALOG: ChainEndpointCatalog = ChainEndpointCatalog;

const CHAIN_ENDPOINTS: &[ChainEndpoint] = &[
    ChainEndpoint {
        chain: ChainArg::Ethereum,
        cli_flag: "--ethereum-rpc",
        env_var: ETHEREUM_RPC_ENV,
        config_field: "ethereum_rpc",
        missing_message: "missing Ethereum RPC URL; set --ethereum-rpc, ETHEREUM_RPC_URL, or ethereum_rpc in the config file",
        parse_error: "failed to parse --ethereum-rpc as a URL",
    },
    ChainEndpoint {
        chain: ChainArg::HyperEvm,
        cli_flag: "--hyperevm-rpc",
        env_var: HYPEREVM_RPC_ENV,
        config_field: "hyperevm_rpc",
        missing_message: "missing HyperEVM RPC URL; set --hyperevm-rpc, HYPEREVM_RPC_URL, or hyperevm_rpc in the config file",
        parse_error: "failed to parse --hyperevm-rpc as a URL",
    },
    ChainEndpoint {
        chain: ChainArg::EthereumSepolia,
        cli_flag: "--ethereum-sepolia-rpc",
        env_var: ETHEREUM_SEPOLIA_RPC_ENV,
        config_field: "ethereum_sepolia_rpc",
        missing_message: "missing Ethereum Sepolia RPC URL; set --ethereum-sepolia-rpc, ETHEREUM_SEPOLIA_RPC_URL, or ethereum_sepolia_rpc in the config file",
        parse_error: "failed to parse --ethereum-sepolia-rpc as a URL",
    },
    ChainEndpoint {
        chain: ChainArg::BaseSepolia,
        cli_flag: "--base-sepolia-rpc",
        env_var: BASE_SEPOLIA_RPC_ENV,
        config_field: "base_sepolia_rpc",
        missing_message: "missing Base Sepolia RPC URL; set --base-sepolia-rpc, BASE_SEPOLIA_RPC_URL, or base_sepolia_rpc in the config file",
        parse_error: "failed to parse --base-sepolia-rpc as a URL",
    },
];

impl ChainEndpointCatalog {
    pub(crate) fn resolve_route<E>(
        &self,
        route: &RouteConfig,
        args: &BridgeArgs,
        file: &BridgeConfigFile,
        env: &E,
    ) -> Result<ResolvedChainRpcEndpoints>
    where
        E: EnvSource,
    {
        Ok(ResolvedChainRpcEndpoints {
            source: self.resolve_endpoint(route.from(), args, file, env)?,
            destination: self.resolve_endpoint(route.to(), args, file, env)?,
        })
    }

    fn resolve_endpoint<E>(
        &self,
        chain: ChainArg,
        args: &BridgeArgs,
        file: &BridgeConfigFile,
        env: &E,
    ) -> Result<ResolvedChainRpcEndpoint>
    where
        E: EnvSource,
    {
        let endpoint = self.endpoint(chain)?;
        let raw_endpoint = sourced_required_cli_env_file(
            endpoint.cli_value(args),
            endpoint.cli_flag,
            env.get(endpoint.env_var),
            endpoint.env_var,
            endpoint.file_value(file),
            endpoint.config_field,
            endpoint.missing_message,
        )?;
        let url = raw_endpoint
            .value()
            .parse()
            .wrap_err(endpoint.parse_error)?;

        Ok(ResolvedChainRpcEndpoint {
            url,
            config_source: raw_endpoint.source(),
        })
    }

    fn endpoint(&self, chain: ChainArg) -> Result<ChainEndpoint> {
        self.endpoints()
            .iter()
            .copied()
            .find(|endpoint| endpoint.chain == chain)
            .ok_or_else(|| eyre!("missing RPC endpoint catalog entry for {chain}"))
    }

    const fn endpoints(&self) -> &'static [ChainEndpoint] {
        CHAIN_ENDPOINTS
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ChainEndpoint {
    chain: ChainArg,
    cli_flag: &'static str,
    env_var: &'static str,
    config_field: &'static str,
    missing_message: &'static str,
    parse_error: &'static str,
}

impl ChainEndpoint {
    fn cli_value(self, args: &BridgeArgs) -> Option<String> {
        match self.chain {
            ChainArg::Ethereum => args.ethereum_rpc.clone(),
            ChainArg::HyperEvm => args.hyperevm_rpc.clone(),
            ChainArg::EthereumSepolia => args.ethereum_sepolia_rpc.clone(),
            ChainArg::BaseSepolia => args.base_sepolia_rpc.clone(),
        }
    }

    fn file_value(self, file: &BridgeConfigFile) -> Option<String> {
        match self.chain {
            ChainArg::Ethereum => file.ethereum_rpc.clone(),
            ChainArg::HyperEvm => file.hyperevm_rpc.clone(),
            ChainArg::EthereumSepolia => file.ethereum_sepolia_rpc.clone(),
            ChainArg::BaseSepolia => file.base_sepolia_rpc.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedChainRpcEndpoints {
    pub(crate) source: ResolvedChainRpcEndpoint,
    pub(crate) destination: ResolvedChainRpcEndpoint,
}

#[derive(Clone, Debug)]
pub(crate) struct ResolvedChainRpcEndpoint {
    pub(crate) url: Url,
    pub(crate) config_source: ConfigValueSource,
}

#[derive(Clone, Debug)]
pub(crate) struct BridgeProviders {
    source: DynProvider,
    destination: DynProvider,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AlloyProviderService;

pub(crate) trait ProviderService<W>
where
    W: WalletService,
{
    type Providers;
    type Bridge;
    type Runtime: BridgeRuntime;

    fn read_only_providers(&self, config: &BridgeConfig) -> Self::Providers;

    fn bridge_providers(
        &self,
        config: &BridgeConfig,
        source_signer: W::SourceSigner,
        relay_signer: RelaySignerRuntime<W::RelaySigner>,
    ) -> Self::Providers;

    fn bridge(
        &self,
        config: &BridgeConfig,
        providers: &Self::Providers,
        recipient: Address,
        transfer_mode: TransferMode,
    ) -> Self::Bridge;

    fn contracts(&self, bridge: &Self::Bridge) -> Result<BridgeContracts>;

    fn runtime(&self, bridge: Self::Bridge, providers: Self::Providers) -> Self::Runtime;
}

impl ProviderService<TrezorWalletService> for AlloyProviderService {
    type Providers = BridgeProviders;
    type Bridge = CctpV2Bridge<DynProvider>;
    type Runtime = CctpBridgeRuntime<DynProvider>;

    fn read_only_providers(&self, config: &BridgeConfig) -> BridgeProviders {
        BridgeProviders {
            source: ProviderBuilder::new()
                .connect_http(config.rpc.source.clone())
                .erased(),
            destination: ProviderBuilder::new()
                .connect_http(config.rpc.destination.clone())
                .erased(),
        }
    }

    fn bridge_providers(
        &self,
        config: &BridgeConfig,
        source_signer: TrezorSigner,
        relay_signer: RelaySignerRuntime<TrezorSigner>,
    ) -> BridgeProviders {
        let source = ProviderBuilder::new()
            .wallet(source_signer)
            .connect_http(config.rpc.source.clone())
            .erased();
        let destination = ProviderBuilder::new()
            .wallet(relay_signer.signer)
            .connect_http(config.rpc.destination.clone())
            .erased();

        BridgeProviders {
            source,
            destination,
        }
    }

    fn bridge(
        &self,
        config: &BridgeConfig,
        providers: &BridgeProviders,
        recipient: Address,
        transfer_mode: TransferMode,
    ) -> CctpV2Bridge<DynProvider> {
        CctpV2Bridge::from_route(config.route.cctp_route())
            .source_provider(providers.source.clone())
            .destination_provider(providers.destination.clone())
            .recipient(recipient)
            .transfer_mode(transfer_mode)
            .build()
    }

    fn contracts(&self, bridge: &CctpV2Bridge<DynProvider>) -> Result<BridgeContracts> {
        BridgeContracts::from_bridge(bridge)
    }

    fn runtime(
        &self,
        bridge: CctpV2Bridge<DynProvider>,
        providers: BridgeProviders,
    ) -> CctpBridgeRuntime<DynProvider> {
        CctpBridgeRuntime::new(bridge, providers.source, providers.destination)
    }
}

#[async_trait(?Send)]
pub(crate) trait ProviderValidationService<P> {
    async fn validate(&self, config: &BridgeConfig, providers: &P) -> Result<ProviderValidation>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct AlloyProviderValidationService;

#[async_trait(?Send)]
impl ProviderValidationService<BridgeProviders> for AlloyProviderValidationService {
    async fn validate(
        &self,
        config: &BridgeConfig,
        providers: &BridgeProviders,
    ) -> Result<ProviderValidation> {
        let source_chain_id = providers
            .source
            .get_chain_id()
            .await
            .wrap_err("failed to read source RPC chain ID")?;
        let destination_chain_id = providers
            .destination
            .get_chain_id()
            .await
            .wrap_err("failed to read destination RPC chain ID")?;

        ProviderValidation::new(config.route, source_chain_id, destination_chain_id)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderValidation {
    pub(crate) source: ProviderChainCheck,
    pub(crate) destination: ProviderChainCheck,
}

impl ProviderValidation {
    pub(crate) fn new(
        route: RouteConfig,
        source_actual_chain_id: u64,
        destination_actual_chain_id: u64,
    ) -> Result<Self> {
        Ok(Self {
            source: ProviderChainCheck::validate(
                route,
                ProviderEndpointRole::Source,
                ExpectedProviderChain::new(route.from()),
                source_actual_chain_id,
            )?,
            destination: ProviderChainCheck::validate(
                route,
                ProviderEndpointRole::Destination,
                ExpectedProviderChain::new(route.to()),
                destination_actual_chain_id,
            )?,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ProviderChainCheck {
    pub(crate) role: ProviderEndpointRole,
    pub(crate) expected: ExpectedProviderChain,
    pub(crate) actual_chain_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExpectedProviderChain {
    pub(crate) chain: ChainArg,
}

impl ExpectedProviderChain {
    pub(crate) const fn new(chain: ChainArg) -> Self {
        Self { chain }
    }

    pub(crate) const fn display_label(&self) -> &'static str {
        self.chain.display_label()
    }

    fn chain_id(&self) -> u64 {
        self.chain.chain_id()
    }
}

impl ProviderChainCheck {
    fn validate(
        route: RouteConfig,
        role: ProviderEndpointRole,
        expected: ExpectedProviderChain,
        actual_chain_id: u64,
    ) -> Result<Self> {
        let expected_chain_id = expected.chain_id();
        if actual_chain_id != expected_chain_id {
            let chain_label = expected.display_label();
            bail!(
                "{} chain ID mismatch for route {route}: expected {expected_chain_id} ({chain_label}), got {actual_chain_id}",
                role.error_label()
            );
        }

        Ok(Self {
            role,
            expected,
            actual_chain_id,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProviderEndpointRole {
    Source,
    Destination,
}

impl ProviderEndpointRole {
    const fn error_label(self) -> &'static str {
        match self {
            Self::Source => "source RPC",
            Self::Destination => "destination RPC",
        }
    }

    pub(crate) const fn report_label(self) -> &'static str {
        match self {
            Self::Source => "Source RPC",
            Self::Destination => "Destination RPC",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BridgeContracts {
    pub(crate) token_messenger: Address,
    pub(crate) message_transmitter: Address,
    pub(crate) destination_domain: DomainId,
}

impl BridgeContracts {
    fn from_bridge<P>(bridge: &CctpV2Bridge<P>) -> Result<Self>
    where
        P: Provider + Clone,
    {
        Ok(Self {
            token_messenger: bridge.token_messenger_v2_contract()?,
            message_transmitter: bridge.message_transmitter_v2_contract()?,
            destination_domain: bridge.destination_domain_id()?,
        })
    }
}

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests use explicit expect messages to assert setup and validation invariants"
)]
mod tests {
    use super::*;
    use crate::config::{ETHEREUM_SEPOLIA_RPC_ENV, RpcEndpointsProvenance};
    use crate::routes::ROUTE_CATALOG;
    use std::collections::HashMap;

    #[derive(Clone, Debug, Default)]
    struct TestEnv(HashMap<String, String>);

    impl EnvSource for TestEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn empty_args() -> BridgeArgs {
        BridgeArgs {
            config: None,
            from: None,
            to: None,
            amount: None,
            recipient: None,
            ethereum_rpc: None,
            hyperevm_rpc: None,
            ethereum_sepolia_rpc: None,
            base_sepolia_rpc: None,
            wallet: None,
            trezor_account: None,
            relay_trezor_account: None,
            usdc: None,
            fast: None,
            max_fee_usdc: None,
            self_relay: None,
            receive_attempts: None,
            receive_interval_secs: None,
            dry_run: None,
            yes: false,
            output: None,
        }
    }

    fn supported_route_config() -> RouteConfig {
        ROUTE_CATALOG
            .resolve(ChainArg::Ethereum, ChainArg::HyperEvm)
            .expect("mainnet route should be supported")
    }

    fn testnet_route_config() -> RouteConfig {
        ROUTE_CATALOG
            .resolve(ChainArg::EthereumSepolia, ChainArg::BaseSepolia)
            .expect("testnet route should be supported")
    }

    #[test]
    fn endpoint_catalog_resolves_route_endpoint_roles_and_provenance() {
        let route = supported_route_config();
        let mut args = empty_args();
        args.ethereum_rpc = Some("https://source.example".to_owned());
        args.hyperevm_rpc = Some("https://destination.example".to_owned());

        let endpoints = CHAIN_ENDPOINT_CATALOG
            .resolve_route(
                &route,
                &args,
                &BridgeConfigFile::default(),
                &TestEnv::default(),
            )
            .expect("endpoint catalog should resolve current route");
        let rpc = RpcEndpoints::from_resolved(&endpoints);
        let provenance = RpcEndpointsProvenance::from_resolved(&endpoints);

        assert_eq!(rpc.source.as_str(), "https://source.example/");
        assert_eq!(rpc.destination.as_str(), "https://destination.example/");
        assert_eq!(
            provenance.source.source,
            ConfigValueSource::CliFlag("--ethereum-rpc")
        );
        assert_eq!(
            provenance.destination.source,
            ConfigValueSource::CliFlag("--hyperevm-rpc")
        );
    }

    #[test]
    fn endpoint_catalog_resolves_testnet_endpoint_roles_and_provenance() {
        let route = testnet_route_config();
        let mut args = empty_args();
        args.ethereum_sepolia_rpc = Some("https://source-testnet.example".to_owned());
        args.base_sepolia_rpc = Some("https://destination-testnet.example".to_owned());

        let endpoints = CHAIN_ENDPOINT_CATALOG
            .resolve_route(
                &route,
                &args,
                &BridgeConfigFile::default(),
                &TestEnv::default(),
            )
            .expect("endpoint catalog should resolve testnet route");
        let rpc = RpcEndpoints::from_resolved(&endpoints);
        let provenance = RpcEndpointsProvenance::from_resolved(&endpoints);

        assert_eq!(rpc.source.as_str(), "https://source-testnet.example/");
        assert_eq!(
            rpc.destination.as_str(),
            "https://destination-testnet.example/"
        );
        assert_eq!(
            provenance.source.source,
            ConfigValueSource::CliFlag("--ethereum-sepolia-rpc")
        );
        assert_eq!(
            provenance.destination.source,
            ConfigValueSource::CliFlag("--base-sepolia-rpc")
        );
    }

    #[test]
    fn endpoint_catalog_maps_testnet_sources_by_chain_after_precedence() {
        let route = testnet_route_config();
        let mut args = empty_args();
        args.base_sepolia_rpc = Some("https://cli.base-sepolia.example".to_owned());
        let mut file = BridgeConfigFile::default();
        file.ethereum_sepolia_rpc = Some("https://file.ethereum-sepolia.example".to_owned());
        file.base_sepolia_rpc = Some("https://file.base-sepolia.example".to_owned());
        let env = TestEnv(HashMap::from([(
            ETHEREUM_SEPOLIA_RPC_ENV.to_owned(),
            "https://env.ethereum-sepolia.example".to_owned(),
        )]));

        let endpoints = CHAIN_ENDPOINT_CATALOG
            .resolve_route(&route, &args, &file, &env)
            .expect("endpoint catalog should apply per-chain testnet precedence");
        let provenance = RpcEndpointsProvenance::from_resolved(&endpoints);

        assert_eq!(
            endpoints.source.url.as_str(),
            "https://env.ethereum-sepolia.example/"
        );
        assert_eq!(
            endpoints.destination.url.as_str(),
            "https://cli.base-sepolia.example/"
        );
        assert_eq!(
            provenance.source.source,
            ConfigValueSource::EnvVar(ETHEREUM_SEPOLIA_RPC_ENV)
        );
        assert_eq!(
            provenance.destination.source,
            ConfigValueSource::CliFlag("--base-sepolia-rpc")
        );
    }

    #[test]
    fn endpoint_catalog_maps_sources_by_chain_after_precedence() {
        let route = supported_route_config();
        let mut args = empty_args();
        args.hyperevm_rpc = Some("https://cli.hyperevm.example".to_owned());
        let mut file = BridgeConfigFile::default();
        file.ethereum_rpc = Some("https://file.ethereum.example".to_owned());
        file.hyperevm_rpc = Some("https://file.hyperevm.example".to_owned());
        let env = TestEnv(HashMap::from([(
            ETHEREUM_RPC_ENV.to_owned(),
            "https://env.ethereum.example".to_owned(),
        )]));

        let endpoints = CHAIN_ENDPOINT_CATALOG
            .resolve_route(&route, &args, &file, &env)
            .expect("endpoint catalog should apply per-chain precedence");
        let provenance = RpcEndpointsProvenance::from_resolved(&endpoints);

        assert_eq!(
            endpoints.source.url.as_str(),
            "https://env.ethereum.example/"
        );
        assert_eq!(
            endpoints.destination.url.as_str(),
            "https://cli.hyperevm.example/"
        );
        assert_eq!(
            provenance.source.source,
            ConfigValueSource::EnvVar(ETHEREUM_RPC_ENV)
        );
        assert_eq!(
            provenance.destination.source,
            ConfigValueSource::CliFlag("--hyperevm-rpc")
        );
    }

    #[test]
    fn endpoint_catalog_reports_chain_specific_parse_error() {
        let route = supported_route_config();
        let mut args = empty_args();
        args.ethereum_rpc = Some("not a url".to_owned());
        args.hyperevm_rpc = Some("https://destination.example".to_owned());

        let error = CHAIN_ENDPOINT_CATALOG
            .resolve_route(
                &route,
                &args,
                &BridgeConfigFile::default(),
                &TestEnv::default(),
            )
            .expect_err("invalid source endpoint should be rejected");
        let message = error.to_string();

        assert!(
            message.contains("failed to parse --ethereum-rpc as a URL"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn provider_validation_accepts_expected_chain_ids() {
        let route = supported_route_config();

        let validation =
            ProviderValidation::new(route, route.source_chain_id(), route.destination_chain_id())
                .expect("chain IDs match");

        assert_eq!(
            validation.source,
            ProviderChainCheck {
                role: ProviderEndpointRole::Source,
                expected: ExpectedProviderChain::new(ChainArg::Ethereum),
                actual_chain_id: route.source_chain_id()
            }
        );
        assert_eq!(
            validation.destination,
            ProviderChainCheck {
                role: ProviderEndpointRole::Destination,
                expected: ExpectedProviderChain::new(ChainArg::HyperEvm),
                actual_chain_id: route.destination_chain_id()
            }
        );
    }

    #[test]
    fn provider_validation_accepts_testnet_chain_ids() {
        let route = testnet_route_config();

        let validation =
            ProviderValidation::new(route, route.source_chain_id(), route.destination_chain_id())
                .expect("testnet chain IDs match");

        assert_eq!(
            validation.source,
            ProviderChainCheck {
                role: ProviderEndpointRole::Source,
                expected: ExpectedProviderChain::new(ChainArg::EthereumSepolia),
                actual_chain_id: 11_155_111
            }
        );
        assert_eq!(
            validation.destination,
            ProviderChainCheck {
                role: ProviderEndpointRole::Destination,
                expected: ExpectedProviderChain::new(ChainArg::BaseSepolia),
                actual_chain_id: 84_532
            }
        );
    }

    #[test]
    fn provider_validation_rejects_source_chain_mismatch_with_route_context() {
        let route = supported_route_config();

        let error = ProviderValidation::new(route, 31_337, route.destination_chain_id())
            .expect_err("source mismatch is invalid");

        let message = error.to_string();
        assert!(
            message.contains("source RPC"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("Ethereum mainnet -> HyperEVM"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("expected 1"),
            "unexpected error: {message}"
        );
        assert!(message.contains("got 31337"), "unexpected error: {message}");
    }

    #[test]
    fn provider_validation_rejects_mainnet_rpc_for_testnet_route_context() {
        let route = testnet_route_config();

        let error = ProviderValidation::new(route, 1, route.destination_chain_id())
            .expect_err("mainnet RPC on testnet route is invalid");

        let message = error.to_string();
        assert!(
            message.contains("source RPC"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("Ethereum Sepolia testnet -> Base Sepolia testnet"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("expected 11155111"),
            "unexpected error: {message}"
        );
        assert!(message.contains("got 1"), "unexpected error: {message}");
    }

    #[test]
    fn provider_validation_rejects_destination_chain_mismatch_with_route_context() {
        let route = supported_route_config();

        let error = ProviderValidation::new(route, route.source_chain_id(), 31_337)
            .expect_err("destination mismatch is invalid");

        let message = error.to_string();
        assert!(
            message.contains("destination RPC"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("Ethereum mainnet -> HyperEVM"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains(&format!("expected {}", route.destination_chain_id())),
            "unexpected error: {message}"
        );
        assert!(message.contains("got 31337"), "unexpected error: {message}");
    }
}
