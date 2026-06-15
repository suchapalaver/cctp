# Production Rust Patterns

This repo is a Trezor-backed CCTP CLI that can move real funds. The Rust rules
below adapt production Rust guidance to this smaller CLI.

## Required Gates

Run these before committing Rust changes:

```sh
nix develop -c cargo fmt -- --check
nix develop -c cargo clippy --all-targets --locked -- -D warnings
nix develop -c cargo test --locked
```

Run `nix develop -c cargo fmt` to format changes.

## Error Handling

### No panics in production paths

Do not use `unwrap()` or `expect()` in production code. Use `?`,
`ok_or_else`, `bail!`, or `wrap_err` with operation-specific context.

Acceptable exceptions:

- test code, with an `expect(...)` message that names the invariant;
- code after explicit validation where a comment explains why failure is
  impossible.

### No silent error discard

Do not use `let _ = fallible_operation().await;` or `let _ = result;` for
fallible work. Propagate the error or handle it explicitly.

Ignoring a failure is acceptable only when all of these are true:

- the error was observed or logged;
- continuing is safe for funds, route state, and operator output;
- a comment explains the reason.

### Preserve context

When an external crate or system call fails, attach context naming the operation
that failed:

- reading or parsing config;
- resolving route or endpoint policy;
- initializing Trezor signers;
- checking provider chain IDs;
- resolving CCTP contracts and fast-transfer fees;
- sending approvals, burns, receives, or mints;
- polling receipts and attestations.

This repo currently uses `eyre`; if structured error enums are added later,
variant names should describe the operation, not the dependency crate.

## Option Discipline

`Option` is a type of last resort, not a default.

Use `Option` only when absence is a real, valid state at that program point:

- raw CLI/config fields before validation;
- optional recipient override before it defaults to the source signer;
- relay signer absence when using permissionless relayers;
- optional polling overrides;
- optional manual fast-transfer fee caps.

Do not use `Option` for:

- construction lifecycle, where a field starts as `None` and is filled later;
- variant-specific data that belongs in an enum variant;
- required config after `BridgeConfig` is built;
- hidden fallback behavior in signing, write, relay, fee, or transaction paths;
- values that are guaranteed by earlier validation.

Patterns to prefer:

- `let Some(value) = maybe_value else { return Ok(None); };` only when absence
  is the intended branch, such as no relay signer in permissionless mode.
- `ok_or_else(|| eyre!("..."))?` when absence is invalid.
- `map(...).transpose()?` for optional parsing where absence is valid but a
  present invalid value is an error.
- enums such as `RelayWalletConfig`, `RecipientConfig`, and
  `TransferRequest` when each variant has different data requirements.

Silent `None -> skip` is prohibited in funds-impacting paths. If a send, sign,
fee, receive, or validation step lacks required data, fail before touching the
wallet.

## Fallbacks And Defaults

Fallbacks are for operations that may legitimately fail, such as optional `.env`
loading or transient network retries. They are not for quietly supporting old
data shapes or missing required inputs.

Defaults are acceptable only when they are explicit product policy and visible
in provenance, such as the default route, wallet account, relay mode, transfer
mode, recipient fallback, or auto-resolved fast fee cap.

Required funds-impacting fields such as amount and RPC endpoints must fail fast
when missing.

## Strong Types

Use domain types instead of primitives at boundaries:

- `CctpV2Route` and `RouteConfig` for route and chain identity;
- `UsdcAmount` for six-decimal USDC values;
- `TransferMode` and `TransferRequest` for standard versus fast transfer
  policy;
- `RelayMode` and `RelayWalletConfig` for relay policy;
- `RpcEndpoints` for source/destination provider roles;
- `WalletAccount` for role, derivation path, chain binding, and address.

If a new value has distinct meaning or invariants, introduce a small type or
enum instead of reusing `String`, `u64`, `bool`, or `Address` everywhere.

## Boundary Rules

- Config resolution converts raw CLI/env/file input into `BridgeConfig` once.
- Route support lives in the route catalog.
- Endpoint role mapping follows the validated route.
- Wallet and provider setup happen after config validation.
- `BridgeWorkflow` sequences bridge operations. It does not parse config, read
  environment variables, choose routes, or render output.
- `BridgeApp` orchestrates services and is testable with mocks.
- Reporters render typed intents and outcomes.

## Real-Funds Safety

- Unsupported routes fail before wallet initialization.
- Chain ID mismatches fail before wallet initialization.
- The intent must show route, chain IDs, account roles, addresses, recipient,
  amount, transfer mode, fee cap policy, CCTP contracts, and relay policy before
  signing.
- Fast-transfer fee resolution fails closed when live fee data is unavailable
  or exceeds the cap.
- Testnet support must be opt-in and visually distinct from mainnet support.

## Comments And Lints

- Comments explain current behavior or invariants.
- Do not write version history in comments; git records history.
- Avoid `#[allow(dead_code)]`. If it is necessary, add a nearby comment that
  explains why the code is intentionally reserved.
- Keep helper abstractions small and tied to actual duplication or safety
  boundaries.

## Pre-Commit Checklist

- No production `unwrap()` or `expect()` without documented validation.
- No silent `let _ =` on fallible work.
- `Option` means legitimate absence, not incomplete construction.
- Required values are validated before wallet/provider/signing setup.
- Domain primitives use strong types or enums.
- User-facing safety output still exposes route, chain, account, amount, fee,
  and relay policy.
- `fmt`, `clippy -D warnings`, and tests pass through `nix develop`.
