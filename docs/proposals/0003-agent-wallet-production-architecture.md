# Proposal-0003: Agent-Bound Wallet Runtime, Production Architecture

| Field | Value |
| ----- | ----- |
| Status | Proposed (locked design) |
| Repos | zally, zpay (gains `zspend-core` + `zspend-runtime` crates), zentity (apps/web, apps/demo-rp), fauzec |
| Supersedes | none |
| Related | ADR-0001, ADR-0002, ADR-0005, ADR-0006, ADR-0007; zentity RFC-0040, RFC-0048 |

## 1. Thesis

We are shipping a production-grade agent-bound wallet runtime that lets an AI agent execute a Zcash payment under a bounded, user-approved authorization without ever holding the user's spending intent in plaintext on a server it does not control. The wallet runs as its own service (`zspend-runtime`, a sibling binary inside the zpay workspace), the facilitator (`zpay-runtime`) is broadcaster-only, the identity issuer (`zentity`) carries the authorization in an OAuth `at+jwt` whose `authorization_details` is the spend contract, and the on-chain artifact moves across the wire as a PCZT. The single architectural commitment that makes this copyable: the access token IS the bounded spend grant (RAR plus DPoP plus `act.sub`), the issuer is the sole policy authority, the wallet is a constrained signer, and the facilitator is a constrained broadcaster. Chain specifics live behind a CAIP-typed envelope so an integrator porting to Solana, Stellar, or EVM rewrites adapters, not the spine.

## 2. The production scenario the architecture serves

A user opens Aether AI in `apps/demo-rp` and asks it to buy a 0.5 ZEC reference token from a merchant. The agent (running inside the demo-rp BFF process for now, then any MCP host later) calls `POST /x402/v2/prepare` on the facilitator, receives a `payment_request` plus a `wallet_endpoint` for routing and a `payee_policy` describing whether the flow is agent-signed or external. The BFF asks the identity issuer for backchannel authorization with a structured `payment_authorization` entry: chain, recipient, amount, and an `intent_hash` over the parsed payment tuple. The issuer evaluates the user's `payment_authorization:sign` capability against the live usage ledger, refuses the mint outright if the daily cap would be exceeded, otherwise either auto-approves under existing constraints or pushes a structured spend card to the user's device. On approval, the issuer mints a single-use, DPoP-bound, audience-bound access token whose `authorization_details[0]` is the verbatim RAR entry. The agent attaches that token plus a fresh DPoP proof to `POST /v1/payments/sign` on the wallet runtime, the wallet verifies the cryptographic envelope (DPoP, signature, audience, intent hash, recipient match, amount cap, single-use `jti`), constructs and signs a PCZT via the embedded zally library, marks the PCZT extractor-ready, and returns it. The agent forwards the PCZT to `POST /x402/v2/settle`, the facilitator extracts and broadcasts via its `Submitter`, and the SSE stream on `/payments/{id}/events` carries the broadcast and confirmation snapshots back to the demo-rp page. The user sees a receipt that links straight to `zexplorer.app/testnet/tx/<tx_id>`.

```
User       demo-rp BFF      zentity (issuer)     zspend (wallet)     zpay         zinder
  |             |                  |                    |              |             |
  |--ask buy--->|                  |                    |              |             |
  |             |--POST /prepare----------------------->|              |             |
  |             |<--{payment_request, wallet_endpoint, payee_policy}---|             |
  |             |--POST /oauth2/bc-authorize (RAR)----->|              |             |
  |<------------|--push approval card (signed RAR)------|              |             |
  |--approve--->|                  |                    |              |             |
  |             |--CIBA poll----->ok                    |              |             |
  |             |<--at+jwt (cnf.jkt, act.sub, RAR, jti)-|              |             |
  |             |--POST /v1/payments/sign (DPoP)------->|              |             |
  |             |                  |             verify+sign           |             |
  |             |<------signed_payload (PCZT, tx_id, expires_at)-------|             |
  |             |--POST /settle (signed_payload)----------------------->|             |
  |             |                  |                    |     extract+submit-------->|
  |             |<--SSE: broadcast--------------------------------------|             |
  |             |<--SSE: confirmed--------------------------------------|<--inclusion-|
  |<-receipt----|                  |                    |              |             |
```

## 3. Architectural decisions (locked)

### D-1: The access token is the spend grant; RAR is the policy carrier

**Why.** The IETF stack already names this pattern: RFC 9396 `authorization_details`, RFC 9068 `at+jwt`, RFC 9449 DPoP, RFC 8693 + `draft-oauth-ai-agents-on-behalf-of-user-02` for `act.sub`. Stripe (Shared Payment Tokens), Coinbase (Agentic Wallets), and Google AP2 all converge on "structured object scopes spend, scope strings do not." Adopting this verbatim means integrators copy a standard, not a Zentity invention.

**How to apply.** Every spend authorization MUST be represented as a `payment_authorization` entry inside `authorization_details`. Scope strings never carry amount, recipient, or expiry semantics. The wallet rejects any access token without a matching RAR entry. Any future authorization type (subscription, escrow, refund) is a new RAR `type`, not a new scope.

### D-2: The issuer is the sole capability-policy authority

**Why.** Defense-in-depth that distributes policy across three layers produces drift: three repos disagree on whether a recipient is allowed, telemetry splits, integrators copy whichever layer they reach first. Capability state already lives in `agent_session_grant` and `capability_usage_ledger`; the only place that can atomically read-and-write usage is the issuer.

**How to apply.** `evaluateSessionGrants` runs synchronously inside the CIBA grant handler. The ledger write is the check: `INSERT ... WHERE running_total + ? <= daily_limit` returning success or failure. If the issuer refuses the mint, the agent sees `capability_exhausted` from CIBA, never from the wallet. The wallet's local `usage_ledger` is post-sign audit and idempotency only; it never feeds back to the issuer.

### D-3: PCZT (ZIP-48 v1) is the signed artifact, wrapped in a chain-neutral envelope on the wire

