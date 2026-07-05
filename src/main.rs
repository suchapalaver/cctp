use std::{
    fs,
    io::{self, Write},
    path::{Path, PathBuf},
    time::Duration,
};

use alloy::{
    primitives::{Address, TxHash, U256, address},
    providers::{DynProvider, Provider, ProviderBuilder},
    signers::trezor::{HDPath, TrezorSigner},
};
use alloy_chains::NamedChain;
use async_trait::async_trait;
use cctp_rs::{
    AttestationBytes, CctpV2Bridge, CctpV2Route, DomainId, FeeBps, MintResult, PollingConfig,
    TransferFee, TransferMode, UsdcAmount,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use eyre::{Result, WrapErr, bail, eyre};
use serde::{Deserialize, Serialize, Serializer, ser::SerializeStruct};
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;
use url::Url;

const MAINNET_USDC: Address = address!("A0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const ETHEREUM_SEPOLIA_USDC: Address = address!("1c7D4B196Cb0C7B01d743Fbc6116a902379C7238");
const DEFAULT_LOG_FILTER: &str = "info,cctp_rs=info";
const ETHEREUM_RPC_ENV: &str = "ETHEREUM_RPC_URL";
const HYPEREVM_RPC_ENV: &str = "HYPEREVM_RPC_URL";
const ETHEREUM_SEPOLIA_RPC_ENV: &str = "ETHEREUM_SEPOLIA_RPC_URL";
const BASE_SEPOLIA_RPC_ENV: &str = "BASE_SEPOLIA_RPC_URL";
const DEFAULT_FAST_FEE_BUFFER_PERCENT: u32 = 20;

#[derive(Debug, Parser)]
#[command(name = "cctp")]
#[command(about = "Bridge USDC with cctp-rs and a Trezor-backed Alloy signer")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Bridge USDC over a supported CCTP route.
    Bridge(BridgeArgs),
}

#[derive(Debug, Args)]
struct BridgeArgs {
    /// Optional TOML config file. CLI flags override env, env overrides file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Source chain.
    #[arg(long)]
    from: Option<ChainArg>,

    /// Destination chain.
    #[arg(long)]
    to: Option<ChainArg>,

    /// USDC amount, in decimal units, e.g. 10 or 10.25.
    #[arg(long)]
    amount: Option<String>,

    /// Destination recipient. Defaults to the Trezor account address.
    #[arg(long)]
    recipient: Option<Address>,

    /// Ethereum mainnet RPC URL.
    #[arg(long)]
    ethereum_rpc: Option<String>,

    /// HyperEVM RPC URL.
    #[arg(long)]
    hyperevm_rpc: Option<String>,

    /// Ethereum Sepolia testnet RPC URL.
    #[arg(long)]
    ethereum_sepolia_rpc: Option<String>,

    /// Base Sepolia testnet RPC URL.
    #[arg(long)]
    base_sepolia_rpc: Option<String>,

    /// Wallet backend.
    #[arg(long, value_enum)]
    wallet: Option<WalletKind>,

    /// Trezor Live account index: m/44'/60'/account'/0/0.
    #[arg(long)]
    trezor_account: Option<u32>,

    /// Trezor Live account index used only for --self-relay on the destination chain.
    ///
    /// Defaults to --trezor-account when omitted.
    #[arg(long)]
    relay_trezor_account: Option<u32>,

    /// Override the source-chain USDC address.
    #[arg(long)]
    usdc: Option<Address>,

    /// Request fast CCTP v2 finality.
    #[arg(
        long,
        default_missing_value = "true",
        num_args = 0..=1,
        require_equals = true,
        value_parser = clap::value_parser!(bool),
    )]
    fast: Option<bool>,

    /// Optional fast-transfer fee cap in USDC decimal units.
    ///
    /// When omitted, the CLI resolves the live route fee and adds a buffer.
    #[arg(long)]
    max_fee_usdc: Option<String>,

    /// Submit receiveMessage from the Trezor account on the destination chain.
    ///
    /// Without this flag the CLI waits for any permissionless relayer to complete
    /// the mint, which avoids requiring destination-chain gas in the Trezor account.
    #[arg(
        long,
        default_missing_value = "true",
        num_args = 0..=1,
        require_equals = true,
        value_parser = clap::value_parser!(bool),
    )]
    self_relay: Option<bool>,

    /// Optional receive-status polling attempt override.
    #[arg(long)]
    receive_attempts: Option<u32>,

    /// Optional receive-status polling interval, in seconds.
    #[arg(long)]
    receive_interval_secs: Option<u64>,

    /// Print the bridge intent without sending transactions.
    #[arg(
        long,
        default_missing_value = "true",
        num_args = 0..=1,
        require_equals = true,
        value_parser = clap::value_parser!(bool),
    )]
    dry_run: Option<bool>,

    /// Print the bridge intent but skip the interactive CONFIRM prompt.
    ///
    /// Intended for explicit non-interactive automation. Ignored by --dry-run.
    #[arg(long)]
    yes: bool,

    /// Reporter output mode.
    #[arg(long, value_enum)]
    output: Option<OutputMode>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum ChainArg {
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
    const fn named_chain(self) -> NamedChain {
        match self {
            Self::Ethereum => NamedChain::Mainnet,
            Self::HyperEvm => NamedChain::Hyperliquid,
            Self::EthereumSepolia => NamedChain::Sepolia,
            Self::BaseSepolia => NamedChain::BaseSepolia,
        }
    }

    const fn display_label(self) -> &'static str {
        match self {
            Self::Ethereum => "Ethereum mainnet",
            Self::HyperEvm => "HyperEVM",
            Self::EthereumSepolia => "Ethereum Sepolia testnet",
            Self::BaseSepolia => "Base Sepolia testnet",
        }
    }

    fn chain_id(self) -> u64 {
        u64::from(self.named_chain())
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum WalletKind {
    #[serde(rename = "trezor")]
    Trezor,
}

impl std::fmt::Display for WalletKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trezor => f.write_str("trezor"),
        }
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
enum OutputMode {
    Human,
    Json,
}

impl std::fmt::Display for OutputMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Human => f.write_str("human"),
            Self::Json => f.write_str("json"),
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    load_dotenv()?;

    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG_FILTER));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(io::stderr)
        .init();

    let cli = Cli::parse();
    match cli.command {
        Commands::Bridge(args) => run_bridge(args).await,
    }
}

fn load_dotenv() -> Result<()> {
    match dotenvy::dotenv() {
        Ok(_) => Ok(()),
        Err(dotenvy::Error::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error).wrap_err("failed to load .env"),
    }
}

async fn run_bridge(args: BridgeArgs) -> Result<()> {
    let config = CliConfigService::default().bridge_config(args)?;
    BridgeApp::production(config.output)
        .run(config)
        .await
        .map(|_| ())
}

#[derive(Clone, Debug)]
struct BridgeApp<W, P, V, F, A, R> {
    wallet_service: W,
    provider_service: P,
    provider_validation_service: V,
    fee_resolution_service: F,
    approval_service: A,
    reporter: R,
}

type ProductionBridgeApp = BridgeApp<
    TrezorWalletService,
    AlloyProviderService,
    AlloyProviderValidationService,
    CctpFeeResolutionService,
    TerminalIntentApprovalService,
    ConfiguredReporter,
>;

impl ProductionBridgeApp {
    fn production(output: OutputMode) -> Self {
        Self::new(
            TrezorWalletService,
            AlloyProviderService,
            AlloyProviderValidationService,
            CctpFeeResolutionService,
            TerminalIntentApprovalService,
            ConfiguredReporter::from_output_mode(output),
        )
    }
}

impl<W, P, V, F, A, R> BridgeApp<W, P, V, F, A, R> {
    const fn new(
        wallet_service: W,
        provider_service: P,
        provider_validation_service: V,
        fee_resolution_service: F,
        approval_service: A,
        reporter: R,
    ) -> Self {
        Self {
            wallet_service,
            provider_service,
            provider_validation_service,
            fee_resolution_service,
            approval_service,
            reporter,
        }
    }
}

impl<W, P, V, F, A, R> BridgeApp<W, P, V, F, A, R>
where
    W: WalletService,
    P: ProviderService<W>,
    V: ProviderValidationService<P::Providers>,
    F: FeeResolutionService<P::Bridge>,
    A: IntentApprovalService,
    R: Reporter,
{
    async fn run(&self, config: BridgeConfig) -> Result<BridgeRunResult> {
        let validation_providers = self.provider_service.read_only_providers(&config);
        let provider_validation = self
            .provider_validation_service
            .validate(&config, &validation_providers)
            .await?;

        let source_signer = self.wallet_service.source_signer(&config).await?;
        let relay_signer = self.wallet_service.relay_signer(&config).await?;
        let source_account = source_signer.account;
        let relay_account = relay_signer.account;
        let source_signer_address = source_account.address;
        let relay = ResolvedRelay::from_config(config.relay, relay_account);
        let workflow_relay = relay.workflow_relay();
        let recipient = config.recipient.resolve(source_signer_address);

        let providers =
            self.provider_service
                .bridge_providers(&config, source_signer.signer, relay_signer);
        let fee_bridge =
            self.provider_service
                .bridge(&config, &providers, recipient, TransferMode::Standard);
        let resolved_transfer = self
            .fee_resolution_service
            .resolve(&fee_bridge, &config)
            .await
            .wrap_err("failed to resolve transfer fee policy")?;
        let bridge = self.provider_service.bridge(
            &config,
            &providers,
            recipient,
            resolved_transfer.transfer_mode(),
        );
        let contracts = self.provider_service.contracts(&bridge)?;
        let intent = BridgeIntent::new(
            &config,
            source_account,
            recipient,
            relay,
            provider_validation,
            contracts,
            resolved_transfer,
        );
        self.reporter.report_intent(&intent)?;

        if config.run_mode.is_dry_run() {
            self.reporter.report_dry_run_complete()?;
            return Ok(BridgeRunResult::DryRun);
        }

        self.approval_service
            .confirm(&intent, config.confirmation)?;
        self.reporter.report_workflow_start()?;

        let runtime = self.provider_service.runtime(bridge, providers);
        let mut workflow = BridgeWorkflow::new(
            BridgeWorkflowConfig::new(&config, resolved_transfer.transfer_mode(), workflow_relay),
            runtime,
            source_signer_address,
            recipient,
        );
        let outcome = workflow.run().await?;
        self.reporter.report_outcome(&outcome)?;
        Ok(BridgeRunResult::Executed(outcome))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum BridgeRunResult {
    DryRun,
    Executed(BridgeOutcome),
}

trait ConfigService {
    fn bridge_config(&self, args: BridgeArgs) -> Result<BridgeConfig>;
}

trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

#[derive(Clone, Copy, Debug, Default)]
struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    }
}

#[derive(Clone, Debug)]
struct CliConfigService<E = ProcessEnv> {
    env: E,
}

impl Default for CliConfigService<ProcessEnv> {
    fn default() -> Self {
        Self { env: ProcessEnv }
    }
}

#[cfg(test)]
impl<E> CliConfigService<E> {
    const fn new(env: E) -> Self {
        Self { env }
    }
}

