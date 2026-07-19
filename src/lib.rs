//! Command-line tool for bridging USDC over Circle CCTP v2 with a Trezor signer.
//!
//! Users choose one of the supported routes, provide RPC endpoints for both
//! chains, review the generated bridge intent, and confirm before any
//! transaction is signed. The CLI verifies route support and provider chain IDs
//! before wallet setup so a mismatched route or RPC endpoint fails before it can
//! affect funds.

use std::{
    io::{self, Write},
    path::PathBuf,
    time::Duration,
};

use alloy::{
    primitives::{Address, TxHash, U256},
    providers::{DynProvider, Provider, ProviderBuilder},
    signers::trezor::{HDPath, TrezorSigner},
};
use async_trait::async_trait;
use cctp_rs::{
    AttestationBytes, CctpV2Bridge, DomainId, MintResult, PollingConfig, TransferFee, TransferMode,
    UsdcAmount,
};
use clap::{Args, Parser, Subcommand};
use eyre::{Result, WrapErr, bail, eyre};
use serde::Serialize;
use tokio::time::sleep;
use tracing_subscriber::EnvFilter;

pub(crate) mod chain;
pub(crate) mod config;
pub(crate) mod reporter;
pub(crate) mod routes;

use chain::ChainArg;
use config::{
    BridgeConfig, CliConfigService, ConfigService, ConfirmationPolicy, FastFeeCapRequest,
    OutputMode, ReceivePolling, RelayMode, TransferRequest, WalletKind,
};
#[cfg(test)]
use config::{ConfigValueSource, EnvSource, ManualFastFeeCap};
use reporter::{BridgeIntent, ConfiguredReporter, Reporter};
#[cfg(test)]
use reporter::{JsonReportSink, JsonReporter};
use routes::RouteConfig;

const DEFAULT_LOG_FILTER: &str = "info,cctp_rs=info";
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

/// Runs the `cctp` command-line application with process arguments and environment.
pub async fn run() -> Result<()> {
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
                        self.config.receive_polling.attempts(),
                        self.config.receive_polling.interval_secs(),
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
                let bytes_read = io::stdin()
                    .read_line(&mut input)
                    .wrap_err("failed to read confirmation input")?;
                if bytes_read == 0 {
                    bail!("bridge intent was not confirmed");
                }
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
#[allow(
    clippy::expect_used,
    reason = "tests use explicit expect messages to assert setup and validation invariants"
)]
mod tests {
    use super::*;
    use crate::routes::{ETHEREUM_SEPOLIA_USDC, MAINNET_USDC, ROUTE_CATALOG};
    use alloy::primitives::address;
    use alloy_chains::NamedChain;
    use cctp_rs::FeeBps;
    use std::{
        cell::RefCell,
        collections::HashMap,
        num::{NonZeroU32, NonZeroU64},
        rc::Rc,
    };

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
    async fn bridge_app_executes_successful_workflow_with_injected_services() -> Result<()> {
        let calls = SharedCalls::default();
        let config = empty_service()
            .bridge_config(sample_args())
            .expect("valid config");
        let app = mock_bridge_app(calls.clone());

        let result = app.run(config).await.expect("bridge succeeds");

        let BridgeRunResult::Executed(outcome) = result else {
            bail!("expected executed bridge result");
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
        Ok(())
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
        fn write_json<T>(&self, event: &T) -> Result<()>
        where
            T: serde::Serialize,
        {
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
                receive_polling: ReceivePolling::AttemptsAndInterval {
                    attempts: NonZeroU32::new(1)
                        .expect("literal non-zero receive attempts should construct"),
                    interval_secs: NonZeroU64::new(1)
                        .expect("literal non-zero receive interval should construct"),
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