**Why.** PCZT splits Constructor, Signer, Combiner, and Extractor cleanly, keeps chain-tip math in zpay where zinder lives, and extends to shielded sends without a wire revision. Naming the wire field `pczt` directly would force every non-Zcash copier to lie in logs and SDKs forever.

**How to apply.** The wire field is `signed_payload`. It is an object: `{ format: "pczt-v1" | <future>, bytes: <base64>, tx_id, fee, expires_at, metadata: { chain_specific: {...} } }`. The Rust type is `SignedPayload`. PCZT is one value of `format`. The wallet returns the PCZT in extractor-ready state (proprietary field `"zentity.final": true`, all inputs signed with SIGHASH_ALL only). zpay's role is Extractor plus broadcast; it refuses any PCZT that is not extractor-ready.

### D-4: Intent binding hashes the parsed tuple, not the URI text

**Why.** Two canonicalizers across language boundaries (TypeScript issuer, Rust wallet) is a vulnerability factory. ZIP-321 allows parameter reordering, optional fields, percent-encoding variance; a one-byte canonicalization drift either DoSes every spend or accepts an attacker's URI that re-parses differently than the issuer evaluated.

**How to apply.** `intent_hash = SHA-256("zentity.payauth.v1" || chain_namespace || chain_reference || recipient_caip10 || amount_value_be || amount_unit || payment_id || expiry_height_be)`. Both issuer and wallet parse first, hash second. The hash is versioned on the wire as `"v1:sha256:<base64url>"`. Conformance vectors ship in `zally-testkit`, and a TypeScript port lives in `apps/web/src/lib/agents/intent-hash.ts` with the same vectors as a test.

### D-5: Audience binding is a JWK thumbprint, not a URL

**Why.** A URL-typed `aud` lets a compromised BFF or DNS path silently re-target a token at an attacker-controlled host. A wallet fleet with multiple instances under one DNS name has no per-instance identity unless the key is the identity.

**How to apply.** Each wallet instance registers its `aud` JWK thumbprint with the issuer per environment. The issuer mints `aud = <wallet_jkt>`. The wallet rejects any token whose `aud` does not equal its own thumbprint. `wallet_endpoint` in `/prepare` is routing metadata only; it carries no security weight.

### D-6: Tokens are single-use, short-lived, and revocation is delta-streamed

**Why.** Long-lived tokens with eventual-consistency revocation give a compromised agent a measurable spending window. A 10-second poll over a 24-hour token is a 10-second blank check; a 10-second poll over a 60-second token is closed-loop.

**How to apply.** Access tokens for `payment_authorization` carry `exp - iat <= 120s` and a single `jti`. The wallet maintains a revocation cache from `/api/auth/oauth2/revoked?since=<ts>`. The poll interval is operator-tunable with a hard upper cap of 30s enforced by `zspend-core` config validation. The wallet fails closed if the cache is older than `2 x poll_interval`. `/readyz` reports `revocation_staleness_seconds` and `/metrics` exposes the gauge.

### D-7: The CIBA approval card is signed, not relayed

**Why.** If the BFF sends one RAR to the issuer and renders a different card to the user, the user's tap approves the wrong spend. The signed surface must equal the displayed surface.

**How to apply.** The push notification payload is the verbatim RAR entry signed by the issuer. The service worker (`public/push-sw.js`) verifies the signature against the cached issuer JWKS before rendering the card. The "Approve" action returns the `intent_hash` to the issuer; the issuer refuses to mint if the returned hash does not match the RAR it would otherwise sign.

### D-8: Idempotency is `jti`-keyed and write-then-sign

**Why.** Horizontal scaling of the wallet runtime with per-instance idempotency caches lets a network blip and a retry produce two signed PCZTs for one authorization. zpay's `(jkt, idempotency_key)` collapse at `/settle` saves the broadcast race but leaves a second signed artifact floating.

**How to apply.** `usage_ledger` is a shared backend (Turso replication or a single libSQL primary). The `jti` insert is the atomic gate: `INSERT INTO usage_ledger (jti, intent_hash, signed_payload) VALUES (?, ?, ?) ON CONFLICT (jti) DO NOTHING RETURNING signed_payload`. If the insert returns the existing row, the wallet returns that signed payload. The wallet never signs without first claiming the `jti`.

### D-9: The wallet is a constrained signer; zpay is a constrained broadcaster; ADR-0002 holds

**Why.** Linking seed code into zpay collapses the trust boundary the architecture depends on. Letting the wallet broadcast directly creates a second chain-tip dependency to maintain and a second place where reorg and expiry math have to agree.

**How to apply.** The wallet implements `Submitter` only for the PCZT round-trip path (sign-and-return) and never broadcasts. zpay implements `Submitter` against zinder and never signs. Both depend on zally for the canonical trait definition (the wallet library is where wallet contracts belong; a third workspace crate solely to "share" them was considered and rejected). Marker types in zally separate the sign-only and broadcast-only obligations.

### D-10: Chain identifiers are CAIP-typed; amounts are decimal-string + unit

**Why.** Baking `_zat`, `network: "mainnet"`, or a freeform string-typed network into the wire schema makes the first non-Zcash adapter a rewrite of every consumer. Multi-chain fleets need typed routing without string parsing.

**How to apply.** RAR carries `chain: { namespace: "zcash", reference: "main" | "test" }` (CAIP-2). `recipient` is CAIP-10. `amount: { currency: "ZEC", value: "50000000", unit: "base" }` where `value` is a decimal string and `unit` is `"base"` or `"display"`. Capability `actionParams` mirror these. `_zat` survives only inside zally and the Zcash adapter in `zspend`.

### D-11: `payment_request` is scheme-tagged; canonicalization is per-scheme

**Why.** ZIP-321, Solana Pay, SEP-0007, and EIP-681 are different grammars. A single field name pretending they are interchangeable will rot the moment a second scheme lands.

**How to apply.** The wire object is `payment_request: { scheme: "zip321", value: "<scheme-specific string>" }`. Each scheme has its own canonicalizer behind a trait. The portable rule is `intent_hash` always being computed over the parsed tuple (D-4), so the scheme can vary without changing the binding algorithm.

