# AGENTS.md

This file provides coding-agent guidance for this repository.

## Build And Verification

Use the Nix development shell for local checks:

```sh
nix develop -c cargo fmt -- --check
nix develop -c cargo clippy --all-targets --locked -- -D warnings
nix develop -c cargo test --locked
```

Use `nix develop -c cargo fmt` before committing Rust changes.

## Commit And Release Policy

- Commits and release tags must be signed.
- Do not push unsigned commits.
- Before handing off a pushed commit, verify local GPG status with
  `git log --show-signature --oneline -1` and GitHub verification with
  `gh api`.
- Release only when there is a clear user-facing reason. Internal refactors can
  wait for the next functional release.

## Rust Conventions

- Use `dotenvy`, not `dotenv`.
- Use named string interpolation, for example `format!("{name} is {value}")`.
- Never use `unwrap()` or `expect()` in production paths unless prior validation
  makes success impossible to miss and a comment explains the invariant.
- Tests may use `expect(...)`, but the message must state the invariant being
  asserted.
- Never silently discard errors with `let _ =`. Propagate with `?`, handle with
  logging/reporting, or document why discarding is safe.
- Use `eyre::WrapErr` at fallible external boundaries so errors name the failed
  operation: config loading, RPC parsing, provider checks, signer setup, fee
  resolution, transaction submission, attestation polling, and minting.
- Use domain types at boundaries. Prefer `CctpV2Route`, `UsdcAmount`,
  `TransferMode`, `RouteConfig`, `RpcEndpoints`, `WalletAccount`, and explicit
  enums over primitive strings and booleans.

## Option Discipline

`Option` is a type of last resort, not a default.

Use `Option` only when a value may legitimately be absent at that point in the
program, such as raw CLI/config inputs, optional recipient override,
permissionless relayer mode with no destination signer, or optional polling
overrides.

Do not use `Option` for:

- construction order where a field starts empty and is filled later; use a
  builder state or a fully validated config type instead;
- variant-specific data; use enums with variant-specific fields, such as
  `RelayWalletConfig`;
- required values after config resolution; convert absence to a clear error with
  `ok_or_else` before wallet, provider, signing, or transaction setup;
- silent `None -> skip` behavior in write, signing, fee, relay, or transaction
  paths.

When absence is valid, make the policy explicit in the type or method name. When
absence is invalid, fail early with route/config context.

## CCTP CLI Boundaries

- Raw CLI/env/file input is resolved once by the config service into an
  immutable `BridgeConfig`.
- Route support policy lives in the route catalog. Workflow code consumes a
  validated route and must not hard-code route support.
- RPC endpoint roles should follow the validated route.
- Wallet and provider setup happen after route/config validation.
- `BridgeWorkflow` owns transaction sequencing only. It should not parse config,
  read environment variables, or render user output.
- `BridgeApp` owns application orchestration and receives services/adapters
  through boundaries so tests can use mocks.
- Reporters render typed intents and outcomes. New output modes should not leak
  presentation decisions into workflow, wallet, or provider code.

## Real-Funds Safety

- Unsupported routes and chain ID mismatches must fail before wallet
  initialization.
- The bridge intent must show route, chain IDs, accounts, recipient, amount,
  transfer mode, fee cap policy, contracts, and relay policy before any signing
  prompt.
- Fast transfer fee data must be resolved before signing and fail closed when
  unavailable or above the configured cap.
- Defaults are allowed only when they are an explicit product decision and are
  visible in provenance. Required funds-impacting fields should fail fast.
- Testnet support must be explicit and visibly distinct from mainnet support.

## Comments And Lints

- Comments describe current behavior and invariants, not version history.
- `#[allow(dead_code)]` requires a nearby comment explaining why the code is
  intentionally reserved.
- Keep changes scoped to the issue or task. Do not mix release changes,
  refactors, and feature work unless the task requires it.
