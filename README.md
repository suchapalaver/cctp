# cctp

Small Trezor-backed CLI for bridging USDC with
[`cctp-rs`](https://crates.io/crates/cctp-rs).

The default supported production route is Ethereum mainnet to HyperEVM. The CLI
also has an explicit Ethereum Sepolia to Base Sepolia testnet route for dry-run
and test-funds validation before operational use. The CLI uses Alloy's Trezor
signer support and defaults to waiting for any permissionless relayer to complete
the destination mint.

## Install

```sh
cargo install cctp
```

The source repository, published crate, and installed command are all named
`cctp`.

## Development

This repository includes a Nix flake for a reproducible Rust development shell:

```sh
nix develop
```

Run the standard local checks through the shell:

```sh
nix develop -c cargo fmt -- --check
nix develop -c cargo clippy --all-targets --locked -- -D warnings
nix develop -c cargo test --locked
```

Agent and Rust production rules for this CLI are documented in
[`AGENTS.md`](AGENTS.md) and [`docs/production-rust.md`](docs/production-rust.md).

## Release

Releases publish to crates.io from `.github/workflows/release.yml` using crates.io
Trusted Publishing. The `cctp` crate owner must configure this on crates.io
before pushing a release tag:

1. Open the `cctp` crate on crates.io.
2. Go to Settings -> Trusted Publishing.
3. Add a GitHub publisher with:
   - Repository owner: `suchapalaver`
   - Repository name: `cctp`
   - Workflow filename: `release.yml`
   - Environment: leave blank unless the GitHub workflow is updated to use one.

To verify the workflow without publishing, run the Release workflow manually from
GitHub Actions. The manual path runs tests, clippy, package listing, and
`cargo publish --dry-run`; it skips crates.io OIDC authentication and the final
publish step. The tag path is the only workflow path that exchanges GitHub's
OIDC token for a short-lived crates.io token.

To publish, bump `Cargo.toml`, commit the release, then push a `v*` tag. The tag
workflow exchanges GitHub's OIDC token for a short-lived crates.io token and runs
`cargo publish --locked`. After the first Trusted Publishing tag release
succeeds, remove the old `CARGO_REGISTRY_TOKEN` repository secret from GitHub.

## Usage

```sh
export ETHEREUM_RPC_URL="https://..."
export HYPEREVM_RPC_URL="https://..."

cctp bridge \
  --amount 10.25 \
  --recipient 0x0000000000000000000000000000000000000000
```

For testnet validation, choose the testnet route explicitly and provide matching
testnet RPC endpoints:

```sh
export ETHEREUM_SEPOLIA_RPC_URL="https://..."
export BASE_SEPOLIA_RPC_URL="https://..."

cctp bridge \
  --from ethereum-sepolia \
  --to base-sepolia \
  --amount 1 \
  --dry-run
```

The testnet route defaults to Circle's Ethereum Sepolia USDC address
`0x1c7D4B196Cb0C7B01d743Fbc6116a902379C7238`. Use Circle's faucet for testnet
USDC and ensure the selected Trezor account has Sepolia ETH for gas. Add
`--self-relay` only when the same account also has Base Sepolia ETH for the
destination `receiveMessage` transaction.

The CLI also loads `.env` from the current directory or a parent directory
before resolving configuration. Keep real RPC URLs in local `.env`;
`.env.example` documents the supported variable names and `.env` is ignored by
git.

By default this sends standard-finality CCTP v2 transactions. To request fast
finality, pass `--fast`:

```sh
cctp bridge \
  --amount 10.25 \
  --fast
```

For fast transfers, the CLI fetches the live route fee from CCTP before any
signing prompt, prints the fee and cap in the bridge intent, and fails closed if
the fee cannot be resolved. When `--max-fee-usdc` is omitted, the CLI uses the
live fee plus a 20% buffer as the transaction `maxFee`.

To provide a manual cap, pass `--max-fee-usdc`. The CLI still fetches the live
fee first and rejects the run if the manual cap is below the current required
fee:

```sh
cctp bridge \
  --amount 10.25 \
  --fast \
  --max-fee-usdc 0.01
```

By default the CLI waits for any relayer to complete the mint on HyperEVM. It
uses a read-only HyperEVM provider and does not initialize a destination signer
or require HyperEVM gas.

To self-relay, add `--self-relay`; the relay account must hold HyperEVM gas.
The relay signer defaults to `--trezor-account`, but can be selected
independently with `--relay-trezor-account`.

Supported routes are explicit CLI catalog entries:

| From | To | Notes |
| --- | --- | --- |
| `ethereum` | `hyperevm` | Ethereum mainnet to HyperEVM CCTP v2. |
| `ethereum-sepolia` | `base-sepolia` | Explicit testnet route for dry-run and test-funds validation. |

Unsupported routes fail during config resolution before wallet initialization.

## HyperEVM, Not HyperCore

This CLI's first route is Ethereum mainnet -> HyperEVM. It does not implement
HyperCore forwarding. The current bridge path is a normal EVM CCTP v2 transfer:
it does not build hook payloads, does not target a HyperCore forwarder, and does
not set a destination-caller requirement.

If HyperCore support is added later, it should use a separate route/workflow type
instead of extending the default HyperEVM workflow. That design needs to account
for hook payload construction, forwarder recipient/caller requirements, and
whether the shared `cctp-rs` layer should expose destination-caller control.

Before any signing prompt, the CLI verifies both RPC providers report the
expected chain IDs, resolves the CCTP contracts, and prints a bridge intent with
the active Trezor account roles, derivation paths, chain bindings, addresses,
amount, resolved fast-transfer fee and cap when applicable, approval spender,
destination MessageTransmitter, and relay policy. A live run requires typing
`CONFIRM` after reviewing that intent. Use `--dry-run` to render the same intent
without sending transactions, or `--yes` for explicit non-interactive
automation.

The bridge intent also prints provenance for high-impact configuration values,
including route, amount, recipient, wallet accounts, relay mode, fast mode, fee
cap, and RPC endpoint roles. RPC endpoints are redacted to scheme, port, and a
masked host suffix so API keys in host labels, paths, usernames, passwords, or
query strings are not shown.

## Configuration

Configuration is treated as a service boundary. Raw CLI/env input is resolved
once into a validated `BridgeConfig`; execution code consumes that immutable
config instead of reading flags or environment variables directly.

Precedence is:

1. CLI flags.
2. Environment variables for RPC URLs: `ETHEREUM_RPC_URL`,
   `HYPEREVM_RPC_URL`, `ETHEREUM_SEPOLIA_RPC_URL`, and
   `BASE_SEPOLIA_RPC_URL`.
3. TOML config file passed with `--config`.
4. Built-in defaults for route, wallet, account, relay mode, and transfer mode.

`amount` and the RPC fields for the selected route must be supplied by CLI, env,
or config file. The default production route requires `ethereum_rpc` and
`hyperevm_rpc`. Example:

```toml
amount = "10.25"
ethereum_rpc = "https://..."
hyperevm_rpc = "https://..."
recipient = "0x0000000000000000000000000000000000000000"
trezor_account = 0
fast = false
self_relay = false
dry_run = false
```

The explicit testnet route uses route-specific RPC fields:

```toml
from = "ethereum-sepolia"
to = "base-sepolia"
amount = "1"
ethereum_sepolia_rpc = "https://..."
base_sepolia_rpc = "https://..."
dry_run = true
```

Run with:

```sh
cctp bridge --config cctp.toml --amount 25
```

Local config files can contain RPC URLs with API keys. Keep those files local:
`cctp.toml`, `cctp.local.toml`, `*.local.toml`, `.env`, and `.env.*` are ignored
by git. Commit only sanitized examples.

Domain primitives are shared with `cctp-rs` where they belong. The CLI route
catalog decides which `CctpV2Route` values this application supports, and
`UsdcAmount` handles six-decimal USDC amount parsing. Adding another supported
route should be localized to the catalog, config tests, and this documentation.
Wallet backends, RPC endpoints, dry-run behavior, and relay policy stay in the
CLI because they are application concerns.