### D-12: Discovery is first-class; errors carry remediation

**Why.** An agent's first call should be discovery, not a guess that fails with `dpop_proof_invalid`. PRC-7807 with a type URL alone is terse; agents need a remediation hint to recover without doc-diving.

**How to apply.** `GET /.well-known/wallet-configuration` returns `supported_formats`, `supported_schemes`, `intent_hash_algorithm`, `jwks_uri_required`, and `audience_thumbprint`. `GET /v1/capabilities` (on the wallet, with the active access token) projects the inbound RAR as `{ payment_id, chain, recipient, max_amount, expires_at, remaining_uses }`. Every 401 and 403 error body carries `remediation: { action: "refresh_dpop" | "reauth_ciba" | "request_new_authorization", docs_url, ciba_endpoint?, authorize_endpoint? }`. Every retryable error carries `Retry-After` with backoff guidance.

### D-13: Seed posture is reported, not assumed

**Why.** Open `SeedSealing` traits permit a "just for staging" environment-variable-backed key to stay in production. Without runtime visibility, the security posture is invisible.

**How to apply.** `SeedSealing` reports `posture() -> SealingPosture { Dev, Hsm, Kms }`. `zspend-runtime` refuses to start in `Dev` posture unless `ZSPEND_ALLOW_DEV_SEED=1` is set. `/readyz` includes posture; `/metrics` exposes a gauge. Seed rotation has a planned API on the trait (`rotate_seed(new) -> Result<MigrationPlan>`) even though the v1 body is `unimplemented!()`; the shape lands now so v2 is additive.

### D-14: One RAR entry per token in v1, with a rejection path for batching

**Why.** RFC 9396 explicitly allows multi-entry RAR. If we ship "v1 silently picks `[0]`," batching arrives later as a quiet semantic shift.

**How to apply.** v1 mints reject `authorization_details.length != 1` with `rar_too_many_entries`. The wallet rejects multi-entry tokens with the same code. When batching lands, the rejection becomes the contract change to negotiate.

### D-15: Naming discipline lifts brand into package names; everything on the wire is unbranded

**Why.** Integrators grep wire fields and type names. A leaked brand becomes the integration vocabulary forever.

**How to apply.** Wire fields: `payment_authorization`, `payment_request`, `signed_payload`, `wallet_endpoint`, `payee_policy`, `intent_hash`, `act.sub`. Rust types: `PaymentAuthorization`, `SignedPayload`, `Submitter`, `Hold`, `SpendHold`, `DisclosureFetcher`, `SigningPolicy`. Capability id: `payment_authorization:sign`. The brand survives only in repo and crate names (`zentity`, `zpay`, `zally`, `fauzec`, plus the `zspend-core` and `zspend-runtime` crates inside the zpay workspace) and in error type URLs (`https://errors.zentity.xyz/wallet/<code>`).

## 4. The vocabulary spine

| Canonical name | Type / format | Lives in | Meaning |
| --- | --- | --- | --- |
| `payment_authorization` | RAR `type` string | zentity (mint), zspend (verify) | The structured spend grant; everything below is its content. |
| `chain` | `{ namespace, reference }` (CAIP-2) | RAR, wallet `/sign`, capability `actionParams` | Chain identifier; replaces freeform `network`. |
| `recipient` | CAIP-10 account id string | RAR, wallet, capability allowlist | The signed-over destination. |
| `amount` | `{ currency, value, unit }` | RAR, wallet, capability | Decimal string in `"base"` units; no `_zat` on the wire. |
| `payment_id` | ULID string | zpay `/prepare`, RAR, wallet, settle | Joins authorization, signing, broadcast, and receipt. |
| `intent_hash` | `"v1:sha256:<base64url 32 bytes>"` | RAR, wallet `/sign` verification | Hash of the parsed payment tuple (D-4). |
| `payment_request` | `{ scheme, value }` | zpay `/prepare`, wallet `/sign` body | Tagged scheme + scheme-specific payload (ZIP-321 today). |
| `signed_payload` | `{ format, bytes, tx_id, fee, expires_at, metadata }` | wallet `/sign` response, zpay `/settle` body | The chain-neutral envelope around the signed transaction. |
| `format` | `"pczt-v1"` (extensible) | `signed_payload.format` | Identifies the bytes layout. |
| `wallet_endpoint` | `{ url, authorization_details_types_required }` | zpay `/prepare` response | Routing target the BFF must call. |
| `payee_policy` | `{ payer_flow, requires_verify }` | `AcceptsEntry`, zpay `/prepare` | Replaces `merchant_requires_verify`. |
| `payer_flow` | `"agent" \| "external" \| "operator_custodied"` open string | `payee_policy.payer_flow` | UI affordance; open vocabulary for forward compat. |
| `act.sub` | JWK thumbprint (Ed25519) | access token claim | Pairwise agent actor id. |
| `cnf.jkt` | JWK thumbprint | access token claim | DPoP key binding. |
| `aud` | JWK thumbprint of wallet instance | access token claim | Wallet identity (D-5). |
| `jti` | UUIDv7 string | access token claim, `usage_ledger` PK | Single-use spend identifier (D-8). |
| `Submitter` | Rust trait | zally (canonical); consumed by zpay and zspend | Role: hand bytes to the chain. |
| `SignedPayload` | Rust struct | zally | Wire envelope around signed bytes; chain-neutral. |
| `IntentHasher` | Rust function (versioned, domain-separated SHA-256) | zally | Parsed-tuple intent binding (D-4). |
| `Canonicalizer` | Rust trait per scheme | zally (ZIP-321 impl ships here); future schemes ship their own impls | Parse-and-canonicalize per payment scheme (D-11). |
| `PaymentAuthorization` | Rust struct + TS Zod schema | zspend (Rust verify); zentity (TS mint); conformance vectors shared | RAR entry type. |
| `Hold` | Rust struct | zally | Reservation of notes against a planned spend. |
| `SpendHold` | Rust struct | zentity (CIBA holds), zspend (per-token) | Cross-domain hold vocabulary. |
| `DispenseHold` | Rust struct | fauzec | Faucet-scoped hold, renamed from `DispenseReservation`. |
| `DisclosureFetcher` | Rust trait | zpay (renamed from `TransactionFetcher`) | Fetches the ZIP-311 disclosure projection. |
| `SigningPolicy` | Rust trait | zspend (service-internal) | Startup-time invariants only (D-13). |
| `payment_authorization:sign` | capability id string | zentity capability catalog | The capability that grants minting `payment_authorization`. |

