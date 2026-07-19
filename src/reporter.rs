use std::io::{self, Write};

use alloy::primitives::{Address, TxHash, U256};
use cctp_rs::{DomainId, FeeBps, UsdcAmount};
use eyre::{Result, WrapErr};
use serde::{Serialize, Serializer, ser::SerializeStruct};

use crate::chain::ChainArg;
use crate::routes::RouteConfig;
use crate::{
    ApprovalOutcome, BridgeConfig, BridgeConfigProvenance, BridgeContracts, BridgeOutcome,
    BridgeRunMode, CompletionOutcome, ConfigValueSource, FastFeeCapSource, MaxFeeProvenance,
    OutputMode, ProviderChainCheck, ProviderValidation, RecipientProvenance, RelayMode,
    RelayPolicyLabel, RelayWalletProvenance, ResolvedRelay, ResolvedTransferMode,
    RouteConfigProvenance, RpcEndpointProvenance, WalletAccount, mode_label,
};

#[derive(Clone, Debug)]
pub(crate) struct BridgeIntent {
    pub(crate) route: RouteConfig,
    pub(crate) source_account: WalletAccount,
    pub(crate) recipient: Address,
    pub(crate) usdc: Address,
    pub(crate) amount: UsdcAmount,
    pub(crate) transfer: ResolvedTransferMode,
    pub(crate) relay: ResolvedRelay,
    pub(crate) provider_validation: ProviderValidation,
    pub(crate) contracts: BridgeContracts,
    pub(crate) provenance: BridgeConfigProvenance,
}

impl BridgeIntent {
    pub(crate) fn new(
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
pub(crate) enum ConfiguredReporter {
    Human(HumanReporter),
    Json(JsonReporter),
}

impl ConfiguredReporter {
    pub(crate) fn from_output_mode(output: OutputMode) -> Self {
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
pub(crate) struct HumanReporter;

pub(crate) trait Reporter {
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
pub(crate) struct JsonReporter<S = StdoutJsonReportSink> {
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
    pub(crate) const fn new(sink: S) -> Self {
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

pub(crate) trait JsonReportSink {
    fn write_json<T>(&self, event: &T) -> Result<()>
    where
        T: Serialize;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StdoutJsonReportSink;

impl JsonReportSink for StdoutJsonReportSink {
    fn write_json<T>(&self, event: &T) -> Result<()>
    where
        T: Serialize,
    {
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
