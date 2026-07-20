use std::{
    fs,
    num::{NonZeroU32, NonZeroU64},
    path::Path,
};

use alloy::primitives::Address;
use cctp_rs::UsdcAmount;
use clap::ValueEnum;
use eyre::{Result, WrapErr, bail, eyre};
use serde::{Deserialize, Serialize};
use url::Url;

use crate::BridgeArgs;
use crate::chain::ChainArg;
use crate::provider::{CHAIN_ENDPOINT_CATALOG, ResolvedChainRpcEndpoints, RpcEndpoints};
use crate::routes::{ROUTE_CATALOG, RouteConfig};
use crate::wallet::{RelayConfig, WalletConfig, WalletKind};

pub(crate) const ETHEREUM_RPC_ENV: &str = "ETHEREUM_RPC_URL";
pub(crate) const HYPEREVM_RPC_ENV: &str = "HYPEREVM_RPC_URL";
pub(crate) const ETHEREUM_SEPOLIA_RPC_ENV: &str = "ETHEREUM_SEPOLIA_RPC_URL";
pub(crate) const BASE_SEPOLIA_RPC_ENV: &str = "BASE_SEPOLIA_RPC_URL";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum OutputMode {
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

pub(crate) trait ConfigService {
    fn bridge_config(&self, args: BridgeArgs) -> Result<BridgeConfig>;
}

pub(crate) trait EnvSource {
    fn get(&self, key: &str) -> Option<String>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct ProcessEnv;

impl EnvSource for ProcessEnv {
    fn get(&self, key: &str) -> Option<String> {
        std::env::var(key).ok().filter(|value| !value.is_empty())
    }
}

#[derive(Clone, Debug)]
pub(crate) struct CliConfigService<E = ProcessEnv> {
    env: E,
}

impl Default for CliConfigService<ProcessEnv> {
    fn default() -> Self {
        Self { env: ProcessEnv }
    }
}

#[cfg(test)]
impl<E> CliConfigService<E> {
    pub(crate) const fn new(env: E) -> Self {
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
        let receive_polling = ReceivePolling::from_overrides(
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
pub(crate) struct BridgeConfigFile {
    from: Option<ChainArg>,
    to: Option<ChainArg>,
    amount: Option<String>,
    recipient: Option<Address>,
    pub(crate) ethereum_rpc: Option<String>,
    pub(crate) hyperevm_rpc: Option<String>,
    pub(crate) ethereum_sepolia_rpc: Option<String>,
    pub(crate) base_sepolia_rpc: Option<String>,
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
pub(crate) enum ConfigValueSource {
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
pub(crate) struct Sourced<T> {
    value: T,
    source: ConfigValueSource,
}

impl<T> Sourced<T> {
    const fn new(value: T, source: ConfigValueSource) -> Self {
        Self { value, source }
    }

    pub(crate) const fn source(&self) -> ConfigValueSource {
        self.source
    }

    pub(crate) fn value(&self) -> &T {
        &self.value
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

pub(crate) fn sourced_required_cli_env_file(
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
pub(crate) struct BridgeConfigProvenance {
    pub(crate) route: RouteConfigProvenance,
    pub(crate) amount: ConfigValueSource,
    pub(crate) rpc: RpcEndpointsProvenance,
    pub(crate) source_wallet: SourceWalletProvenance,
    pub(crate) relay_wallet: RelayWalletProvenance,
    pub(crate) recipient: RecipientProvenance,
    pub(crate) relay_mode: ConfigValueSource,
    pub(crate) fast_mode: ConfigValueSource,
    pub(crate) max_fee: MaxFeeProvenance,
    pub(crate) output: ConfigValueSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct RouteConfigProvenance {
    pub(crate) from: ConfigValueSource,
    pub(crate) to: ConfigValueSource,
}

impl std::fmt::Display for RouteConfigProvenance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "--from {}, --to {}", self.from, self.to)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RpcEndpointsProvenance {
    pub(crate) source: RpcEndpointProvenance,
    pub(crate) destination: RpcEndpointProvenance,
}

impl RpcEndpointsProvenance {
    pub(crate) fn from_resolved(endpoints: &ResolvedChainRpcEndpoints) -> Self {
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
pub(crate) struct RpcEndpointProvenance {
    pub(crate) source: ConfigValueSource,
    pub(crate) redacted_endpoint: String,
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
pub(crate) struct SourceWalletProvenance {
    pub(crate) wallet: ConfigValueSource,
    pub(crate) account: ConfigValueSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayWalletProvenance {
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
pub(crate) enum RecipientProvenance {
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
pub(crate) enum MaxFeeProvenance {
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
pub(crate) enum BridgeRunMode {
    Execute,
    DryRun,
}

impl BridgeRunMode {
    const fn from_dry_run(dry_run: bool) -> Self {
        if dry_run { Self::DryRun } else { Self::Execute }
    }

    pub(crate) const fn is_dry_run(self) -> bool {
        matches!(self, Self::DryRun)
    }
}

#[derive(Clone, Debug)]
pub(crate) struct BridgeConfig {
    pub(crate) route: RouteConfig,
    pub(crate) amount: UsdcAmount,
    pub(crate) rpc: RpcEndpoints,
    pub(crate) source_wallet: WalletConfig,
    pub(crate) relay: RelayConfig,
    pub(crate) recipient: RecipientConfig,
    pub(crate) usdc: Address,
    pub(crate) transfer: TransferRequest,
    pub(crate) receive_polling: ReceivePolling,
    pub(crate) run_mode: BridgeRunMode,
    pub(crate) confirmation: ConfirmationPolicy,
    pub(crate) output: OutputMode,
    pub(crate) provenance: BridgeConfigProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RecipientConfig {
    Signer,
    Address(Address),
}

impl RecipientConfig {
    pub(crate) const fn resolve(self, signer_address: Address) -> Address {
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
pub(crate) enum RelayMode {
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
pub(crate) enum ConfirmationPolicy {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ReceivePolling {
    Default,
    Attempts(NonZeroU32),
    Interval(NonZeroU64),
    AttemptsAndInterval {
        attempts: NonZeroU32,
        interval_secs: NonZeroU64,
    },
}

impl ReceivePolling {
    fn from_overrides(attempts: Option<u32>, interval_secs: Option<u64>) -> Result<Self> {
        match (attempts, interval_secs) {
            (None, None) => Ok(Self::Default),
            (Some(attempts), None) => Ok(Self::Attempts(receive_attempts(attempts)?)),
            (None, Some(interval_secs)) => {
                Ok(Self::Interval(receive_interval_secs(interval_secs)?))
            }
            (Some(attempts), Some(interval_secs)) => Ok(Self::AttemptsAndInterval {
                attempts: receive_attempts(attempts)?,
                interval_secs: receive_interval_secs(interval_secs)?,
            }),
        }
    }

    pub(crate) fn attempts(self) -> Option<u32> {
        match self {
            Self::Default | Self::Interval(_) => None,
            Self::Attempts(attempts) | Self::AttemptsAndInterval { attempts, .. } => {
                Some(attempts.get())
            }
        }
    }

    pub(crate) fn interval_secs(self) -> Option<u64> {
        match self {
            Self::Default | Self::Attempts(_) => None,
            Self::Interval(interval_secs) | Self::AttemptsAndInterval { interval_secs, .. } => {
                Some(interval_secs.get())
            }
        }
    }
}

fn receive_attempts(value: u32) -> Result<NonZeroU32> {
    NonZeroU32::new(value).ok_or_else(|| eyre!("--receive-attempts must be greater than 0"))
}

fn receive_interval_secs(value: u64) -> Result<NonZeroU64> {
    NonZeroU64::new(value).ok_or_else(|| eyre!("--receive-interval-secs must be greater than 0"))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TransferRequest {
    Standard,
    Fast { fee_cap: FastFeeCapRequest },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum FastFeeCapRequest {
    Auto,
    Manual(ManualFastFeeCap),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ManualFastFeeCap {
    pub(crate) amount: UsdcAmount,
    pub(crate) source: ConfigValueSource,
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests use explicit expect messages to assert setup and validation invariants"
)]
mod tests {
    use super::*;
    use crate::BridgeArgs;
    use crate::routes::ETHEREUM_SEPOLIA_USDC;
    use crate::wallet::{RelayConfig, RelayFallbackConfig, WalletConfig};
    use alloy::primitives::{U256, address};
    use alloy_chains::NamedChain;
    use std::{
        collections::HashMap,
        num::{NonZeroU32, NonZeroU64},
        path::PathBuf,
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
        std::fs::write(&path, contents).expect("test config should be written");
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
            .expect("sample args should resolve to a bridge config");

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
            .expect("testnet args should resolve to a bridge config");

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
    fn config_service_rejects_unsupported_route_before_required_inputs() {
        let mut args = empty_args();
        args.from = Some(ChainArg::HyperEvm);
        args.to = Some(ChainArg::Ethereum);

        let error = empty_service()
            .bridge_config(args)
            .expect_err("unsupported route should be invalid");
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
            .expect_err("mixed mainnet/testnet route should be invalid");
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

        let config = empty_service()
            .bridge_config(args)
            .expect("fast config without manual cap should be valid");

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

        let config = empty_service()
            .bridge_config(args)
            .expect("manual fast fee config should be valid");
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
            .expect_err("fee cap without fast mode should be invalid");

        assert!(
            error.to_string().contains("only valid with --fast"),
            "unexpected error: {error}"
        );
    }

    #[test]
    fn config_service_uses_source_wallet_for_default_self_relay_account() {
        let mut args = sample_args();
        args.self_relay = Some(true);

        let config = empty_service()
            .bridge_config(args)
            .expect("self-relay config should be valid");
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

        let config = empty_service()
            .bridge_config(args)
            .expect("distinct relay account should be valid");
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

        let config = empty_service()
            .bridge_config(args)
            .expect("fallback relay account should be valid");
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

        let config = empty_service()
            .bridge_config(args)
            .expect("config file should resolve");

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
            ReceivePolling::AttemptsAndInterval {
                attempts: NonZeroU32::new(3)
                    .expect("literal non-zero receive attempts should construct"),
                interval_secs: NonZeroU64::new(7)
                    .expect("literal non-zero receive interval should construct")
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
    fn receive_polling_resolves_default_and_partial_overrides() {
        assert_eq!(
            ReceivePolling::from_overrides(None, None)
                .expect("absent receive polling overrides should use defaults"),
            ReceivePolling::Default
        );
        assert_eq!(
            ReceivePolling::from_overrides(Some(3), None)
                .expect("non-zero receive attempts override should resolve"),
            ReceivePolling::Attempts(
                NonZeroU32::new(3).expect("literal non-zero receive attempts should construct")
            )
        );
        assert_eq!(
            ReceivePolling::from_overrides(None, Some(7))
                .expect("non-zero receive interval override should resolve"),
            ReceivePolling::Interval(
                NonZeroU64::new(7).expect("literal non-zero receive interval should construct")
            )
        );
    }

    #[test]
    fn receive_polling_rejects_zero_overrides() {
        let attempts_err = ReceivePolling::from_overrides(Some(0), None)
            .expect_err("zero receive attempts should be rejected");
        assert!(attempts_err.to_string().contains("--receive-attempts"));

        let interval_err = ReceivePolling::from_overrides(None, Some(0))
            .expect_err("zero receive interval should be rejected");
        assert!(interval_err.to_string().contains("--receive-interval-secs"));
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
            .expect("config should apply source precedence");

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

        let config = empty_service()
            .bridge_config(args)
            .expect("CLI false values should override file true values");

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

        let config = empty_service()
            .bridge_config(args)
            .expect("confirmation skip should resolve from CLI only");

        assert_eq!(config.confirmation, ConfirmationPolicy::SkipPrompt);
    }

    #[test]
    fn config_service_resolves_output_mode_from_cli_and_file() {
        let mut args = sample_args();
        args.output = Some(OutputMode::Json);

        let config = empty_service()
            .bridge_config(args)
            .expect("CLI output mode should resolve");

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

        let config = empty_service()
            .bridge_config(args)
            .expect("file output mode should resolve");

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

        let config = empty_service()
            .bridge_config(args)
            .expect("CLI output mode should override file output mode");

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
        .expect("environment RPC values should override file values");

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
            .expect("test URL should parse");

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
            .expect_err("missing amount should be invalid");

        assert!(
            error.to_string().contains("missing amount"),
            "unexpected error: {error}"
        );
    }
}