### Error vocabulary

PRC-7807 envelope, `type: https://errors.zentity.xyz/wallet/<code>`, top-level `retryable: bool`, `remediation` object (D-12), `Retry-After` on retryable errors.

| HTTP | code | retryable | meaning |
| --- | --- | --- | --- |
| 400 | `payment_request_invalid` | false | Scheme value failed to parse or wrong chain. |
| 401 | `dpop_proof_invalid` | false | Refresh the DPoP proof; remediation = `refresh_dpop`. |
| 401 | `access_token_invalid` | false | Re-authenticate; remediation = `reauth_ciba`. |
| 401 | `token_revoked` | false | Token in revocation cache; remediation = `request_new_authorization`. |
| 403 | `intent_mismatch` | false | `intent_hash` does not match the parsed request. |
| 403 | `recipient_not_allowed` | false | RAR allowlist miss (distinct from `intent_mismatch` for telemetry). |
| 403 | `amount_exceeded` | false | RAR cap miss. |
| 403 | `audience_mismatch` | false | Token `aud` does not match wallet thumbprint. |
| 409 | `token_already_consumed` | false | Replay of `jti` with a different `intent_hash`. |
| 410 | `authorization_expired` | false | RAR `expires_at` passed. |
| 422 | `insufficient_funds` | false | Wallet balance below `amount.value`. |
| 422 | `rar_too_many_entries` | false | v1 accepts exactly one RAR entry (D-14). |
| 503 | `seed_unavailable` | true | Operator page; remediation hint includes runbook link. |
| 503 | `chain_unreachable` | true | Submitter cannot reach its chain backend. |
| 503 | `revocation_cache_stale` | true | Wallet failed closed on stale revocation. |
| 503 | `not_ready` | true | Pre-readiness; check `/readyz`. |

## 5. Wire surfaces

### 5.1 Wallet runtime (`zspend-runtime` binary, served from the zpay workspace)

| Method | Path | Auth | Request | Response |
| --- | --- | --- | --- | --- |
| GET | `/.well-known/wallet-configuration` | none | n/a | `{ supported_formats, supported_schemes, intent_hash_algorithm, audience_thumbprint, jwks_uri }` |
| GET | `/v1/capabilities` | DPoP-bound `at+jwt` | n/a | RAR projection: `{ payment_id, chain, recipient, max_amount, expires_at, remaining_uses }` |
| POST | `/v1/payments/sign` | DPoP-bound `at+jwt` | `{ payment_request: { scheme, value } }` | `{ signed_payload }` |
| GET | `/v1/payments/{tx_id}` | DPoP-bound `at+jwt` | n/a | Read-through to chain backend for status. |
| POST | `/v1/holds` | DPoP-bound `at+jwt` (operator scope) | `{ payment_id, amount, expires_at }` | `Hold` projection |
| DELETE | `/v1/holds/{hold_id}` | DPoP-bound `at+jwt` (operator scope) | n/a | `204` |
| GET | `/healthz` | none | n/a | `200` if the process is up. |
| GET | `/readyz` | none | n/a | seed unsealed, JWKS reachable, libSQL up, chain backend up, revocation cache fresh, posture reported. |
| GET | `/metrics` | optional bearer (operator) | n/a | Prometheus exposition. |

Error vocabulary on every authenticated route: §4 table.

### 5.2 Facilitator (`zpay`, `/x402/v2/*`) changes

| Method | Path | Change |
| --- | --- | --- |
| GET | `/accepts` | `AcceptsEntry` adds `payee_policy: { payer_flow, requires_verify }`; removes `merchant_requires_verify`. |
| POST | `/prepare` | Response gains `wallet_endpoint: { url, authorization_details_types_required }` and `payee_policy`. `payment_request` returned as `{ scheme: "zip321", value }`, not bare URI. |
| POST | `/settle` | Body changes from `{ payment_id, idempotency_key, raw_tx_hex }` to `{ payment_id, idempotency_key, signed_payload }`. Server runs Extractor + broadcast; rejects PCZTs that are not extractor-ready. |
| POST | `/verify` | Unchanged. |
| GET | `/payments/{id}` | Unchanged. |
| GET | `/payments/{id}/events` | Unchanged. |

Removed errors: `RawTxHexInvalid`, `TransactionMalformed` (replaced by `signed_payload_invalid`, `pczt_not_extractor_ready`).

### 5.3 Identity issuer (`zentity`)

| Method | Path | Change |
| --- | --- | --- |
| POST | `/api/auth/oauth2/bc-authorize` | Accepts `authorization_details` of `type: "payment_authorization"`; validates exactly one entry (D-14); requires `intent_hash`; evaluates capability against `capability_usage_ledger` atomically (D-2, D-8). |
| POST | `/api/auth/oauth2/revoke` | RFC 7009; writes `revoked_tokens`. |
| GET | `/api/auth/oauth2/revoked` | Query `?since=<ts>`; returns delta stream for wallet pollers (D-6). |
| GET | `/.well-known/oauth-authorization-server` | Adds `authorization_details_types_supported: ["payment_authorization"]`. |
| GET | `/api/auth/agent/capabilities` | Seeds `payment_authorization:sign` with the `actionParams` schema (D-1, D-10). |

tRPC routers updated: `agentBoundaries` exposes CRUD for `payment_authorization:sign` constraints (`chain`, `recipient_in[]`, `max_amount_per_spend`, `daily_limit`, `cooldown_sec`). The `ciba` router gains `previewAuthorization` to render the signed spend card (D-7).

## 6. Per-repo change list

### 6.1 zally

