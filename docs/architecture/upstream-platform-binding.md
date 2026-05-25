# Upstream Platform Binding

zpay depends on four sibling repositories. This document specifies what zpay
expects from each upstream, the version-pinning policy, and the upstream
asks that are currently open as proposals.

## zally

[github.com/gustavovalverde/zally](https://github.com/gustavovalverde/zally)

Role: Rust wallet library. zpay's only library-shaped wallet dependency.

zpay depends on:

| Crate | Used by | For |
|-------|---------|-----|
| `zally-core` | `zpay-core` | `Zatoshis`, `TxId`, `Memo`, `Network`, `PaymentRecipient`, `IdempotencyKey` newtypes. |
| `zally-keys` | `zpay-runtime` | `SeedSealing` trait + `AgeFileSealing` impl for the operator wallet. |
| `zally-chain` | `zpay-core` | `ZinderChainSource` for the operator wallet's view of chain state. |
| `zally-wallet` | `zpay-core`, `zpay-runtime` | `Wallet`, `Wallet::propose`, `PaymentRequest::from_uri`, `PaymentRequest::to_uri` (proposal). |

Pin: workspace `Cargo.toml` carries a git rev under
`workspace.dependencies`. Bumps land as their own PR with a one-line
upstream reference in the body.

Open ask: `PaymentRequest::to_uri(&self) -> String`. PRD-42 Phase 2.
Tracked at [docs/proposals/0001-zally-payment-request-to-uri.md](../proposals/0001-zally-payment-request-to-uri.md)
(filed in zpay; proposal is to add the method to zally).

## zinder

[github.com/gustavovalverde/zinder](https://github.com/gustavovalverde/zinder)

Role: Zcash indexer. zpay's only chain-plane dependency.

zpay depends on:

| Crate | Used by | For |
|-------|---------|-----|
| `zinder-client` | `zpay-core` | `RemoteChainIndex::broadcast_transaction`, `ChainEvents`, `MempoolEvents`. |
| `zinder-client` | `zpay-core::oracle` | `VerifyPaymentDisclosure` via the ExplorerQuery gRPC. |

Pin: workspace `Cargo.toml` carries a git rev. Bumps land as their own PR.

Open ask: ZIP-311 verifier inside the existing
`explorer.payment_disclosure.verify_v1` capability. PRD-42 Phase 2.
Tracked at [docs/proposals/0002-zinder-zip311-verifier.md](../proposals/0002-zinder-zip311-verifier.md).

## fauzec

[github.com/gustavovalverde/fauzec](https://github.com/gustavovalverde/fauzec)

Role: Testnet faucet. zpay does not depend on fauzec at the Rust level;
the relationship is operational: zpay's testnet smoke tests claim TAZ
from fauzec to fund agent-payment flows.

Pin: none (no crate dependency). zpay's integration tests refer to a
fauzec HTTP endpoint configured via `ZPAY_TEST_FAUCET_HTTP_ADDR`.

Open ask: `CaptchaMode::Bearer` variant so zpay's test harness can claim
testnet ZEC without a captcha. PRD-42 Phase 1. Tracked at
[docs/proposals/0003-fauzec-bearer-captcha-mode.md](../proposals/0003-fauzec-bearer-captcha-mode.md).

## zexplorer

[github.com/gustavovalverde/zexplorer](https://github.com/gustavovalverde/zexplorer)

Role: Chain-read BFF. zpay uses zexplorer only as a fallback confirmation
oracle when local zinder is not reachable.

Pin: none (no crate dependency; HTTP only). The fallback endpoint is
configured via `ZPAY_NODE__FALLBACK_EXPLORER_HTTP_ADDR`.

Open ask: `POST /api/v1/{network}/transactions/{txid}/watch` endpoint
with Redis-backed watch state and HMAC-signed callback delivery. PRD-42
Phase 2. Tracked at [docs/proposals/0004-zexplorer-per-txid-watch.md](../proposals/0004-zexplorer-per-txid-watch.md).

## zentity

[github.com/gustavovalverde/zentity](https://github.com/gustavovalverde/zentity)

Role: Identity issuer. Issues the PoH SD-JWT-VC tokens zpay validates
inside the x402 / MPP compliance extension.

Pin: none (no crate dependency; HTTP only). The JWKS endpoint URL is
configured via `ZPAY_COMPLIANCE__JWKS_URL`.

What zpay expects from zentity:

- `GET https://app.zentity.xyz/api/auth/oauth2/jwks` returns a JWKS
  document containing at least one EdDSA signing key.
- PoH tokens are SD-JWT-VC format with the claims
  `{ verification_level, verified, sybil_resistant, merchant_sub, aud,
  cnf.jkt, exp, iss }`.
- The `aud` claim matches the merchant origin zpay was prepared for.
- The `cnf.jkt` claim matches the DPoP JKT recorded at prepare time.
- The `iss` claim matches an entry in `ZPAY_COMPLIANCE__ACCEPTED_ISSUERS`.

Open ask: zentity's MCP `purchase` tool gains a `scheme: "zcash"` branch
calling zpay over HTTPS. PRD-42 Phase 6. Tracked at
[docs/proposals/0005-zentity-mcp-zcash-purchase.md](../proposals/0005-zentity-mcp-zcash-purchase.md).

## Version-pinning policy

- Sibling Rust workspaces (zally, zinder) are pinned by git rev in
  `Cargo.toml`. Bumps land in their own PR; the body cites the upstream
  change rev and the reason for the bump.
- HTTP-only dependencies (fauzec, zexplorer, zentity) are configured
  via env vars; no version pin in code. Compatibility is asserted by
  contract tests in `tests/integration/`.
- Patches: the only `[patch.crates-io]` entry is `core2`, mirroring
  every other sibling Rust project's pin to the bbqsrc/core2 fork until
  Zebra publishes a clean resolution path.

## Breaking-change protocol

When a sibling upstream ships a breaking change to a surface zpay
depends on:

1. The upstream PR includes a note naming zpay as a downstream consumer.
2. zpay opens a tracking issue against the upstream change.
3. zpay's `Cargo.toml` rev or `ZPAY_*` config field bumps land in a
   separate PR that names the upstream change.
4. If the change is irreversible, the downstream PR carries a section
   in the body explaining the deprecation path.

In the other direction (zpay's HTTP API changes), zpay follows its own
breaking-change protocol: every breaking change ships as a new version
suffix in the capability string (`x402.v3.*`), and the old capability is
retired with a sunset period of at least 90 days.
