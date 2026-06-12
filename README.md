# cctp

Small Trezor-backed CLI for bridging USDC with
[`cctp-rs`](https://crates.io/crates/cctp-rs).

The first supported route is Ethereum mainnet to HyperEVM. The CLI uses Alloy's
Trezor signer support and defaults to waiting for any permissionless relayer to
complete the destination mint.

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
`cargo publish --dry-run`; it skips the final publish step.

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

The CLI also loads `.env` from the current directory or a parent directory
before resolving configuration. Keep real RPC URLs in local `.env`;
`.env.example` documents the supported variable names and `.env` is ignored by
git.

By default this sends standard-finality CCTP v2 transactions. To request fast
finality, provide an explicit fee cap:

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

Before any signing prompt, the CLI verifies both RPC providers report the
expected chain IDs, resolves the CCTP contracts, and prints a bridge intent with
the active Trezor account roles, derivation paths, chain bindings, addresses,
amount, fee cap, approval spender, destination MessageTransmitter, and relay
policy. A live run requires typing `CONFIRM` after reviewing that intent. Use
`--dry-run` to render the same intent without sending transactions, or `--yes`
for explicit non-interactive automation.

## Configuration

Configuration is treated as a service boundary. Raw CLI/env input is resolved
once into a validated `BridgeConfig`; execution code consumes that immutable
config instead of reading flags or environment variables directly.

Precedence is:

1. CLI flags.
2. Environment variables for RPC URLs: `ETHEREUM_RPC_URL` and
   `HYPEREVM_RPC_URL`.
3. TOML config file passed with `--config`.
4. Built-in defaults for route, wallet, account, relay mode, and transfer mode.

`amount`, `ethereum_rpc`, and `hyperevm_rpc` must be supplied by CLI, env, or
config file. Example:

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

Run with:

```sh
cctp bridge --config cctp.toml --amount 25
```

Local config files can contain RPC URLs with API keys. Keep those files local:
`cctp.toml`, `cctp.local.toml`, `*.local.toml`, `.env`, and `.env.*` are ignored
by git. Commit only sanitized examples.

Domain primitives are shared with `cctp-rs` where they belong. The CLI uses
`CctpV2Route` for route validation and `UsdcAmount` for six-decimal USDC amount
parsing. Wallet backends, RPC endpoints, dry-run behavior, and relay policy stay
in the CLI because they are application concerns.