**Add.**
- `Wallet::construct_pczt(proposal: Proposal) -> Result<Pczt>`: build-only path; no `idempotent_submission` write.
- `Wallet::sign_pczt(pczt: Pczt) -> Result<SignedPczt>`: sign in place, return extractor-ready PCZT with `proprietary["zentity.final"] = true`.
- `Hold` type (renamed from `SpendReservation`); `Wallet::finalize_hold(hold_id, tx_id)` to bind a hold to a broadcast.
- `SignedPayload` envelope type (`{ format, bytes, tx_id, fee, expires_at, metadata }`): the canonical wire shape consumed by zpay (`/settle` body) and produced by zspend (`/sign` response). Chain-neutral on purpose (D-3).
- `IntentHasher`: parsed-tuple SHA-256 binding with versioned domain separation (D-4). Conformance vectors ship in `zally-testkit`; a TypeScript mirror lives at `apps/web/src/lib/agents/intent-hash.ts` and is tested against the same vectors.
- `Canonicalizer` trait per payment scheme (D-11). zally ships the ZIP-321 impl; non-Zcash schemes ship their own crates implementing the trait.

**Break.**
- `SendOutcome` splits into `SignedPczt { pczt, tx_id, fee_zat, expiry_height }` and `BroadcastOutcome { tx_id, height }`.
- `DispenseReservation` renamed to `Hold`; `ReservationId` renamed to `HoldId`. The `Dispense` prefix was fauzec-flavored leaking into the library.
- `SeedSealing::posture()` added; impls updated; `unsafe_plaintext_seed` feature gate reports `Dev` posture.

**Tests / docs.**
- Conformance vectors for `intent_hash` (D-4) shipped in `zally-testkit`.
- `Submitter` semantics documented as "broadcast-only after this PR." The testkit's `CaptureSubmitter` removed from public API; build-without-broadcast goes through `construct_pczt`.

### 6.2 zpay

**Add.**
- `crates/zpay-core` depends on `zally` directly for `Submitter` and `SignedPayload` (no third workspace crate).
- `/settle` Extractor + broadcast stage in `settle.rs`.
- `payee_policy` and `wallet_endpoint` plumbed through `/prepare` (`AcceptsEntry`, `Preparation`).
- `signed_payload_invalid`, `pczt_not_extractor_ready` error variants.

**Break.**
- `BroadcastClient` deleted; zpay consumes `zally::Submitter` directly.
- `TransactionFetcher` renamed to `DisclosureFetcher` (trait lives in `crates/zpay-core/src/transaction_fetcher.rs`; module file renames to `disclosure_fetcher.rs`).
- `/settle` body field `raw_tx_hex` removed; replaced by `signed_payload`.
- `merchant_requires_verify` removed from `AcceptsEntry`; replaced by `payee_policy`.
- Duplicate `SettleError` in `crates/zpay-core/src/error.rs` deleted; the one in `settle.rs` is canonical.
- Duplicate `OracleError` in `crates/zpay-core/src/error.rs` deleted; the one in `oracle.rs` is canonical.

**Tests / docs.**
- Update ADR-0006 to note that `/settle` is now Extractor + broadcast.
- New runbook entry for `pczt_not_extractor_ready`.

### 6.3 zentity (`apps/web`)

**Add.**
- `apps/web/src/lib/agents/payment-authorization.ts`: RAR type definition + Zod schema (D-1, D-10).
- `apps/web/src/lib/agents/intent-hash.ts`: parsed-tuple hash function with conformance vectors (D-4).
- `apps/web/src/lib/agents/revocation-stream.ts`: backend for `/api/auth/oauth2/revoked` delta endpoint (D-6).
- `apps/web/src/lib/auth/oidc/token-revocation.ts`: RFC 7009 endpoint glue and `revoked_tokens` writes.
- `apps/web/src/lib/db/schema/revoked-tokens.ts`: `revoked_tokens` table (`jti`, `revoked_at`, `reason`, `actor_sub`).
- Capability seed entry `payment_authorization:sign` (D-15).
- tRPC: `agentBoundaries.upsertPaymentAuthorization` and `ciba.previewAuthorization`.
- Signed push payload for CIBA approval card (D-7); service worker verification step in `public/push-sw.js`.

**Break.**
- Capability evaluation moves from advisory to authoritative at mint time (D-2). `customGrantTypeHandlers` for CIBA wraps the ledger insert in the same transaction as the token row.
- `customIdTokenClaims` reads RAR from the staged ephemeral entry; staging path validated by `intent_hash` round-trip.

**Remove.**
- Stale `request_approval` capability (drift per CLAUDE.md).
- Any code that surfaces `purchase` as biometric without `payment_authorization` content (the chain-coupled wording is replaced by RAR `chain`).
- Plans for a per-request `/api/auth/agent/sessions/{thumbprint}` lookup (rejected in design; not built).

**Docs.**
- ADR-0048 (new): `payment_authorization` RAR type and capability binding.
- ADR-0049 (new): Token revocation propagation and the wallet's polling contract.
- RFC-0040 revision-history entry pointing at this proposal for `act.sub` + RAR carriage.

### 6.4 zentity (`apps/demo-rp`)

**Add.**
- `apps/demo-rp/src/lib/aether/wallet-client.ts`: DPoP-bound POST to `wallet_endpoint`; constructs `payment_request` envelope.
- `apps/demo-rp/src/app/api/aether/sign/route.ts`: BFF orchestrator between CIBA token and the wallet runtime.
- `PaymentBridge` updated to render the signed RAR (from `ciba.previewAuthorization`) as the confirmation surface and link to `zexplorer.app/testnet/tx/<tx_id>` on success.

**Remove.**
- The "open in wallet" placeholder for the agent path (now the explicit external path under `payer_flow: "external"`).
- Direct usage of `merchant_requires_verify` in `aether/payment-bridge.tsx` (consume `payee_policy`).

### 6.5 fauzec

**Change.**
- Adapt to renamed `Hold` (formerly `SpendReservation`). `DispenseHold` remains the fauzec-scoped subtype.
- No protocol or wire change; reference-pattern role preserved.

