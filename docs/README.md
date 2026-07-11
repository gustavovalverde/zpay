# zpay Documentation

The docs are organised by lifecycle, not by topic:

- **`product-requirements.md`** is the whole-product PRD: problem, positioning,
  capability requirements by surface, milestones, open questions.
- **`architecture/`** holds living boundary contracts. Edit them in place when
  the contract changes; cite them from ADRs and PRs.
- **`adrs/`** holds accepted decisions. Numbered, present-tense, never reused.
  A new decision that supersedes an older one gets a fresh number.
- **`proposals/`** holds asks against upstream sibling repos (zally, zinder,
  fauzec, zexplorer). Header carries `Consumer:` and `Pinned at:` rows. The
  proposal stays after the upstream accepts; it does not become a zpay ADR.
- **`plans/`** holds executable phase-by-phase sequences. Plans cite ADRs by
  number; they do not invent architecture. After ship, archive under
  `docs/archive/plans/YYYY-MM-DD-<slug>.md` or delete.
- **`reference/`** holds typed registries (error vocabulary, capability
  surface). Updated whenever a new entry lands.
- **`runbooks/`** holds operational procedures with explicit commands.

## ADR index

| # | Title | Status | Domain |
|---|-------|--------|--------|
| [0001](adrs/0001-workspace-and-crate-boundaries.md) | Workspace and crate boundaries | Accepted | Project structure |
| [0002](adrs/0002-zally-embedded-as-library.md) | Zally embedded as library | Accepted | Wallet plane |
| [0003](adrs/0003-zinder-as-chain-plane.md) | Zinder as chain plane source of truth | Accepted | Chain plane |
| [0004](adrs/0004-libsql-prepared-tx-cache.md) | libSQL for prepared-tx cache and ledger | Accepted | Persistence |
| [0005](adrs/0005-protocol-neutral-core-with-wire-adapters.md) | Protocol-neutral core with per-wire adapters | Accepted | Facilitator surface |
| [0006](adrs/0006-facilitator-trust-boundary.md) | Facilitator trust boundary and settle-vs-verify split | Accepted | Facilitator surface |
| [0007](adrs/0007-local-zip311-verifier.md) | Local ZIP-311 verifier | Accepted | Verify plane |
| [0008](adrs/0008-compliance-authority-placement.md) | Compliance authority placement | Accepted | Compliance |
| [0009](adrs/0009-settlement-lifecycle-and-finality.md) | Settlement lifecycle and finality semantics | Accepted | Settlement lifecycle |
| [0010](adrs/0010-x402-public-boundary.md) | x402 public boundary | Accepted | Facilitator surface |
| [0011](adrs/0011-zcash-x402-exact-binding.md) | Zcash x402 exact binding | Accepted | Facilitator surface |
| [0012](adrs/0012-testkit-agent-payment-client.md) | Testkit owns agent payment client fixtures | Accepted | Dev and test client infrastructure |
| [0013](adrs/0013-shared-dpop-primitives.md) | Shared DPoP primitives | Accepted | DPoP verification |
| [0014](adrs/0014-operator-payments-console.md) | Operator payments console boundary | Accepted | Operator surface, settlement ledger schema |

## Architecture index

- [public-interfaces.md](architecture/public-interfaces.md): vocabulary spine
- [operational-surfaces.md](architecture/operational-surfaces.md): readiness
  probe, ops listener, metrics, env-var schema
- [facilitator-plane.md](architecture/facilitator-plane.md): prepare, settle,
  confirm, verify lifecycle
- [upstream-platform-binding.md](architecture/upstream-platform-binding.md):
  what zpay expects from zally, zinder, and zentity

## Reference index

- [error-vocabulary.md](reference/error-vocabulary.md): every typed error with
  retry posture and operator action

## Runbook index

- [end-to-end-validation.md](runbooks/end-to-end-validation.md): local zpay
  lifecycle smoke test with zpay, zinder, fauzec, and zexplorer
- [demo-ui.md](runbooks/demo-ui.md): browser checkout demo for checkout and
  autopay Zcash flows
- [railway-deploy.md](runbooks/railway-deploy.md): Railway deployment
- [reorg-recovery.md](runbooks/reorg-recovery.md): settlement reorg response
- [zspend-seed.md](runbooks/zspend-seed.md): wallet seed lifecycle

## Proposals index

| # | Title | Upstream |
|---|-------|----------|
| [0001](proposals/0001-zally-payment-request-to-uri.md) | zally `PaymentRequest::to_uri()` | zally |
| [0002](proposals/0002-zinder-zip311-verifier.md) | zinder ZIP-311 verifier | zinder |
| [0003](proposals/0003-agent-wallet-production-architecture.md) | Agent wallet production architecture | zally |
| [0003](proposals/0003-fauzec-bearer-captcha-mode.md) | fauzec `CaptchaMode::Bearer` | fauzec |
| [0004](proposals/0004-zexplorer-per-txid-watch.md) | zexplorer per-txid watch route | zexplorer |
| [0005](proposals/0005-zentity-mcp-zcash-purchase.md) | zentity MCP `purchase` tool `scheme: "zcash"` | zentity |
| [0006](proposals/0006-zally-zip311-disclosure-production.md) | zally ZIP-311 payment-disclosure production | zally |

See also the upstream-asks list in
[product-requirements.md §Cross-project asks](product-requirements.md#cross-project-asks).