impl<E> ConfigService for CliConfigService<E>
where
    E: EnvSource,
{
    fn bridge_config(&self, args: BridgeArgs) -> Result<BridgeConfig> {
        let file = BridgeConfigFile::read_optional(args.config.as_deref())?;

        let cli_fast = args.fast;
        let from = sourced_cli_file_default(
            args.from,
            "--from",
            file.from,
            "from",
            ChainArg::Ethereum,
            "ethereum",
        );
        let to = sourced_cli_file_default(
            args.to,
            "--to",
            file.to,
            "to",
            ChainArg::HyperEvm,
            "hyperevm",
        );
        let route = ROUTE_CATALOG.resolve(from.value, to.value)?;

        let amount = sourced_required_cli_file(
            args.amount.clone(),
            "--amount",
            file.amount.clone(),
            "amount",
            "missing amount; set --amount or amount in the config file",
        )?;

        let resolved_rpc = CHAIN_ENDPOINT_CATALOG.resolve_route(&route, &args, &file, &self.env)?;

        let wallet = sourced_cli_file_default(
            args.wallet,
            "--wallet",
            file.wallet,
            "wallet",
            WalletKind::Trezor,
            "trezor",
        );
        let trezor_account = sourced_cli_file_default(
            args.trezor_account,
            "--trezor-account",
            file.trezor_account,
            "trezor_account",
            0,
            "trezor account 0",
        );
        let relay_trezor_account = sourced_optional_cli_file(
            args.relay_trezor_account,
            "--relay-trezor-account",
            file.relay_trezor_account,
            "relay_trezor_account",
        );
        let self_relay = sourced_cli_file_default(
            args.self_relay,
            "--self-relay",
            file.self_relay,
            "self_relay",
            false,
            "wait for relayer",
        );
        let fast = sourced_cli_file_default(
            args.fast,
            "--fast",
            file.fast,
            "fast",
            false,
            "standard finality",
        );
        let max_fee_usdc = if !fast.value && cli_fast == Some(false) {
            sourced_optional_cli_only(args.max_fee_usdc, "--max-fee-usdc")
        } else {
            sourced_optional_cli_file(
                args.max_fee_usdc,
                "--max-fee-usdc",
                file.max_fee_usdc,
                "max_fee_usdc",
            )
        };
        let run_mode = BridgeRunMode::from_dry_run(args.dry_run.or(file.dry_run).unwrap_or(false));
        let output = sourced_cli_file_default(
            args.output,
            "--output",
            file.output,
            "output",
            OutputMode::Human,
            "human",
        );
        let receive_polling = ReceivePolling::new(
            args.receive_attempts.or(file.receive_attempts),
            args.receive_interval_secs.or(file.receive_interval_secs),
        )?;
        let relay_mode = RelayMode::from_self_relay(self_relay.value);

        let source_wallet = WalletConfig::from_kind(wallet.value, trezor_account.value);
        source_wallet.validate()?;
        let relay = RelayConfig::from_mode(
            relay_mode,
            wallet.value,
            relay_trezor_account.as_ref().map(|account| account.value),
            trezor_account.value,
        );
        relay.validate()?;
        let rpc = RpcEndpoints::from_resolved(&resolved_rpc);
        let transfer = transfer_request(fast.value, max_fee_usdc.as_ref())?;
        let recipient =
            sourced_optional_cli_file(args.recipient, "--recipient", file.recipient, "recipient");
        let provenance = BridgeConfigProvenance {
            route: RouteConfigProvenance {
                from: from.source,
                to: to.source,
            },
            amount: amount.source,
            rpc: RpcEndpointsProvenance::from_resolved(&resolved_rpc),
            source_wallet: SourceWalletProvenance {
                wallet: wallet.source,
                account: trezor_account.source,
            },
            relay_wallet: RelayWalletProvenance::from_config(
                relay_trezor_account.as_ref().map(|account| account.source),
            ),
            recipient: RecipientProvenance::from_source(
                recipient.as_ref().map(|recipient| recipient.source),
            ),
            relay_mode: self_relay.source,
            fast_mode: fast.source,
            max_fee: max_fee_provenance(&transfer),
            output: output.source,
        };

        Ok(BridgeConfig {
            route,
            amount: UsdcAmount::parse_decimal(&amount.value)?,
            rpc,
            source_wallet,
            relay,
            recipient: RecipientConfig::from(recipient.map(|recipient| recipient.value)),
            usdc: args.usdc.or(file.usdc).unwrap_or(route.default_usdc()),
            transfer,
            receive_polling,
            run_mode,
            confirmation: ConfirmationPolicy::from_yes(args.yes),
            output: output.value,
            provenance,
        })
    }
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeConfigFile {
    from: Option<ChainArg>,
    to: Option<ChainArg>,
    amount: Option<String>,
    recipient: Option<Address>,
    ethereum_rpc: Option<String>,
    hyperevm_rpc: Option<String>,
    ethereum_sepolia_rpc: Option<String>,
    base_sepolia_rpc: Option<String>,
    wallet: Option<WalletKind>,
    trezor_account: Option<u32>,
    relay_trezor_account: Option<u32>,
    usdc: Option<Address>,
    fast: Option<bool>,
    max_fee_usdc: Option<String>,
    self_relay: Option<bool>,
    receive_attempts: Option<u32>,
    receive_interval_secs: Option<u64>,
    dry_run: Option<bool>,
    output: Option<OutputMode>,
}

impl BridgeConfigFile {
    fn read_optional(path: Option<&Path>) -> Result<Self> {
        let Some(path) = path else {
            return Ok(Self::default());
        };

        let contents = fs::read_to_string(path)
            .wrap_err_with(|| format!("failed to read config file {}", path.display()))?;
        toml::from_str(&contents)
            .wrap_err_with(|| format!("failed to parse config file {}", path.display()))
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfigValueSource {
    CliFlag(&'static str),
    EnvVar(&'static str),
    ConfigFile(&'static str),
    Default(&'static str),
}

impl std::fmt::Display for ConfigValueSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CliFlag(flag) => write!(f, "CLI flag {flag}"),
            Self::EnvVar(var) => write!(f, "env {var}"),
            Self::ConfigFile(field) => write!(f, "config field {field}"),
            Self::Default(label) => write!(f, "default {label}"),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Sourced<T> {
    value: T,
    source: ConfigValueSource,
}

impl<T> Sourced<T> {
    const fn new(value: T, source: ConfigValueSource) -> Self {
        Self { value, source }
    }
}

fn sourced_cli_file_default<T>(
    cli: Option<T>,
    cli_flag: &'static str,
    file: Option<T>,
    file_field: &'static str,
    default: T,
    default_label: &'static str,
) -> Sourced<T> {
    if let Some(value) = cli {
        return Sourced::new(value, ConfigValueSource::CliFlag(cli_flag));
    }
    if let Some(value) = file {
        return Sourced::new(value, ConfigValueSource::ConfigFile(file_field));
    }

    Sourced::new(default, ConfigValueSource::Default(default_label))
}

fn sourced_optional_cli_file<T>(
    cli: Option<T>,
    cli_flag: &'static str,
    file: Option<T>,
    file_field: &'static str,
) -> Option<Sourced<T>> {
    if let Some(value) = cli {
        return Some(Sourced::new(value, ConfigValueSource::CliFlag(cli_flag)));
    }
    file.map(|value| Sourced::new(value, ConfigValueSource::ConfigFile(file_field)))
}

fn sourced_optional_cli_only<T>(cli: Option<T>, cli_flag: &'static str) -> Option<Sourced<T>> {
    cli.map(|value| Sourced::new(value, ConfigValueSource::CliFlag(cli_flag)))
}

fn sourced_required_cli_file<T>(
    cli: Option<T>,
    cli_flag: &'static str,
    file: Option<T>,
    file_field: &'static str,
    missing_message: &'static str,
) -> Result<Sourced<T>> {
    sourced_optional_cli_file(cli, cli_flag, file, file_field).ok_or_else(|| eyre!(missing_message))
}

fn sourced_required_cli_env_file(
    cli: Option<String>,
    cli_flag: &'static str,
    env: Option<String>,
    env_var: &'static str,
    file: Option<String>,
    file_field: &'static str,
    missing_message: &'static str,
) -> Result<Sourced<String>> {
    if let Some(value) = cli {
        return Ok(Sourced::new(value, ConfigValueSource::CliFlag(cli_flag)));
    }
    if let Some(value) = env {
        return Ok(Sourced::new(value, ConfigValueSource::EnvVar(env_var)));
    }
    if let Some(value) = file {
        return Ok(Sourced::new(
            value,
            ConfigValueSource::ConfigFile(file_field),
        ));
    }

    Err(eyre!(missing_message))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BridgeConfigProvenance {
    route: RouteConfigProvenance,
    amount: ConfigValueSource,
    rpc: RpcEndpointsProvenance,
    source_wallet: SourceWalletProvenance,
    relay_wallet: RelayWalletProvenance,
    recipient: RecipientProvenance,
    relay_mode: ConfigValueSource,
    fast_mode: ConfigValueSource,
    max_fee: MaxFeeProvenance,
    output: ConfigValueSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RouteConfigProvenance {
    from: ConfigValueSource,
    to: ConfigValueSource,
}

impl std::fmt::Display for RouteConfigProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "--from {}, --to {}", self.from, self.to)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RpcEndpointsProvenance {
    source: RpcEndpointProvenance,
    destination: RpcEndpointProvenance,
}

impl RpcEndpointsProvenance {
    fn from_resolved(endpoints: &ResolvedChainRpcEndpoints) -> Self {
        Self {
            source: RpcEndpointProvenance::from_url(
                endpoints.source.config_source,
                &endpoints.source.url,
            ),
            destination: RpcEndpointProvenance::from_url(
                endpoints.destination.config_source,
                &endpoints.destination.url,
            ),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RpcEndpointProvenance {
    source: ConfigValueSource,
    redacted_endpoint: String,
}

impl RpcEndpointProvenance {
    fn from_url(source: ConfigValueSource, url: &Url) -> Self {
        Self {
            source,
            redacted_endpoint: redact_rpc_endpoint(url),
        }
    }
}

fn redact_rpc_endpoint(url: &Url) -> String {
    match url.host_str() {
        Some(host) => {
            let host = redact_rpc_host(host);
            match url.port() {
                Some(port) => format!("{}://{host}:{port}/...", url.scheme()),
                None => format!("{}://{host}/...", url.scheme()),
            }
        }
        None => format!("{}://<redacted>/...", url.scheme()),
    }
}

fn redact_rpc_host(host: &str) -> String {
    if host == "localhost" || host.parse::<std::net::IpAddr>().is_ok() {
        return host.to_owned();
    }

    let labels: Vec<&str> = host.split('.').collect();
    match labels.as_slice() {
        [] => "<redacted>".to_owned(),
        [_] => "<redacted>".to_owned(),
        [_, suffix @ ..] => format!("<redacted>.{}", suffix.join(".")),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SourceWalletProvenance {
    wallet: ConfigValueSource,
    account: ConfigValueSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayWalletProvenance {
    ExplicitAccount { account: ConfigValueSource },
    DefaultedToSourceAccount,
}

impl RelayWalletProvenance {
    const fn from_config(relay_account: Option<ConfigValueSource>) -> Self {
        match relay_account {
            Some(account) => Self::ExplicitAccount { account },
            None => Self::DefaultedToSourceAccount,
        }
    }
}

impl std::fmt::Display for RelayWalletProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExplicitAccount { account } => write!(f, "account from {account}"),
            Self::DefaultedToSourceAccount => f.write_str("defaulted to source account"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecipientProvenance {
    Explicit { source: ConfigValueSource },
    DefaultedToSourceSigner,
}

impl RecipientProvenance {
    const fn from_source(source: Option<ConfigValueSource>) -> Self {
        match source {
            Some(source) => Self::Explicit { source },
            None => Self::DefaultedToSourceSigner,
        }
    }
}

impl std::fmt::Display for RecipientProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Explicit { source } => write!(f, "explicit from {source}"),
            Self::DefaultedToSourceSigner => f.write_str("defaulted to source signer"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MaxFeeProvenance {
    NotApplicable,
    Manual { source: ConfigValueSource },
    AutoResolved { source: ConfigValueSource },
}

impl std::fmt::Display for MaxFeeProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotApplicable => f.write_str("not applicable"),
            Self::Manual { source } => write!(f, "manual cap from {source}"),
            Self::AutoResolved { source } => write!(f, "auto-resolved cap from {source}"),
        }
    }
}

fn max_fee_provenance(transfer: &TransferRequest) -> MaxFeeProvenance {
    match transfer {
        TransferRequest::Standard => MaxFeeProvenance::NotApplicable,
        TransferRequest::Fast {
            fee_cap: FastFeeCapRequest::Manual(cap),
        } => MaxFeeProvenance::Manual { source: cap.source },
        TransferRequest::Fast {
            fee_cap: FastFeeCapRequest::Auto,
        } => MaxFeeProvenance::AutoResolved {
            source: ConfigValueSource::Default("live fee + 20% buffer"),
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BridgeRunMode {
    Execute,
    DryRun,
}

impl BridgeRunMode {
    const fn from_dry_run(dry_run: bool) -> Self {
        if dry_run { Self::DryRun } else { Self::Execute }
    }

    const fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun)
    }
}

#[derive(Clone, Debug)]
struct BridgeConfig {
    route: RouteConfig,
    amount: UsdcAmount,
    rpc: RpcEndpoints,
    source_wallet: WalletConfig,
    relay: RelayConfig,
    recipient: RecipientConfig,
    usdc: Address,
    transfer: TransferRequest,
    receive_polling: ReceivePolling,
    run_mode: BridgeRunMode,
    confirmation: ConfirmationPolicy,
    output: OutputMode,
    provenance: BridgeConfigProvenance,
}

#[derive(Clone, Debug)]
struct BridgeWorkflowConfig {
    amount: UsdcAmount,
    usdc: Address,
    transfer_mode: TransferMode,
    relay: WorkflowRelay,
    receive_polling: ReceivePolling,
}

impl BridgeWorkflowConfig {
    const fn new(config: &BridgeConfig, transfer_mode: TransferMode, relay: WorkflowRelay) -> Self {
        Self {
            amount: config.amount,
            usdc: config.usdc,
            transfer_mode,
            relay,
            receive_polling: config.receive_polling,
        }
    }

    fn attestation_polling_config(&self) -> PollingConfig {
        if self.transfer_mode.is_fast() {
            PollingConfig::fast_transfer()
        } else {
            PollingConfig::default()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BridgeOutcome {
    source_sender: Address,
    recipient: Address,
    token_messenger: Address,
    destination_domain: DomainId,
    approval: ApprovalOutcome,
    burn_tx: TxHash,
    attestation: AttestationOutcome,
    completion: CompletionOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum ApprovalOutcome {
    Skipped { allowance: U256 },
    Sent { tx_hash: TxHash },
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AttestationOutcome {
    message_len: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CompletionOutcome {
    RelayerCompleted,
    SelfRelayMinted { tx_hash: TxHash },
    SelfRelayAlreadyCompleted,
    FallbackSelfRelayMinted { tx_hash: TxHash },
    FallbackSelfRelayAlreadyCompleted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkflowRelay {
    WaitForRelayer { fallback: WorkflowRelayFallback },
    SelfRelay { submitter: Address },
}

struct BridgeWorkflow<R> {
    config: BridgeWorkflowConfig,
    runtime: R,
    source_sender: Address,
    recipient: Address,
}

impl<R> BridgeWorkflow<R>
where
    R: BridgeRuntime,
{
    const fn new(
        config: BridgeWorkflowConfig,
        runtime: R,
        source_sender: Address,
        recipient: Address,
    ) -> Self {
        Self {
            config,
            runtime,
            source_sender,
            recipient,
        }
    }

    async fn run(&mut self) -> Result<BridgeOutcome> {
        let token_messenger = self.runtime.token_messenger_v2_contract()?;
        let destination_domain = self.runtime.destination_domain_id()?;
        let amount = self.config.amount.atomic();

        let allowance = self
            .runtime
            .get_allowance(self.config.usdc, self.source_sender)
            .await
            .wrap_err("failed to read USDC allowance")?;

        let approval = if allowance < amount {
            let tx_hash = self
                .runtime
                .approve(self.config.usdc, self.source_sender, amount)
                .await
                .wrap_err("failed to send USDC approval transaction")?;
            self.runtime
                .wait_source_receipt(tx_hash, "approval", 120, Duration::from_secs(12))
                .await?;
            ApprovalOutcome::Sent { tx_hash }
        } else {
            ApprovalOutcome::Skipped { allowance }
        };

        let burn_tx = self
            .runtime
            .burn(amount, self.source_sender, self.config.usdc)
            .await
            .wrap_err("failed to send CCTP burn transaction")?;
        self.runtime
            .wait_source_receipt(burn_tx, "burn", 120, Duration::from_secs(12))
            .await?;

        let (message, attestation) = self
            .runtime
            .get_attestation(burn_tx, self.config.attestation_polling_config())
            .await
            .wrap_err("failed to get CCTP attestation from Iris")?;
        let attestation_outcome = AttestationOutcome {
            message_len: message.len(),
        };

        let completion = match self.config.relay {
            WorkflowRelay::WaitForRelayer { fallback } => {
                match self
                    .runtime
                    .wait_for_receive(
                        &message,
                        self.config.receive_polling.attempts,
                        self.config.receive_polling.interval_secs,
                    )
                    .await
                {
                    Ok(()) => CompletionOutcome::RelayerCompleted,
                    Err(wait_error) => {
                        let submitter = fallback.submitter();
                        match self
                            .runtime
                            .mint_if_needed(message, attestation, submitter)
                            .await
                            .wrap_err_with(|| {
                                format!(
                                    "permissionless relayer did not complete before fallback and fallback self-relay failed: {wait_error}"
                                )
                            })? {
                            MintResult::Minted(tx_hash) => {
                                self.runtime
                                    .wait_destination_receipt(
                                        tx_hash,
                                        "mint",
                                        120,
                                        Duration::from_secs(2),
                                    )
                                    .await?;
                                CompletionOutcome::FallbackSelfRelayMinted { tx_hash }
                            }
                            MintResult::AlreadyRelayed => {
                                CompletionOutcome::FallbackSelfRelayAlreadyCompleted
                            }
                        }
                    }
                }
            }
            WorkflowRelay::SelfRelay { submitter } => {
                match self
                    .runtime
                    .mint_if_needed(message, attestation, submitter)
                    .await
                    .wrap_err("failed to self-relay CCTP mint on destination chain")?
                {
                    MintResult::Minted(tx_hash) => {
                        self.runtime
                            .wait_destination_receipt(tx_hash, "mint", 120, Duration::from_secs(2))
                            .await?;
                        CompletionOutcome::SelfRelayMinted { tx_hash }
                    }
                    MintResult::AlreadyRelayed => CompletionOutcome::SelfRelayAlreadyCompleted,
                }
            }
        };

        Ok(BridgeOutcome {
            source_sender: self.source_sender,
            recipient: self.recipient,
            token_messenger,
            destination_domain,
            approval,
            burn_tx,
            attestation: attestation_outcome,
            completion,
        })
    }
}

#[async_trait(?Send)]
trait BridgeRuntime {
    fn token_messenger_v2_contract(&self) -> Result<Address>;

    fn destination_domain_id(&self) -> Result<DomainId>;

    async fn get_allowance(&mut self, token: Address, owner: Address) -> Result<U256>;

    async fn approve(&mut self, token: Address, owner: Address, amount: U256) -> Result<TxHash>;

    async fn burn(&mut self, amount: U256, burn_sender: Address, token: Address) -> Result<TxHash>;

    async fn get_attestation(
        &mut self,
        burn_tx: TxHash,
        polling_config: PollingConfig,
    ) -> Result<(Vec<u8>, AttestationBytes)>;

    async fn wait_for_receive(
        &mut self,
        message: &[u8],
        max_attempts: Option<u32>,
        poll_interval: Option<u64>,
    ) -> Result<()>;

    async fn mint_if_needed(
        &mut self,
        message: Vec<u8>,
        attestation: AttestationBytes,
        from: Address,
    ) -> Result<MintResult>;

    async fn wait_source_receipt(
        &mut self,
        tx_hash: TxHash,
        label: &str,
        max_attempts: u32,
        interval: Duration,
    ) -> Result<()>;

    async fn wait_destination_receipt(
        &mut self,
        tx_hash: TxHash,
        label: &str,
        max_attempts: u32,
        interval: Duration,
    ) -> Result<()>;
}

struct CctpBridgeRuntime<P>
where
    P: Provider + Clone,
{
    bridge: CctpV2Bridge<P>,
    source_provider: P,
    destination_provider: P,
}

impl<P> CctpBridgeRuntime<P>
where
    P: Provider + Clone,
{
    const fn new(bridge: CctpV2Bridge<P>, source_provider: P, destination_provider: P) -> Self {
        Self {
            bridge,
            source_provider,
            destination_provider,
        }
    }
}

#[async_trait(?Send)]
impl<P> BridgeRuntime for CctpBridgeRuntime<P>
where
    P: Provider + Clone,
{
    fn token_messenger_v2_contract(&self) -> Result<Address> {
        Ok(self.bridge.token_messenger_v2_contract()?)
    }

    fn destination_domain_id(&self) -> Result<DomainId> {
        Ok(self.bridge.destination_domain_id()?)
    }

    async fn get_allowance(&mut self, token: Address, owner: Address) -> Result<U256> {
        Ok(self.bridge.get_allowance(token, owner).await?)
    }

    async fn approve(&mut self, token: Address, owner: Address, amount: U256) -> Result<TxHash> {
        Ok(self.bridge.approve(token, owner, amount).await?)
    }

    async fn burn(&mut self, amount: U256, burn_sender: Address, token: Address) -> Result<TxHash> {
        Ok(self.bridge.burn(amount, burn_sender, token).await?)
    }

    async fn get_attestation(
        &mut self,
        burn_tx: TxHash,
        polling_config: PollingConfig,
    ) -> Result<(Vec<u8>, AttestationBytes)> {
        Ok(self.bridge.get_attestation(burn_tx, polling_config).await?)
    }

    async fn wait_for_receive(
        &mut self,
        message: &[u8],
        max_attempts: Option<u32>,
        poll_interval: Option<u64>,
    ) -> Result<()> {
        Ok(self
            .bridge
            .wait_for_receive(message, max_attempts, poll_interval)
            .await?)
    }

    async fn mint_if_needed(
        &mut self,
        message: Vec<u8>,
        attestation: AttestationBytes,
        from: Address,
    ) -> Result<MintResult> {
        Ok(self
            .bridge
            .mint_if_needed(message, attestation, from)
            .await?)
    }

    async fn wait_source_receipt(
        &mut self,
        tx_hash: TxHash,
        label: &str,
        max_attempts: u32,
        interval: Duration,
    ) -> Result<()> {
        wait_for_receipt(
            &self.source_provider,
            tx_hash,
            label,
            max_attempts,
            interval,
        )
        .await
    }

    async fn wait_destination_receipt(
        &mut self,
        tx_hash: TxHash,
        label: &str,
        max_attempts: u32,
        interval: Duration,
    ) -> Result<()> {
        wait_for_receipt(
            &self.destination_provider,
            tx_hash,
            label,
            max_attempts,
            interval,
        )
        .await
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RouteConfig {
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

    fn source_chain_id(&self) -> u64 {
        self.from.chain_id()
    }

    fn destination_chain_id(&self) -> u64 {
        self.to.chain_id()
    }

    const fn cctp_route(&self) -> CctpV2Route {
        self.route
    }

    const fn from(&self) -> ChainArg {
        self.from
    }

    const fn to(&self) -> ChainArg {
        self.to
    }

    const fn source_label(&self) -> &'static str {
        self.from.display_label()
    }

    const fn destination_label(&self) -> &'static str {
        self.to.display_label()
    }

    const fn default_usdc(&self) -> Address {
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

    const fn source_chain(self) -> NamedChain {
        self.from.named_chain()
    }

    const fn destination_chain(self) -> NamedChain {
        self.to.named_chain()
    }

    fn cli_pair(self) -> String {
        format!("{} -> {}", self.from, self.to)
    }
}

#[derive(Clone, Copy, Debug)]
struct RouteCatalog;

const ROUTE_CATALOG: RouteCatalog = RouteCatalog;

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
    fn resolve(&self, from: ChainArg, to: ChainArg) -> Result<RouteConfig> {
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

#[derive(Clone, Debug)]
struct RpcEndpoints {
    source: Url,
    destination: Url,
}

impl RpcEndpoints {
    fn from_resolved(endpoints: &ResolvedChainRpcEndpoints) -> Self {
        Self {
            source: endpoints.source.url.clone(),
            destination: endpoints.destination.url.clone(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct ChainEndpointCatalog;

const CHAIN_ENDPOINT_CATALOG: ChainEndpointCatalog = ChainEndpointCatalog;

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
    fn resolve_route<E>(
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
        let url = raw_endpoint.value.parse().wrap_err(endpoint.parse_error)?;

        Ok(ResolvedChainRpcEndpoint {
            url,
            config_source: raw_endpoint.source,
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
struct ResolvedChainRpcEndpoints {
    source: ResolvedChainRpcEndpoint,
    destination: ResolvedChainRpcEndpoint,
}

#[derive(Clone, Debug)]
struct ResolvedChainRpcEndpoint {
    url: Url,
    config_source: ConfigValueSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RecipientConfig {
    Signer,
    Address(Address),
}

impl RecipientConfig {
    const fn resolve(self, signer_address: Address) -> Address {
        match self {
            Self::Signer => signer_address,
            Self::Address(address) => address,
        }
    }
}

impl From<Option<Address>> for RecipientConfig {
    fn from(value: Option<Address>) -> Self {
        match value {
            Some(address) => Self::Address(address),
            None => Self::Signer,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum RelayMode {
    WaitForRelayer,
    SelfRelay,
}

impl RelayMode {
    const fn from_self_relay(self_relay: bool) -> Self {
        if self_relay {
            Self::SelfRelay
        } else {
            Self::WaitForRelayer
        }
    }
}

impl std::fmt::Display for RelayMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WaitForRelayer => f.write_str("wait for any permissionless relayer"),
            Self::SelfRelay => f.write_str("self-relay on destination chain"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ConfirmationPolicy {
    RequireInteractive,
    SkipPrompt,
}

impl ConfirmationPolicy {
    const fn from_yes(yes: bool) -> Self {
        if yes {
            Self::SkipPrompt
        } else {
            Self::RequireInteractive
        }
    }
}

trait IntentApprovalService {
    fn confirm(&self, intent: &BridgeIntent, policy: ConfirmationPolicy) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
struct TerminalIntentApprovalService;

impl IntentApprovalService for TerminalIntentApprovalService {
    fn confirm(&self, intent: &BridgeIntent, policy: ConfirmationPolicy) -> Result<()> {
        match policy {
            ConfirmationPolicy::SkipPrompt => {
                let mut stderr = io::stderr().lock();
                writeln!(stderr, "Confirmation skipped by --yes.")
                    .wrap_err("failed to write confirmation status")?;
                Ok(())
            }
            ConfirmationPolicy::RequireInteractive => {
                let mut stderr = io::stderr().lock();
                write!(
                    stderr,
                    "Type CONFIRM to sign and submit this bridge intent for {} USDC: ",
                    intent.amount
                )
                .wrap_err("failed to write confirmation prompt")?;
                stderr
                    .flush()
                    .wrap_err("failed to flush confirmation prompt")?;

                let mut input = String::new();
                io::stdin()
                    .read_line(&mut input)
                    .wrap_err("failed to read confirmation input")?;
                validate_confirmation_input(&input)
            }
        }
    }
}

fn validate_confirmation_input(input: &str) -> Result<()> {
    if input.trim() == "CONFIRM" {
        Ok(())
    } else {
        bail!("bridge intent was not confirmed")
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ReceivePolling {
    attempts: Option<u32>,
    interval_secs: Option<u64>,
}

impl ReceivePolling {
    fn new(attempts: Option<u32>, interval_secs: Option<u64>) -> Result<Self> {
        if matches!(attempts, Some(0)) {
            bail!("--receive-attempts must be greater than 0");
        }
        if matches!(interval_secs, Some(0)) {
            bail!("--receive-interval-secs must be greater than 0");
        }

        Ok(Self {
            attempts,
            interval_secs,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransferRequest {
    Standard,
    Fast { fee_cap: FastFeeCapRequest },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FastFeeCapRequest {
    Auto,
    Manual(ManualFastFeeCap),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ManualFastFeeCap {
    amount: UsdcAmount,
    source: ConfigValueSource,
}

fn transfer_request(fast: bool, max_fee_usdc: Option<&Sourced<String>>) -> Result<TransferRequest> {
    if !fast {
        if max_fee_usdc.is_some() {
            bail!("--max-fee-usdc is only valid with --fast");
        }
        return Ok(TransferRequest::Standard);
    }

    let fee_cap = match max_fee_usdc {
        Some(max_fee) => FastFeeCapRequest::Manual(ManualFastFeeCap {
            amount: UsdcAmount::parse_decimal(&max_fee.value)
                .wrap_err("failed to parse --max-fee-usdc")?,
            source: max_fee.source,
        }),
        None => FastFeeCapRequest::Auto,
    };

    Ok(TransferRequest::Fast { fee_cap })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedTransferMode {
    Standard,
    Fast(FastTransferFeeResolution),
}

impl ResolvedTransferMode {
    const fn standard() -> Self {
        Self::Standard
    }

    const fn transfer_mode(self) -> TransferMode {
        match self {
            Self::Standard => TransferMode::Standard,
            Self::Fast(fee) => TransferMode::Fast {
                max_fee: fee.max_fee,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FastTransferFeeResolution {
    live_fee: TransferFee,
    live_fee_amount: U256,
    max_fee: U256,
    cap_source: FastFeeCapSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "source", rename_all = "snake_case")]
enum FastFeeCapSource {
    LiveBuffered { buffer_percent: u32 },
    Manual,
}

fn mode_label(mode: &TransferMode) -> &'static str {
    if mode.is_fast() { "fast" } else { "standard" }
}

fn resolve_fast_transfer_fee(
    amount: UsdcAmount,
    live_fee: TransferFee,
    fee_cap: FastFeeCapRequest,
) -> Result<FastTransferFeeResolution> {
    let live_fee_amount = live_fee.max_fee_with_buffer_percent(amount.atomic(), 0);

    let (max_fee, cap_source) = match fee_cap {
        FastFeeCapRequest::Manual(manual_cap) => {
            let manual_max_fee = manual_cap.amount.atomic();
            if manual_max_fee < live_fee_amount {
                bail!(
                    "manual fast-transfer fee cap {} USDC is below the current live fast-transfer fee {} USDC",
                    UsdcAmount::from_atomic(manual_max_fee),
                    UsdcAmount::from_atomic(live_fee_amount)
                );
            }
            (manual_max_fee, FastFeeCapSource::Manual)
        }
        FastFeeCapRequest::Auto => (
            live_fee.max_fee_with_buffer_percent(amount.atomic(), DEFAULT_FAST_FEE_BUFFER_PERCENT),
            FastFeeCapSource::LiveBuffered {
                buffer_percent: DEFAULT_FAST_FEE_BUFFER_PERCENT,
            },
        ),
    };

    Ok(FastTransferFeeResolution {
        live_fee,
        live_fee_amount,
        max_fee,
        cap_source,
    })
}

#[derive(Clone, Copy, Debug, Default)]
struct CctpFeeResolutionService;

#[async_trait(?Send)]
trait FeeResolutionService<B> {
    async fn resolve(&self, bridge: &B, config: &BridgeConfig) -> Result<ResolvedTransferMode>;
}

#[async_trait(?Send)]
impl<P> FeeResolutionService<CctpV2Bridge<P>> for CctpFeeResolutionService
where
    P: Provider + Clone,
{
    async fn resolve(
        &self,
        bridge: &CctpV2Bridge<P>,
        config: &BridgeConfig,
    ) -> Result<ResolvedTransferMode> {
        match config.transfer {
            TransferRequest::Standard => Ok(ResolvedTransferMode::standard()),
            TransferRequest::Fast { fee_cap } => {
                let live_fee = bridge
                    .get_fast_transfer_fee()
                    .await
                    .wrap_err_with(|| {
                        format!(
                            "failed to fetch live fast-transfer fee for route {}",
                            config.route
                        )
                    })?
                    .ok_or_else(|| {
                        eyre!(
                            "live fast-transfer fee is unavailable for route {}; refusing to sign without fee data",
                            config.route
                        )
                    })?;
                let fee = resolve_fast_transfer_fee(config.amount, live_fee, fee_cap)?;

                Ok(ResolvedTransferMode::Fast(fee))
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletConfig {
    Trezor { account: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletRole {
    SourceBurn,
    DestinationRelay,
}

impl std::fmt::Display for WalletRole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SourceBurn => f.write_str("source burn signer"),
            Self::DestinationRelay => f.write_str("destination relay signer"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WalletDerivationPath {
    TrezorLive { account: u32 },
}

impl WalletDerivationPath {
    const fn trezor_live(account: u32) -> Self {
        Self::TrezorLive { account }
    }
}

impl std::fmt::Display for WalletDerivationPath {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TrezorLive { account } => write!(f, "m/44'/60'/{account}'/0/0"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct WalletAccount {
    role: WalletRole,
    wallet: WalletConfig,
    derivation_path: WalletDerivationPath,
    chain: ChainArg,
    address: Address,
}

impl WalletAccount {
    fn chain_id(&self) -> u64 {
        self.chain.chain_id()
    }

    const fn chain_label(&self) -> &'static str {
        self.chain.display_label()
    }
}

impl WalletConfig {
    const fn from_kind(kind: WalletKind, trezor_account: u32) -> Self {
        match kind {
            WalletKind::Trezor => Self::Trezor {
                account: trezor_account,
            },
        }
    }

    fn validate(self) -> Result<()> {
        self.trezor_account_index().map(|_| ())
    }

    fn account_info(self, role: WalletRole, chain: ChainArg, address: Address) -> WalletAccount {
        WalletAccount {
            role,
            wallet: self,
            derivation_path: self.derivation_path(),
            chain,
            address,
        }
    }

    fn trezor_account_index(self) -> Result<usize> {
        match self {
            Self::Trezor { account } => {
                usize::try_from(account).wrap_err("Trezor account index is too large")
            }
        }
    }

    const fn derivation_path(self) -> WalletDerivationPath {
        match self {
            Self::Trezor { account } => WalletDerivationPath::trezor_live(account),
        }
    }

    async fn trezor_signer(self, chain_id: u64) -> Result<TrezorSigner> {
        match self {
            Self::Trezor { .. } => {
                let account_index = self.trezor_account_index()?;
                TrezorSigner::new(HDPath::TrezorLive(account_index), Some(chain_id))
                    .await
                    .map_err(Into::into)
            }
        }
    }
}

impl std::fmt::Display for WalletConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Trezor { account } => write!(f, "trezor account {account}"),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayConfig {
    WaitForRelayer { fallback: RelayFallbackConfig },
    SelfRelay { wallet: WalletConfig },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayFallbackConfig {
    SelfRelay { wallet: WalletConfig },
}

impl RelayConfig {
    const fn from_mode(
        mode: RelayMode,
        kind: WalletKind,
        relay_trezor_account: Option<u32>,
        source_trezor_account: u32,
    ) -> Self {
        let wallet = relay_wallet_config(kind, relay_trezor_account, source_trezor_account);
        match mode {
            RelayMode::WaitForRelayer => Self::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay { wallet },
            },
            RelayMode::SelfRelay => Self::SelfRelay { wallet },
        }
    }

    const fn wallet(self) -> WalletConfig {
        match self {
            Self::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay { wallet },
            }
            | Self::SelfRelay { wallet } => wallet,
        }
    }

    fn validate(self) -> Result<()> {
        self.wallet().validate()
    }
}

const fn relay_wallet_config(
    kind: WalletKind,
    relay_trezor_account: Option<u32>,
    source_trezor_account: u32,
) -> WalletConfig {
    match kind {
        WalletKind::Trezor => WalletConfig::Trezor {
            account: match relay_trezor_account {
                Some(account) => account,
                None => source_trezor_account,
            },
        },
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedRelayFallback {
    SelfRelay { account: WalletAccount },
}

impl ResolvedRelayFallback {
    const fn account(self) -> WalletAccount {
        match self {
            Self::SelfRelay { account } => account,
        }
    }

    const fn workflow(self) -> WorkflowRelayFallback {
        match self {
            Self::SelfRelay { account } => WorkflowRelayFallback::SelfRelay {
                submitter: account.address,
            },
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkflowRelayFallback {
    SelfRelay { submitter: Address },
}

impl WorkflowRelayFallback {
    const fn submitter(self) -> Address {
        match self {
            Self::SelfRelay { submitter } => submitter,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RelayPolicyLabel {
    WaitThenSelfRelay,
    SelfRelay,
}

impl std::fmt::Display for RelayPolicyLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::WaitThenSelfRelay => {
                f.write_str("wait for any permissionless relayer, then self-relay fallback")
            }
            Self::SelfRelay => f.write_str("self-relay on destination chain"),
        }
    }
}

struct RelaySignerRuntime<S> {
    signer: S,
    account: WalletAccount,
}

struct SourceSignerRuntime<S> {
    signer: S,
    account: WalletAccount,
}

#[derive(Clone, Copy, Debug, Default)]
struct TrezorWalletService;

#[async_trait(?Send)]
trait WalletService {
    type SourceSigner;
    type RelaySigner;

    async fn source_signer(
        &self,
        config: &BridgeConfig,
    ) -> Result<SourceSignerRuntime<Self::SourceSigner>>;

    async fn relay_signer(
        &self,
        config: &BridgeConfig,
    ) -> Result<RelaySignerRuntime<Self::RelaySigner>>;
}

#[async_trait(?Send)]
impl WalletService for TrezorWalletService {
    type SourceSigner = TrezorSigner;
    type RelaySigner = TrezorSigner;

    async fn source_signer(
        &self,
        config: &BridgeConfig,
    ) -> Result<SourceSignerRuntime<TrezorSigner>> {
        let signer = config
            .source_wallet
            .trezor_signer(config.route.source_chain_id())
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to initialize Trezor signer for {}",
                    config.route.source_label()
                )
            })?;
        let address = signer.get_address().await.wrap_err_with(|| {
            format!(
                "failed to read {} address from Trezor",
                config.route.source_label()
            )
        })?;
        let account =
            config
                .source_wallet
                .account_info(WalletRole::SourceBurn, config.route.from(), address);

        Ok(SourceSignerRuntime { signer, account })
    }

    async fn relay_signer(
        &self,
        config: &BridgeConfig,
    ) -> Result<RelaySignerRuntime<TrezorSigner>> {
        let wallet = config.relay.wallet();

        let signer = wallet
            .trezor_signer(config.route.destination_chain_id())
            .await
            .wrap_err_with(|| {
                format!(
                    "failed to initialize Trezor signer for {} self-relay",
                    config.route.destination_label()
                )
            })?;
        let address = signer.get_address().await.wrap_err_with(|| {
            format!(
                "failed to read {} relay address from Trezor",
                config.route.destination_label()
            )
        })?;
        let account = wallet.account_info(WalletRole::DestinationRelay, config.route.to(), address);

        Ok(RelaySignerRuntime { signer, account })
    }
}

#[derive(Clone, Debug)]
struct BridgeProviders {
    source: DynProvider,
    destination: DynProvider,
}

#[derive(Clone, Copy, Debug, Default)]
struct AlloyProviderService;

trait ProviderService<W>
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
trait ProviderValidationService<P> {
    async fn validate(&self, config: &BridgeConfig, providers: &P) -> Result<ProviderValidation>;
}

#[derive(Clone, Copy, Debug, Default)]
struct AlloyProviderValidationService;

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
struct ProviderValidation {
    source: ProviderChainCheck,
    destination: ProviderChainCheck,
}

impl ProviderValidation {
    fn new(
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
struct ProviderChainCheck {
    role: ProviderEndpointRole,
    expected: ExpectedProviderChain,
    actual_chain_id: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ExpectedProviderChain {
    chain: ChainArg,
}

impl ExpectedProviderChain {
    const fn new(chain: ChainArg) -> Self {
        Self { chain }
    }

    const fn display_label(&self) -> &'static str {
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
enum ProviderEndpointRole {
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

    const fn report_label(self) -> &'static str {
        match self {
            Self::Source => "Source RPC",
            Self::Destination => "Destination RPC",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BridgeContracts {
    token_messenger: Address,
    message_transmitter: Address,
    destination_domain: DomainId,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResolvedRelay {
    WaitForRelayer { fallback: ResolvedRelayFallback },
    SelfRelay { account: WalletAccount },
}

impl ResolvedRelay {
    const fn from_config(config: RelayConfig, account: WalletAccount) -> Self {
        match config {
            RelayConfig::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay { .. },
            } => Self::WaitForRelayer {
                fallback: ResolvedRelayFallback::SelfRelay { account },
            },
            RelayConfig::SelfRelay { .. } => Self::SelfRelay { account },
        }
    }

    const fn mode(self) -> RelayMode {
        match self {
            Self::WaitForRelayer { .. } => RelayMode::WaitForRelayer,
            Self::SelfRelay { .. } => RelayMode::SelfRelay,
        }
    }

    const fn label(self) -> RelayPolicyLabel {
        match self {
            Self::WaitForRelayer { .. } => RelayPolicyLabel::WaitThenSelfRelay,
            Self::SelfRelay { .. } => RelayPolicyLabel::SelfRelay,
        }
    }

    const fn account(self) -> WalletAccount {
        match self {
            Self::WaitForRelayer { fallback } => fallback.account(),
            Self::SelfRelay { account } => account,
        }
    }

    const fn workflow_relay(self) -> WorkflowRelay {
        match self {
            Self::WaitForRelayer { fallback } => WorkflowRelay::WaitForRelayer {
                fallback: fallback.workflow(),
            },
            Self::SelfRelay { account } => WorkflowRelay::SelfRelay {
                submitter: account.address,
            },
        }
    }
}

#[derive(Clone, Debug)]
struct BridgeIntent {
    route: RouteConfig,
    source_account: WalletAccount,
    recipient: Address,
    usdc: Address,
    amount: UsdcAmount,
    transfer: ResolvedTransferMode,
    relay: ResolvedRelay,
    provider_validation: ProviderValidation,
    contracts: BridgeContracts,
    provenance: BridgeConfigProvenance,
}

impl BridgeIntent {
    fn new(
        config: &BridgeConfig,
        source_account: WalletAccount,
        recipient: Address,
        relay: ResolvedRelay,
        provider_validation: ProviderValidation,
        contracts: BridgeContracts,
        transfer: ResolvedTransferMode,
    ) -> Self {
        Self {
            route: config.route,
            source_account,
            recipient,
            usdc: config.usdc,
            amount: config.amount,
            transfer,
            relay,
            provider_validation,
            contracts,
            provenance: config.provenance.clone(),
        }
    }
}

#[derive(Clone, Debug)]
enum ConfiguredReporter {
    Human(HumanReporter),
    Json(JsonReporter),
}

impl ConfiguredReporter {
    fn from_output_mode(output: OutputMode) -> Self {
        match output {
            OutputMode::Human => Self::Human(HumanReporter),
            OutputMode::Json => Self::Json(JsonReporter::stdout()),
        }
    }
}

impl Reporter for ConfiguredReporter {
    fn report_intent(&self, intent: &BridgeIntent) -> Result<()> {
        match self {
            Self::Human(reporter) => Reporter::report_intent(reporter, intent),
            Self::Json(reporter) => reporter.report_intent(intent),
        }
    }

    fn report_dry_run_complete(&self) -> Result<()> {
        match self {
            Self::Human(reporter) => Reporter::report_dry_run_complete(reporter),
            Self::Json(reporter) => reporter.report_dry_run_complete(),
        }
    }

    fn report_workflow_start(&self) -> Result<()> {
        match self {
            Self::Human(reporter) => Reporter::report_workflow_start(reporter),
            Self::Json(reporter) => reporter.report_workflow_start(),
        }
    }

    fn report_outcome(&self, outcome: &BridgeOutcome) -> Result<()> {
        match self {
            Self::Human(reporter) => Reporter::report_outcome(reporter, outcome),
            Self::Json(reporter) => reporter.report_outcome(outcome),
        }
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct HumanReporter;

trait Reporter {
    fn report_intent(&self, intent: &BridgeIntent) -> Result<()>;

    fn report_dry_run_complete(&self) -> Result<()>;

    fn report_workflow_start(&self) -> Result<()>;

    fn report_outcome(&self, outcome: &BridgeOutcome) -> Result<()>;
}

impl Reporter for HumanReporter {
    fn report_intent(&self, intent: &BridgeIntent) -> Result<()> {
        HumanReporter::report_intent(self, intent);
        Ok(())
    }

    fn report_dry_run_complete(&self) -> Result<()> {
        HumanReporter::report_dry_run_complete(self);
        Ok(())
    }

    fn report_workflow_start(&self) -> Result<()> {
        HumanReporter::report_workflow_start(self);
        Ok(())
    }

    fn report_outcome(&self, outcome: &BridgeOutcome) -> Result<()> {
        HumanReporter::report_outcome(self, outcome);
        Ok(())
    }
}

impl HumanReporter {
    fn report_intent(&self, intent: &BridgeIntent) {
        println!("Bridge intent");
        println!("Route: {} ({})", intent.route, intent.provenance.route);
        self.report_provider_check(
            &intent.provider_validation.source,
            &intent.provenance.rpc.source,
        );
        self.report_provider_check(
            &intent.provider_validation.destination,
            &intent.provenance.rpc.destination,
        );
        self.report_wallet_account("Source", &intent.source_account);
        println!(
            "Source wallet source: wallet {}, account {}",
            intent.provenance.source_wallet.wallet, intent.provenance.source_wallet.account
        );
        println!(
            "Recipient: {} ({})",
            intent.recipient, intent.provenance.recipient
        );
        println!("USDC: {}", intent.usdc);
        println!(
            "Amount: {} USDC ({})",
            intent.amount, intent.provenance.amount
        );
        self.report_transfer_mode(&intent.transfer, intent.provenance.fast_mode);
        println!("Fee cap source: {}", intent.provenance.max_fee);
        println!(
            "Relay: {} ({})",
            intent.relay.label(),
            intent.provenance.relay_mode
        );
        match intent.relay {
            ResolvedRelay::SelfRelay { account } => {
                self.report_wallet_account("Relay", &account);
                println!("Relay wallet source: {}", intent.provenance.relay_wallet);
            }
            ResolvedRelay::WaitForRelayer { fallback } => {
                println!("Relay fallback: self-relay if the relayer wait expires");
                self.report_wallet_account("Relay fallback", &fallback.account());
                println!("Relay wallet source: {}", intent.provenance.relay_wallet);
            }
        }
        println!(
            "TokenMessengerV2 approval spender: {}",
            intent.contracts.token_messenger
        );
        println!(
            "MessageTransmitterV2 destination contract: {}",
            intent.contracts.message_transmitter
        );
        println!(
            "Destination domain: {}",
            intent.contracts.destination_domain
        );
    }

    fn report_provider_check(&self, check: &ProviderChainCheck, endpoint: &RpcEndpointProvenance) {
        println!(
            "{} verified: {} (chain id {}, endpoint {}, {})",
            check.role.report_label(),
            check.expected.display_label(),
            check.actual_chain_id,
            endpoint.redacted_endpoint,
            endpoint.source
        );
    }

    fn report_wallet_account(&self, label: &str, account: &WalletAccount) {
        println!("{label} role: {}", account.role);
        println!("{label} wallet: {}", account.wallet);
        println!("{label} derivation: {}", account.derivation_path);
        println!(
            "{label} chain: {} (chain id {})",
            account.chain_label(),
            account.chain_id()
        );
        println!("{label} address: {}", account.address);
    }

    fn report_transfer_mode(
        &self,
        transfer: &ResolvedTransferMode,
        fast_source: ConfigValueSource,
    ) {
        let mode = transfer.transfer_mode();
        println!("Mode: {} (fast mode {})", mode_label(&mode), fast_source);
        if let ResolvedTransferMode::Fast(fee) = *transfer {
            println!(
                "Fast live fee: {} bps ({} USDC for this amount)",
                fee.live_fee.minimum_fee,
                UsdcAmount::from_atomic(fee.live_fee_amount)
            );
            match fee.cap_source {
                FastFeeCapSource::LiveBuffered { buffer_percent } => {
                    println!(
                        "Fast fee cap: {} USDC (live fee + {buffer_percent}% buffer)",
                        UsdcAmount::from_atomic(fee.max_fee)
                    );
                }
                FastFeeCapSource::Manual => {
                    println!(
                        "Fast fee cap: {} USDC (manual cap)",
                        UsdcAmount::from_atomic(fee.max_fee)
                    );
                }
            }
        }
    }

    fn report_dry_run_complete(&self) {
        println!("Dry run complete. No transactions sent.");
    }

    fn report_workflow_start(&self) {
        println!("Starting bridge workflow.");
    }

    fn report_outcome(&self, outcome: &BridgeOutcome) {
        println!("Source sender: {}", outcome.source_sender);
        println!("Recipient: {}", outcome.recipient);
        println!("TokenMessengerV2: {}", outcome.token_messenger);
        println!("Destination domain: {}", outcome.destination_domain);
        match outcome.approval {
            ApprovalOutcome::Skipped { allowance } => {
                println!("Existing USDC allowance is sufficient: {allowance} atomic units.");
            }
            ApprovalOutcome::Sent { tx_hash } => {
                println!("Approval tx: {tx_hash}");
            }
        }
        println!("Burn tx: {}", outcome.burn_tx);
        println!(
            "Attestation ready. Canonical message bytes: {}",
            outcome.attestation.message_len
        );
        match outcome.completion {
            CompletionOutcome::RelayerCompleted => {
                println!("Transfer completed by a permissionless relayer.");
            }
            CompletionOutcome::SelfRelayMinted { tx_hash } => {
                println!("Mint tx: {tx_hash}");
            }
            CompletionOutcome::SelfRelayAlreadyCompleted => {
                println!("Transfer was already completed by a relayer.");
            }
            CompletionOutcome::FallbackSelfRelayMinted { tx_hash } => {
                println!("Relayer wait expired; fallback mint tx: {tx_hash}");
            }
            CompletionOutcome::FallbackSelfRelayAlreadyCompleted => {
                println!(
                    "Relayer wait expired; transfer was already completed before fallback mint."
                );
            }
        }
        println!("Transfer complete.");
    }
}

#[derive(Clone, Debug)]
struct JsonReporter<S = StdoutJsonReportSink> {
    sink: S,
}

impl JsonReporter<StdoutJsonReportSink> {
    const fn stdout() -> Self {
        Self {
            sink: StdoutJsonReportSink,
        }
    }
}

#[cfg(test)]
impl<S> JsonReporter<S> {
    const fn new(sink: S) -> Self {
        Self { sink }
    }
}

impl<S> Reporter for JsonReporter<S>
where
    S: JsonReportSink,
{
    fn report_intent(&self, intent: &BridgeIntent) -> Result<()> {
        self.report_event(JsonReportEvent::BridgeIntent(Box::new(json_bridge_intent(
            intent,
        ))))
    }

    fn report_dry_run_complete(&self) -> Result<()> {
        self.report_event(JsonReportEvent::DryRunComplete(JsonDryRunComplete {
            run_mode: BridgeRunMode::DryRun,
        }))
    }

    fn report_workflow_start(&self) -> Result<()> {
        self.report_event(JsonReportEvent::WorkflowStart(JsonWorkflowStart {
            status: JsonWorkflowStatus::Started,
        }))
    }

    fn report_outcome(&self, outcome: &BridgeOutcome) -> Result<()> {
        self.report_event(JsonReportEvent::BridgeOutcome(Box::new(
            json_bridge_outcome(outcome),
        )))
    }
}

impl<S> JsonReporter<S>
where
    S: JsonReportSink,
{
    fn report_event(&self, event: JsonReportEvent) -> Result<()> {
        self.sink.write_json(&event)
    }
}

trait JsonReportSink {
    fn write_json(&self, event: &JsonReportEvent) -> Result<()>;
}

#[derive(Clone, Copy, Debug, Default)]
struct StdoutJsonReportSink;

impl JsonReportSink for StdoutJsonReportSink {
    fn write_json(&self, event: &JsonReportEvent) -> Result<()> {
        let mut stdout = io::stdout().lock();
        serde_json::to_writer(&mut stdout, event).wrap_err("failed to write JSON report")?;
        writeln!(stdout).wrap_err("failed to finish JSON report line")
    }
}

#[derive(Serialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
enum JsonReportEvent {
    BridgeIntent(Box<JsonBridgeIntent>),
    DryRunComplete(JsonDryRunComplete),
    WorkflowStart(JsonWorkflowStart),
    BridgeOutcome(Box<JsonBridgeOutcome>),
}

#[derive(Serialize)]
struct JsonBridgeIntent {
    route: JsonRoute,
    signer: JsonWalletAccount,
    relay_signer: JsonWalletAccount,
    recipient: Address,
    usdc: Address,
    amount: JsonAmount,
    mode: JsonTransferMode,
    relay_policy: JsonRelayPolicy,
    provider_checks: JsonProviderChecks,
    contracts: JsonContracts,
    provenance: JsonIntentProvenance,
}

struct JsonRoute(RouteConfig);

impl Serialize for JsonRoute {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let route = self.0;
        let mut state = serializer.serialize_struct("JsonRoute", 3)?;
        state.serialize_field("label", &route.to_string())?;
        state.serialize_field(
            "source",
            &JsonRouteChain {
                chain: route.from(),
            },
        )?;
        state.serialize_field("destination", &JsonRouteChain { chain: route.to() })?;
        state.end()
    }
}

struct JsonRouteChain {
    chain: ChainArg,
}

impl Serialize for JsonRouteChain {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let named_chain = self.chain.named_chain();
        let mut state = serializer.serialize_struct("JsonRouteChain", 4)?;
        state.serialize_field("cli", &self.chain)?;
        state.serialize_field("chain", &named_chain)?;
        state.serialize_field("label", &self.chain.display_label())?;
        state.serialize_field("chain_id", &self.chain.chain_id())?;
        state.end()
    }
}

struct JsonWalletAccount(WalletAccount);

impl Serialize for JsonWalletAccount {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let account = self.0;
        let mut state = serializer.serialize_struct("JsonWalletAccount", 5)?;
        state.serialize_field("role", &JsonDisplay(account.role))?;
        state.serialize_field("wallet", &JsonDisplay(account.wallet))?;
        state.serialize_field("derivation_path", &JsonDisplay(account.derivation_path))?;
        state.serialize_field(
            "chain",
            &JsonChainIdentity {
                chain: account.chain,
            },
        )?;
        state.serialize_field("address", &account.address)?;
        state.end()
    }
}

struct JsonChainIdentity {
    chain: ChainArg,
}

impl Serialize for JsonChainIdentity {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("JsonChainIdentity", 3)?;
        state.serialize_field("chain", &self.chain.named_chain())?;
        state.serialize_field("label", &self.chain.display_label())?;
        state.serialize_field("chain_id", &self.chain.chain_id())?;
        state.end()
    }
}

#[derive(Serialize)]
struct JsonAmount {
    #[serde(serialize_with = "serialize_display")]
    usdc: UsdcAmount,
    #[serde(serialize_with = "serialize_display")]
    atomic: U256,
}

#[derive(Serialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
enum JsonTransferMode {
    Standard,
    Fast { fast_fee: JsonFastFee },
}

#[derive(Serialize)]
struct JsonFastFee {
    live_fee_bps: FeeBps,
    live_fee_amount: JsonAmount,
    max_fee: JsonAmount,
    cap_source: FastFeeCapSource,
}

#[derive(Serialize)]
struct JsonRelayPolicy {
    mode: RelayMode,
    #[serde(serialize_with = "serialize_display")]
    policy: RelayPolicyLabel,
    #[serde(serialize_with = "serialize_display")]
    source: ConfigValueSource,
    destination_provider: JsonDestinationProvider,
    fallback: Option<JsonRelayFallbackPolicy>,
    #[serde(serialize_with = "serialize_display")]
    relay_wallet: RelayWalletProvenance,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonDestinationProvider {
    SelfRelaySigner,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonRelayFallbackPolicy {
    SelfRelaySigner,
}

#[derive(Serialize)]
struct JsonProviderChecks {
    source: JsonProviderCheck,
    destination: JsonProviderCheck,
}

#[derive(Serialize)]
struct JsonProviderCheck {
    role: &'static str,
    expected: JsonChainIdentity,
    actual_chain_id: u64,
    endpoint: JsonRpcEndpoint,
}

#[derive(Serialize)]
struct JsonRpcEndpoint {
    redacted: String,
    #[serde(serialize_with = "serialize_display")]
    source: ConfigValueSource,
}

#[derive(Serialize)]
struct JsonContracts {
    token_messenger: Address,
    message_transmitter: Address,
    destination_domain: JsonCctpDomain,
}

struct JsonCctpDomain(DomainId);

impl Serialize for JsonCctpDomain {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut state = serializer.serialize_struct("JsonCctpDomain", 2)?;
        state.serialize_field("domain", &self.0)?;
        state.serialize_field("domain_id", &self.0.as_u32())?;
        state.end()
    }
}

#[derive(Serialize)]
struct JsonIntentProvenance {
    #[serde(serialize_with = "serialize_display")]
    route: RouteConfigProvenance,
    #[serde(serialize_with = "serialize_display")]
    amount: ConfigValueSource,
    #[serde(serialize_with = "serialize_display")]
    recipient: RecipientProvenance,
    source_wallet: JsonSourceWalletProvenance,
    #[serde(serialize_with = "serialize_display")]
    relay_wallet: RelayWalletProvenance,
    #[serde(serialize_with = "serialize_display")]
    relay_mode: ConfigValueSource,
    #[serde(serialize_with = "serialize_display")]
    fast_mode: ConfigValueSource,
    #[serde(serialize_with = "serialize_display")]
    max_fee: MaxFeeProvenance,
    #[serde(serialize_with = "serialize_display")]
    output: ConfigValueSource,
}

#[derive(Serialize)]
struct JsonSourceWalletProvenance {
    #[serde(serialize_with = "serialize_display")]
    wallet: ConfigValueSource,
    #[serde(serialize_with = "serialize_display")]
    account: ConfigValueSource,
}

#[derive(Serialize)]
struct JsonDryRunComplete {
    run_mode: BridgeRunMode,
}

#[derive(Serialize)]
struct JsonWorkflowStart {
    status: JsonWorkflowStatus,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonWorkflowStatus {
    Started,
}

#[derive(Serialize)]
struct JsonBridgeOutcome {
    status: JsonBridgeStatus,
    source_sender: Address,
    recipient: Address,
    token_messenger: Address,
    destination_domain: JsonCctpDomain,
    approval: JsonApprovalOutcome,
    burn: JsonTransactionStatus,
    attestation: JsonAttestationOutcome,
    completion: JsonCompletionOutcome,
    transactions: JsonTransactions,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonBridgeStatus {
    Complete,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum JsonApprovalOutcome {
    SkippedExistingAllowance {
        #[serde(serialize_with = "serialize_display")]
        allowance_atomic: U256,
    },
    Confirmed {
        tx_hash: TxHash,
    },
}

#[derive(Serialize)]
struct JsonTransactionStatus {
    status: JsonTransactionStatusKind,
    tx_hash: TxHash,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonTransactionStatusKind {
    Confirmed,
}

#[derive(Serialize)]
struct JsonAttestationOutcome {
    status: JsonAttestationStatus,
    canonical_message_bytes: usize,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum JsonAttestationStatus {
    Ready,
}

#[derive(Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum JsonCompletionOutcome {
    RelayerCompleted,
    SelfRelayMinted { tx_hash: TxHash },
    SelfRelayAlreadyCompleted,
    FallbackSelfRelayMinted { tx_hash: TxHash },
    FallbackSelfRelayAlreadyCompleted,
}

#[derive(Serialize)]
struct JsonTransactions {
    approval: Option<TxHash>,
    burn: TxHash,
    mint: Option<TxHash>,
}

fn serialize_display<T, S>(value: &T, serializer: S) -> Result<S::Ok, S::Error>
where
    T: std::fmt::Display,
    S: Serializer,
{
    serializer.serialize_str(&value.to_string())
}

struct JsonDisplay<T>(T);

impl<T> Serialize for JsonDisplay<T>
where
    T: std::fmt::Display,
{
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

fn json_bridge_intent(intent: &BridgeIntent) -> JsonBridgeIntent {
    let relay_account = intent.relay.account();
    JsonBridgeIntent {
        route: JsonRoute(intent.route),
        signer: json_wallet_account(&intent.source_account),
        relay_signer: json_wallet_account(&relay_account),
        recipient: intent.recipient,
        usdc: intent.usdc,
        amount: json_usdc_amount(intent.amount),
        mode: json_transfer_mode(&intent.transfer),
        relay_policy: JsonRelayPolicy {
            mode: intent.relay.mode(),
            policy: intent.relay.label(),
            source: intent.provenance.relay_mode,
            destination_provider: JsonDestinationProvider::SelfRelaySigner,
            fallback: match intent.relay {
                ResolvedRelay::WaitForRelayer { .. } => {
                    Some(JsonRelayFallbackPolicy::SelfRelaySigner)
                }
                ResolvedRelay::SelfRelay { .. } => None,
            },
            relay_wallet: intent.provenance.relay_wallet,
        },
        provider_checks: JsonProviderChecks {
            source: json_provider_check(
                &intent.provider_validation.source,
                &intent.provenance.rpc.source,
            ),
            destination: json_provider_check(
                &intent.provider_validation.destination,
                &intent.provenance.rpc.destination,
            ),
        },
        contracts: JsonContracts {
            token_messenger: intent.contracts.token_messenger,
            message_transmitter: intent.contracts.message_transmitter,
            destination_domain: json_cctp_domain(intent.contracts.destination_domain),
        },
        provenance: JsonIntentProvenance {
            route: intent.provenance.route,
            amount: intent.provenance.amount,
            recipient: intent.provenance.recipient,
            source_wallet: JsonSourceWalletProvenance {
                wallet: intent.provenance.source_wallet.wallet,
                account: intent.provenance.source_wallet.account,
            },
            relay_wallet: intent.provenance.relay_wallet,
            relay_mode: intent.provenance.relay_mode,
            fast_mode: intent.provenance.fast_mode,
            max_fee: intent.provenance.max_fee,
            output: intent.provenance.output,
        },
    }
}

fn json_wallet_account(account: &WalletAccount) -> JsonWalletAccount {
    JsonWalletAccount(*account)
}

fn json_provider_check(
    check: &ProviderChainCheck,
    endpoint: &RpcEndpointProvenance,
) -> JsonProviderCheck {
    JsonProviderCheck {
        role: check.role.report_label(),
        expected: JsonChainIdentity {
            chain: check.expected.chain,
        },
        actual_chain_id: check.actual_chain_id,
        endpoint: JsonRpcEndpoint {
            redacted: endpoint.redacted_endpoint.clone(),
            source: endpoint.source,
        },
    }
}

fn json_transfer_mode(transfer: &ResolvedTransferMode) -> JsonTransferMode {
    match *transfer {
        ResolvedTransferMode::Standard => JsonTransferMode::Standard,
        ResolvedTransferMode::Fast(fee) => JsonTransferMode::Fast {
            fast_fee: JsonFastFee {
                live_fee_bps: fee.live_fee.minimum_fee,
                live_fee_amount: json_atomic_usdc_amount(fee.live_fee_amount),
                max_fee: json_atomic_usdc_amount(fee.max_fee),
                cap_source: fee.cap_source,
            },
        },
    }
}

fn json_cctp_domain(domain: DomainId) -> JsonCctpDomain {
    JsonCctpDomain(domain)
}

fn json_usdc_amount(amount: UsdcAmount) -> JsonAmount {
    JsonAmount {
        usdc: amount,
        atomic: amount.atomic(),
    }
}

fn json_atomic_usdc_amount(atomic: U256) -> JsonAmount {
    json_usdc_amount(UsdcAmount::from_atomic(atomic))
}

fn json_bridge_outcome(outcome: &BridgeOutcome) -> JsonBridgeOutcome {
    let approval_tx = match outcome.approval {
        ApprovalOutcome::Skipped { .. } => None,
        ApprovalOutcome::Sent { tx_hash } => Some(tx_hash),
    };
    let mint_tx = match outcome.completion {
        CompletionOutcome::RelayerCompleted
        | CompletionOutcome::SelfRelayAlreadyCompleted
        | CompletionOutcome::FallbackSelfRelayAlreadyCompleted => None,
        CompletionOutcome::SelfRelayMinted { tx_hash }
        | CompletionOutcome::FallbackSelfRelayMinted { tx_hash } => Some(tx_hash),
    };

    JsonBridgeOutcome {
        status: JsonBridgeStatus::Complete,
        source_sender: outcome.source_sender,
        recipient: outcome.recipient,
        token_messenger: outcome.token_messenger,
        destination_domain: json_cctp_domain(outcome.destination_domain),
        approval: match outcome.approval {
            ApprovalOutcome::Skipped { allowance } => {
                JsonApprovalOutcome::SkippedExistingAllowance {
                    allowance_atomic: allowance,
                }
            }
            ApprovalOutcome::Sent { tx_hash } => JsonApprovalOutcome::Confirmed { tx_hash },
        },
        burn: JsonTransactionStatus {
            status: JsonTransactionStatusKind::Confirmed,
            tx_hash: outcome.burn_tx,
        },
        attestation: JsonAttestationOutcome {
            status: JsonAttestationStatus::Ready,
            canonical_message_bytes: outcome.attestation.message_len,
        },
        completion: match outcome.completion {
            CompletionOutcome::RelayerCompleted => JsonCompletionOutcome::RelayerCompleted,
            CompletionOutcome::SelfRelayMinted { tx_hash } => {
                JsonCompletionOutcome::SelfRelayMinted { tx_hash }
            }
            CompletionOutcome::SelfRelayAlreadyCompleted => {
                JsonCompletionOutcome::SelfRelayAlreadyCompleted
            }
            CompletionOutcome::FallbackSelfRelayMinted { tx_hash } => {
                JsonCompletionOutcome::FallbackSelfRelayMinted { tx_hash }
            }
            CompletionOutcome::FallbackSelfRelayAlreadyCompleted => {
                JsonCompletionOutcome::FallbackSelfRelayAlreadyCompleted
            }
        },
        transactions: JsonTransactions {
            approval: approval_tx,
            burn: outcome.burn_tx,
            mint: mint_tx,
        },
    }
}

async fn wait_for_receipt<P>(
    provider: &P,
    tx_hash: TxHash,
    label: &str,
    max_attempts: u32,
    interval: Duration,
) -> Result<()>
where
    P: Provider,
{
    for _ in 1..=max_attempts {
        let receipt = provider
            .get_transaction_receipt(tx_hash)
            .await
            .wrap_err_with(|| format!("failed to poll {label} transaction receipt"))?;

        if receipt.is_some() {
            return Ok(());
        }

        sleep(interval).await;
    }

    bail!("{label} transaction {tx_hash} was not confirmed before timeout")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        cell::RefCell,
        collections::HashMap,
        rc::Rc,
        sync::atomic::{AtomicU64, Ordering},
    };

    static CONFIG_COUNTER: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone, Debug, Default)]
    struct TestEnv(HashMap<String, String>);

    impl EnvSource for TestEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.0.get(key).cloned()
        }
    }

    fn config_service(env: &[(&str, &str)]) -> CliConfigService<TestEnv> {
        CliConfigService::new(TestEnv(
            env.iter()
                .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                .collect(),
        ))
    }

    fn empty_service() -> CliConfigService<TestEnv> {
        config_service(&[])
    }

    fn write_config(contents: &str) -> PathBuf {
        let count = CONFIG_COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cctp-config-{}-{count}.toml", std::process::id()));
        std::fs::write(&path, contents).expect("write config");
        path
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

    fn sample_args() -> BridgeArgs {
        BridgeArgs {
            from: Some(ChainArg::Ethereum),
            to: Some(ChainArg::HyperEvm),
            amount: Some("1.25".to_owned()),
            ethereum_rpc: Some("https://ethereum.example".to_owned()),
            hyperevm_rpc: Some("https://hyperevm.example".to_owned()),
            wallet: Some(WalletKind::Trezor),
            trezor_account: Some(0),
            ..empty_args()
        }
    }

    fn testnet_args() -> BridgeArgs {
        BridgeArgs {
            from: Some(ChainArg::EthereumSepolia),
            to: Some(ChainArg::BaseSepolia),
            amount: Some("1.25".to_owned()),
            ethereum_sepolia_rpc: Some("https://ethereum-sepolia.example".to_owned()),
            base_sepolia_rpc: Some("https://base-sepolia.example".to_owned()),
            wallet: Some(WalletKind::Trezor),
            trezor_account: Some(0),
            ..empty_args()
        }
    }

    #[test]
    fn config_service_builds_bridge_config() {
        let config = empty_service()
            .bridge_config(sample_args())
            .expect("valid config");

        assert_eq!(
            config.route.cctp_route().source_chain(),
            NamedChain::Mainnet
        );
        assert_eq!(
            config.route.cctp_route().destination_chain(),
            NamedChain::Hyperliquid
        );
        assert_eq!(config.amount.atomic(), U256::from(1_250_000u64));
        assert_eq!(config.recipient, RecipientConfig::Signer);
        assert_eq!(config.source_wallet, WalletConfig::Trezor { account: 0 });
        assert_eq!(
            config.relay,
            RelayConfig::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay {
                    wallet: WalletConfig::Trezor { account: 0 }
                }
            }
        );
        assert_eq!(config.rpc.source.as_str(), "https://ethereum.example/");
        assert_eq!(config.rpc.destination.as_str(), "https://hyperevm.example/");
        assert_eq!(config.transfer, TransferRequest::Standard);
        assert_eq!(config.confirmation, ConfirmationPolicy::RequireInteractive);
        assert_eq!(config.output, OutputMode::Human);
        assert_eq!(
            config.provenance.route,
            RouteConfigProvenance {
                from: ConfigValueSource::CliFlag("--from"),
                to: ConfigValueSource::CliFlag("--to")
            }
        );
        assert_eq!(
            config.provenance.recipient,
            RecipientProvenance::DefaultedToSourceSigner
        );
        assert_eq!(
            config.provenance.amount,
            ConfigValueSource::CliFlag("--amount")
        );
        assert_eq!(
            config.provenance.rpc.source.source,
            ConfigValueSource::CliFlag("--ethereum-rpc")
        );
        assert_eq!(
            config.provenance.rpc.destination.source,
            ConfigValueSource::CliFlag("--hyperevm-rpc")
        );
        assert_eq!(
            config.provenance.source_wallet,
            SourceWalletProvenance {
                wallet: ConfigValueSource::CliFlag("--wallet"),
                account: ConfigValueSource::CliFlag("--trezor-account")
            }
        );
        assert_eq!(
            config.provenance.relay_wallet,
            RelayWalletProvenance::DefaultedToSourceAccount
        );
        assert_eq!(
            config.provenance.fast_mode,
            ConfigValueSource::Default("standard finality")
        );
        assert_eq!(config.provenance.max_fee, MaxFeeProvenance::NotApplicable);
        assert_eq!(
            config.provenance.output,
            ConfigValueSource::Default("human")
        );
    }

    #[test]
    fn config_service_builds_explicit_testnet_bridge_config() {
        let config = empty_service()
            .bridge_config(testnet_args())
            .expect("valid testnet config");

        assert_eq!(
            config.route.cctp_route().source_chain(),
            NamedChain::Sepolia
        );
        assert_eq!(
            config.route.cctp_route().destination_chain(),
            NamedChain::BaseSepolia
        );
        assert_eq!(config.route.source_label(), "Ethereum Sepolia testnet");
        assert_eq!(config.route.destination_label(), "Base Sepolia testnet");
        assert_eq!(config.route.source_chain_id(), 11_155_111);
        assert_eq!(config.route.destination_chain_id(), 84_532);
        assert_eq!(config.usdc, ETHEREUM_SEPOLIA_USDC);
        assert_eq!(
            config.rpc.source.as_str(),
            "https://ethereum-sepolia.example/"
        );
        assert_eq!(
            config.rpc.destination.as_str(),
            "https://base-sepolia.example/"
        );
        assert_eq!(
            config.provenance.rpc.source.source,
            ConfigValueSource::CliFlag("--ethereum-sepolia-rpc")
        );
        assert_eq!(
            config.provenance.rpc.destination.source,
            ConfigValueSource::CliFlag("--base-sepolia-rpc")
        );
    }

    #[test]
    fn route_catalog_resolves_supported_route() {
        let route = supported_route_config();

        assert_eq!(route.cctp_route().source_chain(), NamedChain::Mainnet);
        assert_eq!(
            route.cctp_route().destination_chain(),
            NamedChain::Hyperliquid
        );
        assert_eq!(route.source_label(), "Ethereum mainnet");
        assert_eq!(route.destination_label(), "HyperEVM");
        assert_eq!(route.default_usdc(), MAINNET_USDC);
    }

    #[test]
    fn route_catalog_resolves_explicit_testnet_route() {
        let route = testnet_route_config();

        assert_eq!(route.cctp_route().source_chain(), NamedChain::Sepolia);
        assert_eq!(
            route.cctp_route().destination_chain(),
            NamedChain::BaseSepolia
        );
        assert_eq!(route.source_label(), "Ethereum Sepolia testnet");
        assert_eq!(route.destination_label(), "Base Sepolia testnet");
        assert_eq!(route.default_usdc(), ETHEREUM_SEPOLIA_USDC);
    }

    #[test]
    fn route_catalog_lists_supported_routes_on_rejection() {
        let error = ROUTE_CATALOG
            .resolve(ChainArg::HyperEvm, ChainArg::Ethereum)
            .expect_err("unsupported route is invalid");
        let message = error.to_string();

        assert!(
            message.contains("unsupported route hyperevm -> ethereum"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("supported routes: ethereum -> hyperevm"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains("ethereum-sepolia -> base-sepolia"),
            "unexpected error: {message}"
        );
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
            .expect("endpoint catalog resolves current route");
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
            .expect("endpoint catalog resolves testnet route");
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
        let file = BridgeConfigFile {
            ethereum_sepolia_rpc: Some("https://file.ethereum-sepolia.example".to_owned()),
            base_sepolia_rpc: Some("https://file.base-sepolia.example".to_owned()),
            ..BridgeConfigFile::default()
        };
        let env = TestEnv(HashMap::from([(
            ETHEREUM_SEPOLIA_RPC_ENV.to_owned(),
            "https://env.ethereum-sepolia.example".to_owned(),
        )]));

        let endpoints = CHAIN_ENDPOINT_CATALOG
            .resolve_route(&route, &args, &file, &env)
            .expect("endpoint catalog applies per-chain testnet precedence");
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
        let file = BridgeConfigFile {
            ethereum_rpc: Some("https://file.ethereum.example".to_owned()),
            hyperevm_rpc: Some("https://file.hyperevm.example".to_owned()),
            ..BridgeConfigFile::default()
        };
        let env = TestEnv(HashMap::from([(
            ETHEREUM_RPC_ENV.to_owned(),
            "https://env.ethereum.example".to_owned(),
        )]));

        let endpoints = CHAIN_ENDPOINT_CATALOG
            .resolve_route(&route, &args, &file, &env)
            .expect("endpoint catalog applies per-chain precedence");
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
            .expect_err("invalid source endpoint is rejected");
        let message = error.to_string();

        assert!(
            message.contains("failed to parse --ethereum-rpc as a URL"),
            "unexpected error: {message}"
        );
    }

    #[test]
    fn config_service_rejects_unsupported_route_before_required_inputs() {
        let mut args = empty_args();
        args.from = Some(ChainArg::HyperEvm);
        args.to = Some(ChainArg::Ethereum);

        let error = empty_service()
            .bridge_config(args)
            .expect_err("unsupported route is invalid");
        let message = error.to_string();

        assert!(
            message.contains("unsupported route hyperevm -> ethereum"),
            "unexpected error: {message}"
        );
        assert!(
            !message.contains("missing amount"),
            "route policy should fail before later config checks: {message}"
        );
    }

    #[test]
    fn config_service_rejects_mixed_mainnet_testnet_route_before_required_inputs() {
        let mut args = empty_args();
        args.from = Some(ChainArg::Ethereum);
        args.to = Some(ChainArg::BaseSepolia);

        let error = empty_service()
            .bridge_config(args)
            .expect_err("mixed mainnet/testnet route is invalid");
        let message = error.to_string();

        assert!(
            message.contains("unsupported route ethereum -> base-sepolia"),
            "unexpected error: {message}"
        );
        assert!(
            !message.contains("missing amount"),
            "route policy should fail before later config checks: {message}"
        );
    }

    #[test]
    fn config_service_accepts_fast_without_manual_fee() {
        let mut args = sample_args();
        args.fast = Some(true);

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(
            config.transfer,
            TransferRequest::Fast {
                fee_cap: FastFeeCapRequest::Auto
            }
        );
        assert_eq!(
            config.provenance.fast_mode,
            ConfigValueSource::CliFlag("--fast")
        );
        assert_eq!(
            config.provenance.max_fee,
            MaxFeeProvenance::AutoResolved {
                source: ConfigValueSource::Default("live fee + 20% buffer")
            }
        );
    }

    #[test]
    fn config_service_parses_manual_fast_fee_cap() {
        let mut args = sample_args();
        args.fast = Some(true);
        args.max_fee_usdc = Some("0.01".to_owned());

        let config = empty_service().bridge_config(args).expect("valid config");
        assert_eq!(
            config.transfer,
            TransferRequest::Fast {
                fee_cap: FastFeeCapRequest::Manual(ManualFastFeeCap {
                    amount: UsdcAmount::from_atomic(U256::from(10_000u64)),
                    source: ConfigValueSource::CliFlag("--max-fee-usdc")
                })
            }
        );
        assert_eq!(
            config.provenance.max_fee,
            MaxFeeProvenance::Manual {
                source: ConfigValueSource::CliFlag("--max-fee-usdc")
            }
        );
    }

    #[test]
    fn config_service_rejects_manual_fee_cap_without_fast_mode() {
        let mut args = sample_args();
        args.max_fee_usdc = Some("0.01".to_owned());

        let error = empty_service()
            .bridge_config(args)
            .expect_err("fee cap without fast mode is invalid");

        assert!(
            error.to_string().contains("only valid with --fast"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn fast_fee_resolution_uses_buffered_live_fee_without_manual_cap() {
        let amount = UsdcAmount::from_atomic(U256::from(1_000_000u64));
        let live_fee = TransferFee::new(1000, FeeBps::from_hundredths(100));

        let resolved = resolve_fast_transfer_fee(amount, live_fee, FastFeeCapRequest::Auto)
            .expect("fee resolution succeeds");

        assert_eq!(resolved.live_fee, live_fee);
        assert_eq!(resolved.live_fee_amount, U256::from(100u64));
        assert_eq!(resolved.max_fee, U256::from(120u64));
        assert_eq!(
            resolved.cap_source,
            FastFeeCapSource::LiveBuffered {
                buffer_percent: DEFAULT_FAST_FEE_BUFFER_PERCENT
            }
        );
    }

    #[test]
    fn fast_fee_resolution_uses_valid_manual_cap() {
        let amount = UsdcAmount::from_atomic(U256::from(1_000_000u64));
        let live_fee = TransferFee::new(1000, FeeBps::from_hundredths(100));

        let resolved = resolve_fast_transfer_fee(
            amount,
            live_fee,
            FastFeeCapRequest::Manual(ManualFastFeeCap {
                amount: UsdcAmount::from_atomic(U256::from(150u64)),
                source: ConfigValueSource::CliFlag("--max-fee-usdc"),
            }),
        )
        .expect("fee resolution succeeds");

        assert_eq!(resolved.live_fee_amount, U256::from(100u64));
        assert_eq!(resolved.max_fee, U256::from(150u64));
        assert_eq!(resolved.cap_source, FastFeeCapSource::Manual);
    }

    #[test]
    fn fast_fee_resolution_rejects_manual_cap_below_live_fee() {
        let amount = UsdcAmount::from_atomic(U256::from(1_000_000u64));
        let live_fee = TransferFee::new(1000, FeeBps::from_hundredths(100));

        let error = resolve_fast_transfer_fee(
            amount,
            live_fee,
            FastFeeCapRequest::Manual(ManualFastFeeCap {
                amount: UsdcAmount::from_atomic(U256::from(99u64)),
                source: ConfigValueSource::CliFlag("--max-fee-usdc"),
            }),
        )
        .expect_err("below-fee cap is invalid");

        assert!(
            error.to_string().contains("below the current live"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn config_service_uses_source_wallet_for_default_self_relay_account() {
        let mut args = sample_args();
        args.self_relay = Some(true);

        let config = empty_service().bridge_config(args).expect("valid config");
        assert_eq!(
            config.relay,
            RelayConfig::SelfRelay {
                wallet: WalletConfig::Trezor { account: 0 }
            }
        );
    }

    #[test]
    fn config_service_accepts_distinct_self_relay_account() {
        let mut args = sample_args();
        args.self_relay = Some(true);
        args.relay_trezor_account = Some(2);

        let config = empty_service().bridge_config(args).expect("valid config");
        assert_eq!(config.source_wallet, WalletConfig::Trezor { account: 0 });
        assert_eq!(
            config.relay,
            RelayConfig::SelfRelay {
                wallet: WalletConfig::Trezor { account: 2 }
            }
        );
    }

    #[test]
    fn config_service_uses_relay_account_for_default_fallback() {
        let mut args = sample_args();
        args.relay_trezor_account = Some(2);

        let config = empty_service().bridge_config(args).expect("valid config");
        assert_eq!(
            config.relay,
            RelayConfig::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay {
                    wallet: WalletConfig::Trezor { account: 2 }
                }
            }
        );
    }

    #[test]
    fn wallet_config_describes_trezor_derivation_and_chain_binding() {
        let wallet = WalletConfig::Trezor { account: 3 };
        let address = address!("0000000000000000000000000000000000000003");

        let account = wallet.account_info(WalletRole::SourceBurn, ChainArg::Ethereum, address);

        wallet.validate().expect("wallet config is valid");
        assert_eq!(account.role, WalletRole::SourceBurn);
        assert_eq!(account.wallet, wallet);
        assert_eq!(
            account.derivation_path,
            WalletDerivationPath::TrezorLive { account: 3 }
        );
        assert_eq!(account.derivation_path.to_string(), "m/44'/60'/3'/0/0");
        assert_eq!(account.chain_label(), "Ethereum mainnet");
        assert_eq!(account.chain.named_chain(), NamedChain::Mainnet);
        assert_eq!(account.chain, ChainArg::Ethereum);
        assert_eq!(account.chain_id(), 1);
        assert_eq!(account.address, address);
    }

    #[test]
    fn relay_config_validates_without_device() {
        let relay = RelayConfig::from_mode(RelayMode::SelfRelay, WalletKind::Trezor, Some(2), 0);

        relay.validate().expect("relay config is valid");
        assert_eq!(relay.wallet(), WalletConfig::Trezor { account: 2 });
        assert!(
            RelayConfig::from_mode(RelayMode::WaitForRelayer, WalletKind::Trezor, None, 0)
                .validate()
                .is_ok()
        );
    }

    #[test]
    fn config_service_reads_config_file() {
        let path = write_config(
            r#"
amount = "2.5"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
recipient = "0x0000000000000000000000000000000000000007"
usdc = "0x0000000000000000000000000000000000000008"
trezor_account = 4
self_relay = true
relay_trezor_account = 5
receive_attempts = 3
receive_interval_secs = 7
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(
            config.route.cctp_route().source_chain(),
            NamedChain::Mainnet
        );
        assert_eq!(
            config.route.cctp_route().destination_chain(),
            NamedChain::Hyperliquid
        );
        assert_eq!(config.amount.atomic(), U256::from(2_500_000u64));
        assert_eq!(
            config.recipient,
            RecipientConfig::Address(address!("0000000000000000000000000000000000000007"))
        );
        assert_eq!(
            config.usdc,
            address!("0000000000000000000000000000000000000008")
        );
        assert_eq!(config.rpc.source.as_str(), "https://file.ethereum.example/");
        assert_eq!(
            config.rpc.destination.as_str(),
            "https://file.hyperevm.example/"
        );
        assert_eq!(config.source_wallet, WalletConfig::Trezor { account: 4 });
        assert_eq!(
            config.relay,
            RelayConfig::SelfRelay {
                wallet: WalletConfig::Trezor { account: 5 }
            }
        );
        assert_eq!(
            config.receive_polling,
            ReceivePolling {
                attempts: Some(3),
                interval_secs: Some(7)
            }
        );
        assert_eq!(
            config.provenance.route,
            RouteConfigProvenance {
                from: ConfigValueSource::Default("ethereum"),
                to: ConfigValueSource::Default("hyperevm")
            }
        );
        assert_eq!(
            config.provenance.recipient,
            RecipientProvenance::Explicit {
                source: ConfigValueSource::ConfigFile("recipient")
            }
        );
        assert_eq!(
            config.provenance.rpc.source.source,
            ConfigValueSource::ConfigFile("ethereum_rpc")
        );
        assert_eq!(
            config.provenance.source_wallet.account,
            ConfigValueSource::ConfigFile("trezor_account")
        );
        assert_eq!(
            config.provenance.relay_wallet,
            RelayWalletProvenance::ExplicitAccount {
                account: ConfigValueSource::ConfigFile("relay_trezor_account")
            }
        );
        assert_eq!(
            config.provenance.relay_mode,
            ConfigValueSource::ConfigFile("self_relay")
        );
    }

    #[test]
    fn config_service_applies_cli_env_file_default_precedence() {
        let path = write_config(
            r#"
amount = "1"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
trezor_account = 4
dry_run = true
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);
        args.amount = Some("3".to_owned());
        args.ethereum_rpc = Some("https://cli.ethereum.example".to_owned());
        args.trezor_account = Some(9);

        let config = config_service(&[(HYPEREVM_RPC_ENV, "https://env.hyperevm.example")])
            .bridge_config(args)
            .expect("valid config");

        assert_eq!(config.amount.atomic(), U256::from(3_000_000u64));
        assert_eq!(config.rpc.source.as_str(), "https://cli.ethereum.example/");
        assert_eq!(
            config.rpc.destination.as_str(),
            "https://env.hyperevm.example/"
        );
        assert_eq!(config.source_wallet, WalletConfig::Trezor { account: 9 });
        assert_eq!(
            config.relay,
            RelayConfig::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay {
                    wallet: WalletConfig::Trezor { account: 9 }
                }
            }
        );
        assert_eq!(config.run_mode, BridgeRunMode::DryRun);
        assert_eq!(
            config.provenance.amount,
            ConfigValueSource::CliFlag("--amount")
        );
        assert_eq!(
            config.provenance.rpc.source.source,
            ConfigValueSource::CliFlag("--ethereum-rpc")
        );
        assert_eq!(
            config.provenance.rpc.destination.source,
            ConfigValueSource::EnvVar(HYPEREVM_RPC_ENV)
        );
        assert_eq!(
            config.provenance.source_wallet,
            SourceWalletProvenance {
                wallet: ConfigValueSource::Default("trezor"),
                account: ConfigValueSource::CliFlag("--trezor-account")
            }
        );
        assert_eq!(
            config.provenance.relay_mode,
            ConfigValueSource::Default("wait for relayer")
        );
        assert_eq!(
            config.provenance.fast_mode,
            ConfigValueSource::Default("standard finality")
        );
    }

    #[test]
    fn config_service_cli_false_overrides_file_true() {
        let path = write_config(
            r#"
amount = "1"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
fast = true
max_fee_usdc = "0.01"
self_relay = true
dry_run = true
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);
        args.fast = Some(false);
        args.self_relay = Some(false);
        args.dry_run = Some(false);

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(config.transfer, TransferRequest::Standard);
        assert_eq!(
            config.relay,
            RelayConfig::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay {
                    wallet: WalletConfig::Trezor { account: 0 }
                }
            }
        );
        assert_eq!(config.run_mode, BridgeRunMode::Execute);
        assert_eq!(
            config.provenance.fast_mode,
            ConfigValueSource::CliFlag("--fast")
        );
        assert_eq!(config.provenance.max_fee, MaxFeeProvenance::NotApplicable);
    }

    #[test]
    fn config_service_keeps_confirmation_skip_cli_only() {
        let mut args = sample_args();
        args.yes = true;

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(config.confirmation, ConfirmationPolicy::SkipPrompt);
    }

    #[test]
    fn config_service_resolves_output_mode_from_cli_and_file() {
        let mut args = sample_args();
        args.output = Some(OutputMode::Json);

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(config.output, OutputMode::Json);
        assert_eq!(
            config.provenance.output,
            ConfigValueSource::CliFlag("--output")
        );

        let path = write_config(
            r#"
amount = "1"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
output = "json"
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(config.output, OutputMode::Json);
        assert_eq!(
            config.provenance.output,
            ConfigValueSource::ConfigFile("output")
        );
    }

    #[test]
    fn config_service_cli_output_mode_overrides_file() {
        let path = write_config(
            r#"
amount = "1"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
output = "json"
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);
        args.output = Some(OutputMode::Human);

        let config = empty_service().bridge_config(args).expect("valid config");

        assert_eq!(config.output, OutputMode::Human);
        assert_eq!(
            config.provenance.output,
            ConfigValueSource::CliFlag("--output")
        );
    }

    #[test]
    fn config_service_uses_env_rpc_over_file() {
        let path = write_config(
            r#"
amount = "1"
ethereum_rpc = "https://file.ethereum.example"
hyperevm_rpc = "https://file.hyperevm.example"
"#,
        );
        let mut args = empty_args();
        args.config = Some(path);

        let config = config_service(&[
            (ETHEREUM_RPC_ENV, "https://env.ethereum.example"),
            (HYPEREVM_RPC_ENV, "https://env.hyperevm.example"),
        ])
        .bridge_config(args)
        .expect("valid config");

        assert_eq!(config.rpc.source.as_str(), "https://env.ethereum.example/");
        assert_eq!(
            config.rpc.destination.as_str(),
            "https://env.hyperevm.example/"
        );
        assert_eq!(
            config.provenance.rpc.source.source,
            ConfigValueSource::EnvVar(ETHEREUM_RPC_ENV)
        );
        assert_eq!(
            config.provenance.rpc.destination.source,
            ConfigValueSource::EnvVar(HYPEREVM_RPC_ENV)
        );
    }

    #[test]
    fn rpc_endpoint_provenance_redacts_secret_url_parts() {
        let url: Url = "https://user:password@rpc.example:8545/v3/secret-key?api-key=secret"
            .parse()
            .expect("valid URL");

        let redacted = redact_rpc_endpoint(&url);

        assert_eq!(redacted, "https://<redacted>.example:8545/...");
        assert!(!redacted.contains("user"));
        assert!(!redacted.contains("password"));
        assert!(!redacted.contains("secret"));
        assert!(!redacted.contains("api-key"));
    }

    #[test]
    fn config_service_rejects_invalid_config_file() {
        let path = write_config("unknown = true\n");
        let mut args = sample_args();
        args.config = Some(path);

        assert!(empty_service().bridge_config(args).is_err());
    }

    #[test]
    fn config_service_rejects_missing_required_values() {
        let error = empty_service()
            .bridge_config(empty_args())
            .expect_err("missing amount is invalid");

        assert!(
            error.to_string().contains("missing amount"),
            "unexpected error: {error}"
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

    #[test]
    fn bridge_intent_captures_contracts_provider_checks_and_relay_account() {
        let mut args = sample_args();
        args.self_relay = Some(true);
        args.relay_trezor_account = Some(2);
        let config = empty_service().bridge_config(args).expect("valid config");
        let source_account = config.source_wallet.account_info(
            WalletRole::SourceBurn,
            config.route.from(),
            source_sender(),
        );
        let relay_account = config.relay.wallet().account_info(
            WalletRole::DestinationRelay,
            config.route.to(),
            address!("0000000000000000000000000000000000000004"),
        );
        let provider_validation = ProviderValidation::new(
            config.route,
            config.route.source_chain_id(),
            config.route.destination_chain_id(),
        )
        .expect("chain IDs match");
        let contracts = BridgeContracts {
            token_messenger: address!("0000000000000000000000000000000000000010"),
            message_transmitter: address!("0000000000000000000000000000000000000020"),
            destination_domain: DomainId::HyperEvm,
        };

        let intent = BridgeIntent::new(
            &config,
            source_account,
            recipient(),
            ResolvedRelay::SelfRelay {
                account: relay_account,
            },
            provider_validation,
            contracts,
            ResolvedTransferMode::standard(),
        );

        assert_eq!(intent.route, config.route);
        assert_eq!(intent.source_account, source_account);
        assert_eq!(intent.recipient, recipient());
        assert_eq!(
            intent.amount,
            UsdcAmount::from_atomic(U256::from(1_250_000u64))
        );
        assert_eq!(
            intent.relay,
            ResolvedRelay::SelfRelay {
                account: relay_account
            }
        );
        assert_eq!(intent.provider_validation, provider_validation);
        assert_eq!(intent.contracts, contracts);
        assert_eq!(intent.transfer, ResolvedTransferMode::standard());
        assert_eq!(intent.provenance, config.provenance);
    }

    #[test]
    fn json_reporter_emits_machine_readable_intent_and_outcome_events() {
        let mut args = sample_args();
        args.self_relay = Some(true);
        args.relay_trezor_account = Some(2);
        args.output = Some(OutputMode::Json);
        let config = empty_service().bridge_config(args).expect("valid config");
        let source_account = config.source_wallet.account_info(
            WalletRole::SourceBurn,
            config.route.from(),
            source_sender(),
        );
        let relay_account = config.relay.wallet().account_info(
            WalletRole::DestinationRelay,
            config.route.to(),
            address!("0000000000000000000000000000000000000004"),
        );
        let provider_validation = ProviderValidation::new(
            config.route,
            config.route.source_chain_id(),
            config.route.destination_chain_id(),
        )
        .expect("chain IDs match");
        let contracts = mock_contracts();
        let intent = BridgeIntent::new(
            &config,
            source_account,
            recipient(),
            ResolvedRelay::SelfRelay {
                account: relay_account,
            },
            provider_validation,
            contracts,
            ResolvedTransferMode::standard(),
        );
        let outcome = BridgeOutcome {
            source_sender: source_sender(),
            recipient: recipient(),
            token_messenger: contracts.token_messenger,
            destination_domain: contracts.destination_domain,
            approval: ApprovalOutcome::Sent {
                tx_hash: tx_hash(0x11),
            },
            burn_tx: tx_hash(0x22),
            attestation: AttestationOutcome {
                message_len: MOCK_MESSAGE.len(),
            },
            completion: CompletionOutcome::SelfRelayMinted {
                tx_hash: tx_hash(0x33),
            },
        };
        let lines = SharedJsonLines::default();
        let reporter = JsonReporter::new(lines.clone());

        reporter
            .report_intent(&intent)
            .expect("intent JSON event serializes");
        reporter
            .report_workflow_start()
            .expect("workflow JSON event serializes");
        reporter
            .report_outcome(&outcome)
            .expect("outcome JSON event serializes");

        let events = lines.json_values();
        let source_address = source_sender().to_string();
        let recipient_address = recipient().to_string();
        let approval_tx = tx_hash(0x11).to_string();
        let burn_tx = tx_hash(0x22).to_string();
        let mint_tx = tx_hash(0x33).to_string();

        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["event"].as_str(), Some("bridge_intent"));
        assert_eq!(
            events[0]["data"]["route"]["source"]["cli"].as_str(),
            Some("ethereum")
        );
        assert_eq!(
            events[0]["data"]["route"]["source"]["chain"].as_str(),
            Some("mainnet")
        );
        assert_eq!(
            events[0]["data"]["route"]["source"]["chain_id"].as_u64(),
            Some(1)
        );
        assert_eq!(
            events[0]["data"]["route"]["destination"]["cli"].as_str(),
            Some("hyperevm")
        );
        assert_eq!(
            events[0]["data"]["route"]["destination"]["chain"].as_str(),
            Some("hyperliquid")
        );
        assert_eq!(
            events[0]["data"]["route"]["destination"]["chain_id"].as_u64(),
            Some(999)
        );
        assert_eq!(
            events[0]["data"]["signer"]["address"].as_str(),
            Some(source_address.as_str())
        );
        assert_eq!(
            events[0]["data"]["signer"]["chain"]["chain"].as_str(),
            Some("mainnet")
        );
        assert_eq!(
            events[0]["data"]["signer"]["chain"]["chain_id"].as_u64(),
            Some(1)
        );
        assert_eq!(
            events[0]["data"]["recipient"].as_str(),
            Some(recipient_address.as_str())
        );
        assert_eq!(
            events[0]["data"]["provider_checks"]["source"]["expected"]["chain"].as_str(),
            Some("mainnet")
        );
        assert_eq!(
            events[0]["data"]["provider_checks"]["source"]["expected"]["chain_id"].as_u64(),
            Some(1)
        );
        assert_eq!(
            events[0]["data"]["contracts"]["destination_domain"]["domain_id"].as_u64(),
            Some(19)
        );
        assert_eq!(events[0]["data"]["amount"]["usdc"].as_str(), Some("1.25"));
        assert_eq!(
            events[0]["data"]["amount"]["atomic"].as_str(),
            Some("1250000")
        );
        assert_eq!(events[0]["data"]["mode"]["mode"].as_str(), Some("standard"));
        assert_eq!(
            events[0]["data"]["relay_policy"]["mode"].as_str(),
            Some("self_relay")
        );
        assert_eq!(
            events[0]["data"]["relay_policy"]["policy"].as_str(),
            Some("self-relay on destination chain")
        );
        assert_eq!(
            events[0]["data"]["relay_policy"]["destination_provider"].as_str(),
            Some("self_relay_signer")
        );
        assert!(events[0]["data"]["relay_policy"]["fallback"].is_null());
        assert_eq!(
            events[0]["data"]["provenance"]["output"].as_str(),
            Some("CLI flag --output")
        );
        assert_eq!(events[1]["event"].as_str(), Some("workflow_start"));
        assert_eq!(events[2]["event"].as_str(), Some("bridge_outcome"));
        assert_eq!(events[2]["data"]["status"].as_str(), Some("complete"));
        assert_eq!(
            events[2]["data"]["destination_domain"]["domain_id"].as_u64(),
            Some(19)
        );
        assert_eq!(
            events[2]["data"]["approval"]["status"].as_str(),
            Some("confirmed")
        );
        assert_eq!(
            events[2]["data"]["transactions"]["approval"].as_str(),
            Some(approval_tx.as_str())
        );
        assert_eq!(
            events[2]["data"]["transactions"]["burn"].as_str(),
            Some(burn_tx.as_str())
        );
        assert_eq!(
            events[2]["data"]["transactions"]["mint"].as_str(),
            Some(mint_tx.as_str())
        );
        assert_eq!(
            events[2]["data"]["completion"]["status"].as_str(),
            Some("self_relay_minted")
        );
    }

    #[test]
    fn confirmation_input_requires_exact_confirm_token() {
        validate_confirmation_input("CONFIRM\n").expect("CONFIRM is accepted");

        let error = validate_confirmation_input("confirm\n").expect_err("lowercase is rejected");
        assert!(
            error.to_string().contains("not confirmed"),
            "unexpected error: {error}"
        );
    }

    #[tokio::test]
    async fn bridge_app_dry_run_reports_intent_without_approval_or_workflow() {
        let calls = SharedCalls::default();
        let mut args = sample_args();
        args.dry_run = Some(true);
        let config = empty_service().bridge_config(args).expect("valid config");
        let app = mock_bridge_app(calls.clone());

        let result = app.run(config).await.expect("dry run succeeds");

        assert_eq!(result, BridgeRunResult::DryRun);
        assert_eq!(
            calls.entries(),
            vec![
                "read_only_providers",
                "validate_providers",
                "source_signer",
                "relay_signer",
                "bridge_providers",
                "bridge_standard",
                "resolve_fee",
                "bridge_standard",
                "contracts",
                "report_intent",
                "report_dry_run_complete"
            ]
        );
    }

    #[tokio::test]
    async fn bridge_app_executes_successful_workflow_with_injected_services() {
        let calls = SharedCalls::default();
        let config = empty_service()
            .bridge_config(sample_args())
            .expect("valid config");
        let app = mock_bridge_app(calls.clone());

        let result = app.run(config).await.expect("bridge succeeds");

        let BridgeRunResult::Executed(outcome) = result else {
            panic!("expected executed bridge result");
        };
        assert_eq!(
            outcome.approval,
            ApprovalOutcome::Skipped {
                allowance: U256::MAX
            }
        );
        assert_eq!(outcome.burn_tx, tx_hash(0x22));
        assert_eq!(outcome.completion, CompletionOutcome::RelayerCompleted);
        assert_eq!(
            calls.entries(),
            vec![
                "read_only_providers",
                "validate_providers",
                "source_signer",
                "relay_signer",
                "bridge_providers",
                "bridge_standard",
                "resolve_fee",
                "bridge_standard",
                "contracts",
                "report_intent",
                "confirm",
                "report_workflow_start",
                "runtime",
                "report_outcome"
            ]
        );
    }

    #[tokio::test]
    async fn bridge_app_rejects_testnet_rpc_mismatch_before_wallet_initialization() {
        let calls = SharedCalls::default();
        let config = empty_service()
            .bridge_config(testnet_args())
            .expect("valid testnet config");
        let app = mock_bridge_app_with_provider_chain_ids(calls.clone(), 1, 84_532);

        let error = app
            .run(config)
            .await
            .expect_err("testnet route with mainnet source RPC is invalid");
        let message = error.to_string();

        assert!(
            message.contains("source RPC"),
            "unexpected error: {message}"
        );
        assert_eq!(
            calls.entries(),
            vec!["read_only_providers", "validate_providers"]
        );
    }

    #[derive(Clone, Debug)]
    struct SharedCalls(Rc<RefCell<Vec<&'static str>>>);

    impl Default for SharedCalls {
        fn default() -> Self {
            Self(Rc::new(RefCell::new(Vec::new())))
        }
    }

    impl SharedCalls {
        fn push(&self, label: &'static str) {
            self.0.borrow_mut().push(label);
        }

        fn entries(&self) -> Vec<&'static str> {
            self.0.borrow().clone()
        }
    }

    #[derive(Clone, Debug, Default)]
    struct SharedJsonLines(Rc<RefCell<Vec<String>>>);

    impl SharedJsonLines {
        fn json_values(&self) -> Vec<serde_json::Value> {
            self.0
                .borrow()
                .iter()
                .map(|line| {
                    serde_json::from_str(line)
                        .expect("JSON reporter should emit one valid JSON object per line")
                })
                .collect()
        }
    }

    impl JsonReportSink for SharedJsonLines {
        fn write_json(&self, event: &JsonReportEvent) -> Result<()> {
            let line =
                serde_json::to_string(event).wrap_err("failed to serialize JSON test line")?;
            self.0.borrow_mut().push(line);
            Ok(())
        }
    }

    #[derive(Clone, Copy, Debug)]
    struct MockSigner;

    #[derive(Clone, Debug)]
    struct MockWalletService {
        calls: SharedCalls,
    }

    #[async_trait(?Send)]
    impl WalletService for MockWalletService {
        type SourceSigner = MockSigner;
        type RelaySigner = MockSigner;

        async fn source_signer(
            &self,
            config: &BridgeConfig,
        ) -> Result<SourceSignerRuntime<Self::SourceSigner>> {
            self.calls.push("source_signer");
            Ok(SourceSignerRuntime {
                signer: MockSigner,
                account: config.source_wallet.account_info(
                    WalletRole::SourceBurn,
                    config.route.from(),
                    source_sender(),
                ),
            })
        }

        async fn relay_signer(
            &self,
            config: &BridgeConfig,
        ) -> Result<RelaySignerRuntime<Self::RelaySigner>> {
            self.calls.push("relay_signer");
            let wallet = config.relay.wallet();

            Ok(RelaySignerRuntime {
                signer: MockSigner,
                account: wallet.account_info(
                    WalletRole::DestinationRelay,
                    config.route.to(),
                    address!("0000000000000000000000000000000000000003"),
                ),
            })
        }
    }

    #[derive(Clone, Debug)]
    struct MockProviderService {
        calls: SharedCalls,
    }

    struct MockProviders;

    struct MockBridge {
        transfer_mode: TransferMode,
    }

    impl ProviderService<MockWalletService> for MockProviderService {
        type Providers = MockProviders;
        type Bridge = MockBridge;
        type Runtime = MockBridgeRuntime;

        fn read_only_providers(&self, _config: &BridgeConfig) -> Self::Providers {
            self.calls.push("read_only_providers");
            MockProviders
        }

        fn bridge_providers(
            &self,
            _config: &BridgeConfig,
            _source_signer: MockSigner,
            _relay_signer: RelaySignerRuntime<MockSigner>,
        ) -> Self::Providers {
            self.calls.push("bridge_providers");
            MockProviders
        }

        fn bridge(
            &self,
            _config: &BridgeConfig,
            _providers: &Self::Providers,
            _recipient: Address,
            transfer_mode: TransferMode,
        ) -> Self::Bridge {
            if transfer_mode.is_fast() {
                self.calls.push("bridge_fast");
            } else {
                self.calls.push("bridge_standard");
            }
            MockBridge { transfer_mode }
        }

        fn contracts(&self, _bridge: &Self::Bridge) -> Result<BridgeContracts> {
            self.calls.push("contracts");
            Ok(mock_contracts())
        }

        fn runtime(&self, _bridge: Self::Bridge, _providers: Self::Providers) -> Self::Runtime {
            self.calls.push("runtime");
            MockBridgeRuntime {
                allowance: U256::MAX,
                ..Default::default()
            }
        }
    }

    #[derive(Clone, Debug)]
    struct MockProviderValidationService {
        calls: SharedCalls,
        source_chain_id: Option<u64>,
        destination_chain_id: Option<u64>,
    }

    #[async_trait(?Send)]
    impl ProviderValidationService<MockProviders> for MockProviderValidationService {
        async fn validate(
            &self,
            config: &BridgeConfig,
            _providers: &MockProviders,
        ) -> Result<ProviderValidation> {
            self.calls.push("validate_providers");
            ProviderValidation::new(
                config.route,
                self.source_chain_id
                    .unwrap_or_else(|| config.route.source_chain_id()),
                self.destination_chain_id
                    .unwrap_or_else(|| config.route.destination_chain_id()),
            )
        }
    }

    #[derive(Clone, Debug)]
    struct MockFeeResolutionService {
        calls: SharedCalls,
    }

    #[async_trait(?Send)]
    impl FeeResolutionService<MockBridge> for MockFeeResolutionService {
        async fn resolve(
            &self,
            bridge: &MockBridge,
            _config: &BridgeConfig,
        ) -> Result<ResolvedTransferMode> {
            self.calls.push("resolve_fee");
            assert!(
                !bridge.transfer_mode.is_fast(),
                "fee resolution bridge should use standard mode"
            );
            Ok(ResolvedTransferMode::standard())
        }
    }

    #[derive(Clone, Debug)]
    struct MockApprovalService {
        calls: SharedCalls,
    }

    impl IntentApprovalService for MockApprovalService {
        fn confirm(&self, _intent: &BridgeIntent, _policy: ConfirmationPolicy) -> Result<()> {
            self.calls.push("confirm");
            Ok(())
        }
    }

    #[derive(Clone, Debug)]
    struct MockReporter {
        calls: SharedCalls,
    }

    impl Reporter for MockReporter {
        fn report_intent(&self, _intent: &BridgeIntent) -> Result<()> {
            self.calls.push("report_intent");
            Ok(())
        }

        fn report_dry_run_complete(&self) -> Result<()> {
            self.calls.push("report_dry_run_complete");
            Ok(())
        }

        fn report_workflow_start(&self) -> Result<()> {
            self.calls.push("report_workflow_start");
            Ok(())
        }

        fn report_outcome(&self, _outcome: &BridgeOutcome) -> Result<()> {
            self.calls.push("report_outcome");
            Ok(())
        }
    }

    fn mock_bridge_app(
        calls: SharedCalls,
    ) -> BridgeApp<
        MockWalletService,
        MockProviderService,
        MockProviderValidationService,
        MockFeeResolutionService,
        MockApprovalService,
        MockReporter,
    > {
        mock_bridge_app_with_provider_validation(
            calls,
            MockProviderValidationService {
                calls: SharedCalls::default(),
                source_chain_id: None,
                destination_chain_id: None,
            },
        )
    }

    fn mock_bridge_app_with_provider_chain_ids(
        calls: SharedCalls,
        source_chain_id: u64,
        destination_chain_id: u64,
    ) -> BridgeApp<
        MockWalletService,
        MockProviderService,
        MockProviderValidationService,
        MockFeeResolutionService,
        MockApprovalService,
        MockReporter,
    > {
        mock_bridge_app_with_provider_validation(
            calls.clone(),
            MockProviderValidationService {
                calls,
                source_chain_id: Some(source_chain_id),
                destination_chain_id: Some(destination_chain_id),
            },
        )
    }

    fn mock_bridge_app_with_provider_validation(
        calls: SharedCalls,
        provider_validation_service: MockProviderValidationService,
    ) -> BridgeApp<
        MockWalletService,
        MockProviderService,
        MockProviderValidationService,
        MockFeeResolutionService,
        MockApprovalService,
        MockReporter,
    > {
        let provider_validation_service = MockProviderValidationService {
            calls: calls.clone(),
            ..provider_validation_service
        };

        BridgeApp::new(
            MockWalletService {
                calls: calls.clone(),
            },
            MockProviderService {
                calls: calls.clone(),
            },
            provider_validation_service,
            MockFeeResolutionService {
                calls: calls.clone(),
            },
            MockApprovalService {
                calls: calls.clone(),
            },
            MockReporter { calls },
        )
    }

    fn mock_contracts() -> BridgeContracts {
        BridgeContracts {
            token_messenger: address!("0000000000000000000000000000000000000010"),
            message_transmitter: address!("0000000000000000000000000000000000000020"),
            destination_domain: DomainId::HyperEvm,
        }
    }

    #[tokio::test]
    async fn workflow_prefers_relayer_before_fallback() {
        let allowance = U256::from(2_000_000u64);
        let relay_submitter = address!("0000000000000000000000000000000000000003");
        let runtime = MockBridgeRuntime {
            allowance,
            ..Default::default()
        };
        let mut workflow = mock_workflow(
            WorkflowRelay::WaitForRelayer {
                fallback: WorkflowRelayFallback::SelfRelay {
                    submitter: relay_submitter,
                },
            },
            runtime,
        );

        let outcome = workflow.run().await.expect("workflow succeeds");

        assert_eq!(outcome.approval, ApprovalOutcome::Skipped { allowance });
        assert_eq!(outcome.burn_tx, tx_hash(0x22));
        assert_eq!(
            outcome.attestation,
            AttestationOutcome {
                message_len: MOCK_MESSAGE.len()
            }
        );
        assert_eq!(outcome.completion, CompletionOutcome::RelayerCompleted);
        assert_eq!(
            workflow.runtime.calls,
            vec![
                "get_allowance",
                "burn",
                "wait_source_receipt",
                "get_attestation",
                "wait_for_receive"
            ]
        );
        assert_eq!(workflow.runtime.last_mint_from, None);
    }

    #[tokio::test]
    async fn workflow_falls_back_to_self_relay_when_relayer_wait_expires() {
        let relay_submitter = address!("0000000000000000000000000000000000000003");
        let runtime = MockBridgeRuntime {
            receive_completes: false,
            mint_result: MintResult::Minted(tx_hash(0x33)),
            ..Default::default()
        };
        let mut workflow = mock_workflow(
            WorkflowRelay::WaitForRelayer {
                fallback: WorkflowRelayFallback::SelfRelay {
                    submitter: relay_submitter,
                },
            },
            runtime,
        );

        let outcome = workflow.run().await.expect("workflow succeeds");

        assert_eq!(
            outcome.completion,
            CompletionOutcome::FallbackSelfRelayMinted {
                tx_hash: tx_hash(0x33)
            }
        );
        assert_eq!(
            workflow.runtime.calls,
            vec![
                "get_allowance",
                "burn",
                "wait_source_receipt",
                "get_attestation",
                "wait_for_receive",
                "mint_if_needed",
                "wait_destination_receipt"
            ]
        );
        assert_eq!(workflow.runtime.last_mint_from, Some(relay_submitter));
    }

    #[tokio::test]
    async fn workflow_self_relays_with_distinct_relay_submitter() {
        let relay_submitter = address!("0000000000000000000000000000000000000003");
        let runtime = MockBridgeRuntime {
            allowance: U256::ZERO,
            mint_result: MintResult::Minted(tx_hash(0x33)),
            ..Default::default()
        };
        let mut workflow = mock_workflow(
            WorkflowRelay::SelfRelay {
                submitter: relay_submitter,
            },
            runtime,
        );

        let outcome = workflow.run().await.expect("workflow succeeds");

        assert_eq!(
            outcome.approval,
            ApprovalOutcome::Sent {
                tx_hash: tx_hash(0x11)
            }
        );
        assert_eq!(
            outcome.completion,
            CompletionOutcome::SelfRelayMinted {
                tx_hash: tx_hash(0x33)
            }
        );
        assert_eq!(
            workflow.runtime.calls,
            vec![
                "get_allowance",
                "approve",
                "wait_source_receipt",
                "burn",
                "wait_source_receipt",
                "get_attestation",
                "mint_if_needed",
                "wait_destination_receipt"
            ]
        );
        assert_eq!(workflow.runtime.last_mint_from, Some(relay_submitter));
    }

    #[test]
    fn resolved_relay_uses_fallback_account_for_wait_mode() {
        let account = WalletConfig::Trezor { account: 0 }.account_info(
            WalletRole::DestinationRelay,
            ChainArg::HyperEvm,
            address!("0000000000000000000000000000000000000003"),
        );
        let relay = ResolvedRelay::from_config(
            RelayConfig::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay {
                    wallet: WalletConfig::Trezor { account: 0 },
                },
            },
            account,
        );

        assert_eq!(
            relay.workflow_relay(),
            WorkflowRelay::WaitForRelayer {
                fallback: WorkflowRelayFallback::SelfRelay {
                    submitter: account.address
                }
            }
        );
    }

    const MOCK_MESSAGE: &[u8] = &[0xaa, 0xbb, 0xcc];

    fn source_sender() -> Address {
        address!("0000000000000000000000000000000000000001")
    }

    fn recipient() -> Address {
        address!("0000000000000000000000000000000000000002")
    }

    fn tx_hash(byte: u8) -> TxHash {
        TxHash::from([byte; 32])
    }

    fn supported_route_config() -> RouteConfig {
        ROUTE_CATALOG
            .resolve(ChainArg::Ethereum, ChainArg::HyperEvm)
            .expect("supported route")
    }

    fn testnet_route_config() -> RouteConfig {
        ROUTE_CATALOG
            .resolve(ChainArg::EthereumSepolia, ChainArg::BaseSepolia)
            .expect("supported testnet route")
    }

    fn mock_workflow(
        relay: WorkflowRelay,
        runtime: MockBridgeRuntime,
    ) -> BridgeWorkflow<MockBridgeRuntime> {
        BridgeWorkflow::new(
            BridgeWorkflowConfig {
                amount: UsdcAmount::from_atomic(U256::from(1_000_000u64)),
                usdc: MAINNET_USDC,
                transfer_mode: TransferMode::Standard,
                relay,
                receive_polling: ReceivePolling {
                    attempts: Some(1),
                    interval_secs: Some(1),
                },
            },
            runtime,
            source_sender(),
            recipient(),
        )
    }

    struct MockBridgeRuntime {
        allowance: U256,
        approve_tx: TxHash,
        burn_tx: TxHash,
        message: Vec<u8>,
        attestation: AttestationBytes,
        mint_result: MintResult,
        receive_completes: bool,
        calls: Vec<&'static str>,
        last_mint_from: Option<Address>,
    }

    impl Default for MockBridgeRuntime {
        fn default() -> Self {
            Self {
                allowance: U256::MAX,
                approve_tx: tx_hash(0x11),
                burn_tx: tx_hash(0x22),
                message: MOCK_MESSAGE.to_vec(),
                attestation: vec![0xdd],
                mint_result: MintResult::AlreadyRelayed,
                receive_completes: true,
                calls: Vec::new(),
                last_mint_from: None,
            }
        }
    }

    #[async_trait(?Send)]
    impl BridgeRuntime for MockBridgeRuntime {
        fn token_messenger_v2_contract(&self) -> Result<Address> {
            Ok(address!("0000000000000000000000000000000000000010"))
        }

        fn destination_domain_id(&self) -> Result<DomainId> {
            Ok(DomainId::HyperEvm)
        }

        async fn get_allowance(&mut self, _token: Address, _owner: Address) -> Result<U256> {
            self.calls.push("get_allowance");
            Ok(self.allowance)
        }

        async fn approve(
            &mut self,
            _token: Address,
            _owner: Address,
            _amount: U256,
        ) -> Result<TxHash> {
            self.calls.push("approve");
            Ok(self.approve_tx)
        }

        async fn burn(
            &mut self,
            _amount: U256,
            _burn_sender: Address,
            _token: Address,
        ) -> Result<TxHash> {
            self.calls.push("burn");
            Ok(self.burn_tx)
        }

        async fn get_attestation(
            &mut self,
            _burn_tx: TxHash,
            _polling_config: PollingConfig,
        ) -> Result<(Vec<u8>, AttestationBytes)> {
            self.calls.push("get_attestation");
            Ok((self.message.clone(), self.attestation.clone()))
        }

        async fn wait_for_receive(
            &mut self,
            _message: &[u8],
            _max_attempts: Option<u32>,
            _poll_interval: Option<u64>,
        ) -> Result<()> {
            self.calls.push("wait_for_receive");
            if self.receive_completes {
                Ok(())
            } else {
                bail!("destination receive status was not observed")
            }
        }

        async fn mint_if_needed(
            &mut self,
            _message: Vec<u8>,
            _attestation: AttestationBytes,
            from: Address,
        ) -> Result<MintResult> {
            self.calls.push("mint_if_needed");
            self.last_mint_from = Some(from);
            Ok(self.mint_result.clone())
        }

        async fn wait_source_receipt(
            &mut self,
            _tx_hash: TxHash,
            _label: &str,
            _max_attempts: u32,
            _interval: Duration,
        ) -> Result<()> {
            self.calls.push("wait_source_receipt");
            Ok(())
        }

        async fn wait_destination_receipt(
            &mut self,
            _tx_hash: TxHash,
            _label: &str,
            _max_attempts: u32,
            _interval: Duration,
        ) -> Result<()> {
            self.calls.push("wait_destination_receipt");
            Ok(())
        }
    }
}