### 6.6 zspend crates (inside the zpay workspace)

Two new crates live inside the existing zpay workspace, not a new repo. The facilitator (`zpay-runtime`) and the wallet (`zspend-runtime`) are sibling binaries in the same Cargo workspace; they share `zpay-core` for shared wire types (`PaymentId`, `PayeeId`, network identifiers) and both depend on `zally` for `Submitter`, `SignedPayload`, `IntentHasher`, and the `Canonicalizer` trait. Each binary ships as its own Docker image and its own Railway deploy; operators run whichever subset their role calls for. A `git subtree split` is the escape hatch if real fork demand later justifies extraction.

`zspend-core` and `zspend-runtime` are strictly internal: no public library API, no external consumers, no semver promises across the workspace boundary.

- `crates/zspend-core` (service-internal types only):
  - `PaymentAuthorization` Rust struct mirroring the issuer's Zod schema, plus conformance vectors that match the TypeScript side at `apps/web/src/lib/agents/payment-authorization.ts`.
  - `AccessTokenVerifier`, `JwksVerifier`, `DpopVerifier`: RFC 9068 + RFC 9449 consumers of the issuer's JWKS and DPoP proofs.
  - `UsageLedger`: libSQL schema and `INSERT ... ON CONFLICT (jti)` claim logic (D-8).
  - `RevocationCache`: delta-stream consumer with hard-capped poll interval and fail-closed staleness check (D-6).
  - `SigningPolicy`: startup-time invariants only (D-13).
  - PRC-7807 error encoder with `remediation` (D-12).
- `crates/zspend-runtime`:
  - Binary. Axum routes per §5.1; tracing-subscriber JSON logs; `Retry-After` middleware.
  - Wires `SeedSealing` (`age` for dev, KMS adapter for prod), libSQL `UsageLedger` (D-8), `zally::Wallet`.
  - `zspend-runtime init` (seal a dev seed, print wallet `aud` thumbprint to register with the issuer) and `zspend-runtime serve`.

Workspace-level additions: `Dockerfile.zpay` and `Dockerfile.zspend` (split from the current single `Dockerfile`); `docker-compose.yml` gains a `zspend` service alongside the existing `zpay` service. Railway deploys each as its own service against the same repo, with separate `railway.toml` profiles or per-service env overrides. Port convention mirrors fauzec (testnet base + ops offset) so testnet and mainnet wallet instances coexist on one host.

## 7. What we delete

Aggressive removals in this slice; cite paths.

- `crates/zpay-core/src/error.rs::SettleError` (duplicate of `settle.rs::SettleError`).
- `crates/zpay-core/src/error.rs::OracleError` (duplicate of `oracle.rs::OracleError`).
- `crates/zpay-x402/src/lib.rs`: `raw_tx_hex` parameter on `/settle`; `RawTxHexInvalid` and `TransactionMalformed` error variants.
- `crates/zpay-core/src/accepts.rs::AcceptsEntry::merchant_requires_verify` field.
- `zally::SendOutcome::broadcast_at_height` (split into `SignedPczt` + `BroadcastOutcome`).
- `zally::SpendReservation` type name (renamed to `Hold`; behavior preserved).
- `zally::TransactionFetcher` trait name (renamed to `DisclosureFetcher`).
- `zally-testkit::CaptureSubmitter` from the public API surface (`construct_pczt` is the supported build-without-broadcast path).
- `apps/web/src/lib/db/seed.ts`: stale `request_approval` capability entry; `purchase` action params that are not RAR-shaped.
- `apps/demo-rp/src/components/aether/payment-bridge.tsx`: code paths that read `merchant_requires_verify` directly.
- Any reference to a `BroadcastRouter` in `AppState` (not built; explicitly rejected).
- Plans for a `/api/auth/agent/sessions/{thumbprint}` lookup endpoint (rejected; not present).
- `payment-intent+jwt` (rejected; RAR `intent_hash` is the binding).
- The "umbrella `payment:authorize` scope at the BFF" idea (rejected; CIBA-per-spend is the contract).
- A `zspend-core` workspace crate that shares `Submitter`, `SignedPayload`, `IntentHasher`, or the `Canonicalizer` trait between `zpay-runtime` and `zspend-runtime` (rejected; those types live in zally as canonical wallet-library types). The crate `zspend-core` still exists inside the zpay workspace, but it carries only service-internal auth/wire/ledger types.
- A separate `zspend` repo (rejected; `zspend-core` and `zspend-runtime` are sibling crates inside the zpay workspace, deployed as separate Docker images. `git subtree split` is the escape hatch if real fork demand emerges).
- `BroadcastClient` trait name across zpay (replaced by `Submitter`).
- env var `NEXT_PUBLIC_ZPAY_USE_RAW_TX_HEX` if it exists in any local config (not committed; verify before merge).

## 8. Build sequence

Phases reference decisions by D-N.

### Phase 1: Vocabulary renames and dead-code removal (zally, zpay, fauzec)

**Work.** Mechanical renames and removals with no contract changes. Scope split from the earlier draft because the trait consolidation and the wire-format change are tightly entangled and land cleaner together in Phase 2.

- **zally**: rename `DispenseReservation` to `Hold` (the `Dispense` prefix was fauzec-flavored leaking into the library) and `ReservationId` to `HoldId`; module file renames track (`reservation.rs` to `hold.rs`, `reservation_id.rs` to `hold_id.rs`). Add `SeedSealing::posture() -> SealingPosture { Dev, Hsm, Kms }` as an additive trait method.
- **zpay**: rename `TransactionFetcher` to `DisclosureFetcher` (the trait lives in `crates/zpay-core/src/transaction_fetcher.rs`; an earlier draft mistakenly placed this rename in zally). Delete the duplicate `SettleError` and `OracleError` enums in `crates/zpay-core/src/error.rs`; the canonical variants in `settle.rs` and `oracle.rs` have zero imports of the duplicates.
- **fauzec**: bump zally rev pin to the post-rename hash; adopt the new `Hold` and `HoldId` names through every call site.

