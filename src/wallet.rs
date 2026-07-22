use alloy::{
    primitives::Address,
    signers::trezor::{HDPath, TrezorSigner},
};
use async_trait::async_trait;
use clap::ValueEnum;
use eyre::{Result, WrapErr};
use serde::Deserialize;

use crate::chain::ChainArg;
use crate::config::{BridgeConfig, RelayMode};
use crate::{WorkflowRelay, WorkflowRelayFallback};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub(crate) enum WalletKind {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WalletConfig {
    Trezor { account: u32 },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WalletRole {
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
pub(crate) enum WalletDerivationPath {
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
pub(crate) struct WalletAccount {
    pub(crate) role: WalletRole,
    pub(crate) wallet: WalletConfig,
    pub(crate) derivation_path: WalletDerivationPath,
    pub(crate) chain: ChainArg,
    pub(crate) address: Address,
}

impl WalletAccount {
    pub(crate) fn chain_id(&self) -> u64 {
        self.chain.chain_id()
    }

    pub(crate) const fn chain_label(&self) -> &'static str {
        self.chain.display_label()
    }
}

impl WalletConfig {
    pub(crate) const fn from_kind(kind: WalletKind, trezor_account: u32) -> Self {
        match kind {
            WalletKind::Trezor => Self::Trezor {
                account: trezor_account,
            },
        }
    }

    pub(crate) fn validate(self) -> Result<()> {
        self.trezor_account_index().map(|_| ())
    }

    pub(crate) fn account_info(
        self,
        role: WalletRole,
        chain: ChainArg,
        address: Address,
    ) -> WalletAccount {
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
pub(crate) enum RelayConfig {
    WaitForRelayer { fallback: RelayFallbackConfig },
    SelfRelay { wallet: WalletConfig },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RelayFallbackConfig {
    SelfRelay { wallet: WalletConfig },
}

impl RelayConfig {
    pub(crate) const fn from_mode(
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

    pub(crate) const fn wallet(self) -> WalletConfig {
        match self {
            Self::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay { wallet },
            }
            | Self::SelfRelay { wallet } => wallet,
        }
    }

    pub(crate) fn validate(self) -> Result<()> {
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
pub(crate) enum ResolvedRelayFallback {
    SelfRelay { account: WalletAccount },
}

impl ResolvedRelayFallback {
    pub(crate) const fn account(self) -> WalletAccount {
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
pub(crate) enum RelayPolicyLabel {
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

pub(crate) struct RelaySignerRuntime<S> {
    pub(crate) signer: S,
    pub(crate) account: WalletAccount,
}

pub(crate) struct SourceSignerRuntime<S> {
    pub(crate) signer: S,
    pub(crate) account: WalletAccount,
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TrezorWalletService;

#[async_trait(?Send)]
pub(crate) trait WalletService {
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ResolvedRelay {
    WaitForRelayer { fallback: ResolvedRelayFallback },
    SelfRelay { account: WalletAccount },
}

impl ResolvedRelay {
    pub(crate) const fn from_config(config: RelayConfig, account: WalletAccount) -> Self {
        match config {
            RelayConfig::WaitForRelayer {
                fallback: RelayFallbackConfig::SelfRelay { .. },
            } => Self::WaitForRelayer {
                fallback: ResolvedRelayFallback::SelfRelay { account },
            },
            RelayConfig::SelfRelay { .. } => Self::SelfRelay { account },
        }
    }

    pub(crate) const fn mode(self) -> RelayMode {
        match self {
            Self::WaitForRelayer { .. } => RelayMode::WaitForRelayer,
            Self::SelfRelay { .. } => RelayMode::SelfRelay,
        }
    }

    pub(crate) const fn label(self) -> RelayPolicyLabel {
        match self {
            Self::WaitForRelayer { .. } => RelayPolicyLabel::WaitThenSelfRelay,
            Self::SelfRelay { .. } => RelayPolicyLabel::SelfRelay,
        }
    }

    pub(crate) const fn account(self) -> WalletAccount {
        match self {
            Self::WaitForRelayer { fallback } => fallback.account(),
            Self::SelfRelay { account } => account,
        }
    }

    pub(crate) const fn workflow_relay(self) -> WorkflowRelay {
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "tests use explicit expect messages to assert setup and validation invariants"
)]
mod tests {
    use super::*;
    use crate::config::RelayMode;
    use alloy::primitives::address;
    use alloy_chains::NamedChain;

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
}