**Validation gate.** All three repos compile against the new names. `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`, and `cargo test --workspace` pass per repo. `cargo deny` stays green.

**Rollback.** Revert per-repo commits independently; no integrator consumers exist.

### Phase 2: Wire change (PCZT) + trait consolidation + chain-neutral types

**Work.** These three land together because they are entangled by `/settle`'s body shape and the broadcast trait surface; splitting them would require temporary adapter code that Phase 2 immediately removes.

- **Add to zally**: `SignedPayload` envelope (`{ format, bytes, tx_id, fee, expires_at, metadata }`), `IntentHasher` (parsed-tuple SHA-256 with versioned domain separation per D-4), `Canonicalizer` trait per scheme with the ZIP-321 implementation (D-11). Conformance vectors ship in `zally-testkit`.
- **Add to zally**: `Wallet::construct_pczt(proposal) -> Result<Pczt>` (build-only path; no `idempotent_submission` write) and `Wallet::sign_pczt(pczt) -> Result<SignedPczt>` per D-3. Split `SendOutcome` into `SignedPczt { pczt, tx_id, fee_zat, expiry_height }` and `BroadcastOutcome { tx_id, height }`.
- **zpay**: delete `BroadcastClient` and its `BroadcastError`; zpay consumes `zally::Submitter` and `zally::SubmitterError` directly (D-9). `settle.rs` absorbs the trait change: input switches from `&str` (hex) to `&[u8]` (decoded from `signed_payload.bytes`), and the outcome variant table maps `SubmitOutcome::{Accepted, Duplicate, Queued, Rejected}` to the zpay-side settlement states. Add the Extractor stage; reject non-extractor-ready PCZTs with `pczt_not_extractor_ready`.
- **zpay**: change `/settle` body from `{ payment_id, idempotency_key, raw_tx_hex }` to `{ payment_id, idempotency_key, signed_payload }`. Wire `payee_policy` and `wallet_endpoint` into `/prepare` responses (`AcceptsEntry`, `Preparation`).
- **zentity (apps/web)**: add the TypeScript `intent_hash` mirror at `apps/web/src/lib/agents/intent-hash.ts` with the shared conformance vectors as a test.

**Validation gate.** Round-trip test: zally testkit signs a PCZT, zpay's `/settle` extracts and broadcasts via a mock `Submitter`, the resulting txid matches the PCZT's. Reject test: non-extractor-ready PCZT returns `pczt_not_extractor_ready`. Schema test: `/prepare` response validates against the new shape. Outcome-translation test covers every `SubmitOutcome` variant to its zpay-side mapping.

**Rollback.** Revert the `/settle` body change; restore `raw_tx_hex` from the previous commit. PCZT construction in zally is additive and stays; the trait consolidation in zpay reverts together with the settle.rs commit.

### Phase 3: zentity RAR + revocation + signed approval card

**Work.** Implement `payment_authorization` RAR type validation, `intent_hash` over the parsed tuple with the same conformance vectors that ship in `zally-testkit` (D-4). Atomic capability evaluation in the CIBA grant handler (D-2). RFC 7009 `/oauth2/revoke` plus `revoked_tokens` table plus `/api/auth/oauth2/revoked?since=` delta endpoint (D-6). Discovery metadata update for `authorization_details_types_supported`. Capability seed for `payment_authorization:sign` with CAIP-typed `actionParams` (D-10). Signed push payload for approval (D-7); service worker verification.

**Validation gate.** Unit tests: mint refuses on daily cap exceeded; mint refuses on `intent_hash` mismatch returned from approval; revocation appears in the delta stream within one polling cycle. E2E: dashboard revoke -> wallet's revocation cache reflects within hard-capped window. Push card signature verification rejects a tampered payload.

**Rollback.** Disable the new RAR `type` in the bc-authorize validator (returns `unsupported_authorization_type`). Revocation table and endpoint stay; harmless without minters.

### Phase 4: zspend-runtime binary

**Work.** Scaffold `crates/zspend-core` (service-internal: `PaymentAuthorization`, `AccessTokenVerifier`, `JwksVerifier`, `DpopVerifier`, `UsageLedger`, `RevocationCache`, `SigningPolicy`, PRC-7807 encoder) and `crates/zspend-runtime` (binary with axum routes) inside the zpay workspace. Add `Dockerfile.zspend` and the `zspend` service to `docker-compose.yml`. Implement `/v1/payments/sign`: DPoP verify, JWKS verify with forced refetch on `kid` miss, `aud` thumbprint check (D-5), parse + canonicalize `payment_request`, recompute `intent_hash`, compare to RAR, single-use `jti` claim via `INSERT ... ON CONFLICT` against shared `usage_ledger` (D-8), sign via `zally::Wallet::sign_pczt`. Implement `/.well-known/wallet-configuration` and `/v1/capabilities` (D-12). `age` `SeedSealing` impl with `posture() = Dev`. libSQL `UsageLedger`. Revocation poller with hard-capped interval (D-6). PRC-7807 error encoder with `remediation` and `Retry-After`.

**Validation gate.** End-to-end test in CI: spawn `zspend-runtime` with a sealed dev seed, mint a token against a local zentity, sign a PCZT, round-trip through zpay-runtime's `/settle`, assert broadcast against a mock zinder. Replay test: same `jti` returns the cached `signed_payload`; `jti` with different `intent_hash` returns `token_already_consumed`. Stale revocation test: wallet returns `revocation_cache_stale` after the hard cap.

**Rollback.** `zspend-runtime` and `zspend-core` are isolated crates; reverting Phase 4 leaves zpay-runtime, zally, and zentity in the state shipped by Phases 1-3 with the `external` wallet path still functional.

### Phase 5: Aether flow end-to-end in demo-rp

**Work.** BFF orchestrator (`apps/demo-rp/src/app/api/aether/sign/route.ts`): reads `wallet_endpoint` from `/prepare`, calls `/oauth2/bc-authorize` with `payment_authorization` RAR, polls CIBA, attaches token plus DPoP to wallet `/sign`, hands `signed_payload` to zpay `/settle`. Receipt page links to `zexplorer.app/testnet/tx/<tx_id>`. Service worker renders the signed RAR as the structured spend card; verifies signature.

**Validation gate.** Playwright E2E hitting testnet: user clicks "buy 0.5 ZEC", approves on the same device, sees `tx_id` and explorer link. Negative test: user denies in the push card -> demo-rp shows graceful failure with `remediation.action = "request_new_authorization"`.

**Rollback.** Aether page reverts to "open in wallet" with `payer_flow: "external"`. The PCZT and RAR plumbing in zpay and zentity stay deployed.

### Phase 6: External wallet path

**Work.** demo-rp renders the ZIP-321 QR when `payee_policy.payer_flow == "external"`. Confirm `PaymentBridge` SSE picks up a Zashi-broadcast tx via zinder watch. No new wallet code; just UI affordance plus integration test.

**Validation gate.** Manual test with Zashi against testnet: QR scan, broadcast, demo-rp shows `confirmed` in SSE within expected window. Automated test with a fake external broadcaster that submits to the same zinder.

**Rollback.** Remove the QR branch from `PaymentBridge`; the agent path keeps working.

### Phase 7: Operator surfaces and KMS sealing

**Work.** Railway deployment configs for `zspend-runtime` mirroring fauzec's port and env convention; a sibling service in the same zpay project. KMS `SeedSealing` adapter (new crate `crates/zspend-keys-kms` inside the workspace). Prometheus dashboards: mint refusal rate, wallet sign latency, intent-mismatch rate, revocation staleness, posture gauge. Runbook entries for `seed_unavailable`, `revocation_cache_stale`, `chain_unreachable`. Bootstrap docs (`zspend-runtime init` walkthrough, env schema).

**Validation gate.** Staging deploy of `zspend-runtime` with `posture() = Kms` passes `/readyz`. Dashboards populate against a real testnet workload from Phase 5.

**Rollback.** Stop the staging deploy; production path is the local-dev `age` seal.

### Phase 8: Hardening and security model doc

**Work.** Chaos tests: revoke a token between mint and sign (wallet refuses with `token_revoked`). Replay `jti` with different `intent_hash` (wallet returns `token_already_consumed`). zinder unreachable during `/settle` (zpay returns `chain_unreachable`, retryable). Multi-replica `zspend-runtime` against a single shared `usage_ledger` (no double-sign). Audience mismatch (attacker swaps `wallet_endpoint.url` after mint; wallet refuses with `audience_mismatch`). Document the security model in `docs/architecture/agent-payments-security-model.md`, the operator runbook in `docs/runbooks/zspend-operator.md`, and ADRs for the breaking changes (in zally, in zpay covering both `zpay-runtime` and `zspend-runtime`, in zentity).

**Validation gate.** All chaos tests pass in CI. Security model doc reviewed against the threat list (intent canonicalization drift, revocation window, audience confusion, idempotency race, JWKS cache window, `act.sub` revocation lag, weak sealing posture).

**Rollback.** Documentation; nothing to roll back.

## 9. What we explicitly do not build in this slice

- `payment-intent+jwt`: RAR `intent_hash` already binds the URI to the grant; a second signature is ceremony.
- A wallet-side "top-up" semantic when a spend exceeds RAR: each spend is a fresh CIBA approval. Tokens are bounded artifacts, not balances.
- Multi-entry RAR batching: v1 rejects with `rar_too_many_entries`; batching is an additive change post-v1.
- Shielded Zcash spends: the PCZT envelope supports it; the v1 wallet adapter signs transparent only. The wire does not need to change to add Sapling or Orchard.
- A second `Submitter` impl per wallet for direct chain access: zpay remains the broadcast path; bypass routes are operator decisions outside this slice.
- A `BroadcastRouter` per `(payee_id, network)` in zpay: not needed; one `Submitter` per zpay deployment covers production.
- Subscription, escrow, and refund authorization types: future RAR types; not part of v1.
- A per-request callback from wallet to issuer for fresh authorization checks: defeats offline DPoP verification and creates a hot dependency.
- An on-chain attestation that the wallet ran: out of scope; the audit trail is the access token + `usage_ledger` row.

## 10. Open questions for the operator

1. **Final binary name for the wallet runtime.** Proposed: `zspend-runtime` (paired with `zspend-core`) inside the zpay workspace. Mirrors `zpay-runtime` + `zpay-core` and reads cleanly in Docker tags, logs, and Railway service names. Confirm before scaffolding.
2. **Deployment target for `zspend`.** Railway (matches fauzec) versus a dedicated host with KMS attachment. The KMS adapter is sealed-only in v1; the choice of provider (AWS KMS, GCP KMS, HashiCorp Vault) shapes the adapter trait impl.
3. **Hard cap on revocation poll interval.** Proposal is 30s. Tighter is safer; looser reduces issuer load. Confirm the value baked into `zspend-core` config validation.
4. **`act.sub` storage and pairing.** The issuer's pairwise table already exists. Do we expose `act.sub` rotation in the dashboard, or rotate only on agent session reissue.
5. **`zspend` audience thumbprint registration.** Manual registration via the rp-admin UI versus an out-of-band CLI. Both are workable; pick before Phase 4.
6. **Breaking-change posture for zpay's `/settle`.** No integrators today, so the break is free. Confirm there are no downstream copies before deleting `raw_tx_hex`.
7. **`payee_policy.payer_flow` registry.** Open string with documented values (`"agent"`, `"external"`, `"operator_custodied"`) versus a closed enum. Open is forward-compatible; closed catches typos.
8. **`age` seed format for dev.** Standard `age` identity sidecar (fauzec pattern) versus a zspend-specific extension. Reuse fauzec verbatim unless a concrete reason emerges.
9. **Conformance vector publication path.** Inside `zally-testkit` only versus a public spec repo. Affects how third-party wallets verify they match.
10. **Sentry / observability backend.** Whether the operator deploys with the Vercel observability stack, Railway's built-in logs, or an external Prometheus + Grafana pair. Shapes the `/metrics` exposition format choice.
