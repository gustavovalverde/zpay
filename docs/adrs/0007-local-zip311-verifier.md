# ADR-0007: Local ZIP-311 Verifier with 3-Axis Wire Verdict

| Field | Value |
| ----- | ----- |
| Status | Accepted; implementation details superseded by [ADR-0015](0015-zip311-draft1-verification.md) |
| Product | zpay |
| Domain | Facilitator wire surface, payment-disclosure verification |
| Related | [ADR-0003](0003-zinder-as-chain-plane.md), [ADR-0005](0005-protocol-neutral-core-with-wire-adapters.md), [ZIP-311](https://zips.z.cash/zip-0311) |

> The implementation details below describe the original transparent-only
> verifier. ADR-0015 replaces its parser, fetcher, network configuration, and
> pool coverage. The independent verdict axes remain authoritative.

## Context

The first M2 implementation of `POST /x402/v2/verify` delegated the entire ZIP-311 verification (parse, digest reconstruction, signature check, chain-presence probe) to zinder's `ExplorerQuery.VerifyPaymentDisclosure` RPC. The wire response carried a single fused `Verdict` enum with six variants (`Valid`, `MismatchAmount`, `InvalidSignature`, `TransactionNotFound`, `Malformed`, `CapabilityUnavailable`) that collapsed three orthogonal questions into one categorical answer.

That coupling has three structural problems:

- **The cryptography is an in-process operation that depends on no chain state.** Parsing a ZIP-311 disclosure, reconstructing the BLAKE2b digest, and verifying a BIP-322-legacy signature against a hash160 are all pure functions of the disclosure bytes plus the prevout scriptPubKey. Delegating them to an explorer-plane RPC adds a network round-trip and a capability dependency for work zpay could perform locally.
- **The fused verdict cannot express "the chain plane is unavailable but the cryptography verified."** A facilitator that knows the disclosure is well-signed but cannot reach an explorer plane should report exactly that, so the merchant can decide whether to fail open (cryptography is enough) or fail closed (require chain presence too). The fused enum forces one of those positions onto every caller.
- **The forward-compatibility door is closed.** An unknown disclosure version byte under the old model surfaces as `Malformed`, which tells a future operator "your bytes are broken" when the truth is "this build does not yet know how to interpret your bytes". Without an `Inconclusive` arm, ZIP-311 cannot evolve without breaking every older verifier.

The same audit surfaced a fourth problem: zinder's explorer plane does not currently expose raw transaction bytes with resolved prevout scriptPubKeys in a single capability. Even when the explorer-plane verifier exists, building a local verifier on top of zinder's wallet plane (which does expose `WalletQuery.TransparentOutputsByOutpoint`) is the path zpay needs anyway for the chain-side data.

## Decision

**Move ZIP-311 cryptography into `zpay-core` and split the wire response into three orthogonal axes.**

### Trait split

- `PaymentDisclosureVerifier` (in `zpay-core::verify`) runs only the cryptography: parse, digest, signature verification. It takes the disclosure bytes plus a `TransactionFetcher` and returns a 3-axis `VerifyResponse`. The implementation `LocalPaymentDisclosureVerifier` is configured with a `PaymentNetwork` at construction time.
- `TransactionFetcher` (in `zpay-core::transaction_fetcher`, a top-level sibling to `broadcast`, `tip`, and `oracle`) resolves a ZIP-244 txid to a minimal `DisclosedTransaction` carrying only the fields the verifier reads: prevout scriptPubKeys for transparent inputs and the shielded-output fields the deferred Sapling slice will consume.

The two traits never compose into a single super-trait. Production deployments mix and match: a local verifier paired with a `ZinderTransactionFetcher` in production, a local verifier paired with `RejectingTransactionFetcher` (a real `pub(crate) struct` in `zpay-runtime::rejecting_fetcher` whose `fetch_transaction` returns `FetchError::Unavailable`) in CI and dev stacks that have no explorer endpoint, or a local verifier paired with a scripted fixture in tests. The runtime selects between them at startup by checking `ZPAY_EXPLORER_URL`; `AppState` carries a single concrete fetcher chosen once, so the verify hot path costs one enum dispatch.

### 3-axis wire response

`POST /x402/v2/verify` returns:

```json
{
  "cryptographic_verdict": "valid" | "invalid_signature" | "malformed" | "inconclusive",
  "inconclusive_reason": "unsupported_pool" | "unknown_version" | "prevout_unresolved",
  "chain_presence": "mined" | "not_found" | "oracle_unavailable",
  "amount_reconciliation": "match" | "mismatch" | "not_checked",
  "transaction_id": "hex",
  "payment_id": "string",
  "disclosed_value_zat": 12345
}
```

`inconclusive_reason` appears only when `cryptographic_verdict == "inconclusive"`. The three top-level verdicts are independent: a disclosure can be cryptographically valid but pin to a transaction the chain plane cannot find; a disclosure can be malformed even when the oracle is healthy.

Callers that previously checked `verdict == "valid"` migrate to:

```
cryptographic_verdict == "valid" && chain_presence == "mined" && amount_reconciliation == "match"
```

There is no backwards-compat shim. The fused verdict is gone.

### Sapling deferral

Sapling spend-proof verification (Groth16 against `(rt, cv, nf, rk)` plus per-spend RedJubjub `spendAuthSig`) is gated behind a `verify_sapling` Cargo feature in `zpay-core`. Without the feature, any disclosure that touches the Sapling pool surfaces as:

```json
{ "cryptographic_verdict": "inconclusive", "inconclusive_reason": "unsupported_pool", ... }
```

The parser still decodes the Sapling fields verbatim; only the proof verification is deferred. This keeps the door open to either landing a Groth16 prover backend in a follow-on slice or delegating to a future zinder capability without rewriting the parser.

### Network pinning

`LocalPaymentDisclosureVerifier` is pinned to a single `PaymentNetwork` at construction time. The BLAKE2b digest is personalized with the SLIP-44 coin type (133 for mainnet, 1 for testnet/regtest), so a disclosure produced for one network does not verify under another. Auto-detecting the network from disclosure bytes would invite a network-confusion attack where a malicious sender pivots the verifier into a network of their choosing; the configured network MUST win.

`ZPAY_VERIFY__NETWORK` selects mainnet or testnet at startup. The variable has no default: an unset or empty value fails startup with `StartupError::VerifyNetworkMissing`. A regtest deployment pins to testnet explicitly (regtest carries no distinct SLIP-44 number); the operator must say so.

## Rationale

The three axes are the categorical questions a relying party asks when consuming a verify response:

- *Is the cryptography sound?* Answered locally, no chain dependency.
- *Is the chain plane reachable, and does it know about this transaction?* Answered by the fetcher, transport-class failures separated from "transaction not found".
- *Does the disclosed value match what the merchant expected?* Reserved for a follow-on slice that reads the disclosed Sapling outputs.

Splitting them lets each axis stay categorical: every variant carries its own meaning and can be acted on independently. The fused six-variant enum could not express common operational realities (cryptography fine, chain plane down) and forced callers to special-case fused-verdict translations.

Surfacing `Inconclusive { UnknownVersion }` rather than `Malformed` for unknown version bytes is load-bearing for forward compatibility. A future ZIP-311 revision that bumps the version byte should not cause every older facilitator to start emitting `Malformed`. `Inconclusive` is the right vocabulary: "this build does not know enough to answer".

## Consequences

Positive:

- Cryptography runs in-process: no network round-trip per `/verify` call, no explorer-plane capability dependency.
- The 3-axis verdict expresses operational realities the fused enum could not (cryptography fine but chain unreachable, signature invalid but transaction present, etc.).
- Forward compatibility for ZIP-311 revisions: an unknown version byte surfaces as `Inconclusive { UnknownVersion }` rather than `Malformed`.
- The deferred Sapling work has a clean landing zone: flip the `verify_sapling` feature gate and replace the `Inconclusive { UnsupportedPool }` short-circuit with the Groth16 verifier; no wire-shape change.

Negative:

- Wire shape changes for any caller that previously checked the fused `Verdict`. The migration is mechanical (three field checks instead of one) but every caller must update.
- `zpay-core` gains `blake2b_simd`, `secp256k1`, `ripemd`, and `sha2` dependencies. The compile-time cost is real but bounded (these are all small, no-std-clean crates).
- The local verifier currently surfaces `chain_presence: "oracle_unavailable"` more often than the zinder-backed verifier did. The follow-on slice that translates `WalletQuery.Transaction` + `TransparentOutputsByOutpoint` into a `DisclosedTransaction` is what closes that gap.

Neutral:

- The zinder explorer-plane `VerifyPaymentDisclosure` capability is no longer the source of truth. Operators who relied on it for capability discovery now read `chain_presence` instead.
- `ZinderTransactionFetcher` carries scaffolding for the upcoming translator but currently returns `FetchError::Unavailable`. `RejectingTransactionFetcher` and the zinder variant produce the same wire output today; they will diverge once the translator lands.

## Switch Criteria

We replace the local verifier with a delegated chain-plane verifier when ALL of:

- The explorer plane exposes a capability that returns the BLAKE2b digest output for a disclosure plus the per-input verification breakdown.
- The capability covers Sapling spend proofs as a first-class case rather than a delegation back to the consumer.
- The latency of the delegated call is competitive with the local path (sub-50ms 99th-percentile).

Until ALL three hold, the local verifier stays the default.

## Alternatives Considered

### Keep the fused `Verdict` and add fields for the missing axes

Would have avoided the wire-shape break but doubled the cognitive surface (six fused arms PLUS three new axis fields, none of which the existing arms could drop). The fused enum was the source of the ambiguity; widening it would not fix the problem.

### Move only the parser into `zpay-core`, keep the verifier in `zpay-runtime`

Splits the verifier across two crates for no architectural gain. The parser is small enough that the cost of co-locating it with the rest of the cryptography is negligible.

### Ship Sapling Groth16 verification in this commit

Would have doubled the scope of the slice and required either pulling in a heavy Groth16 prover dependency or hand-vendoring Sapling-specific verification helpers. The `verify_sapling` feature gate is the right deferral: the parser and the digest construction are both Sapling-aware; only the proof verification is deferred.

## Out of Scope

- Sapling spend-proof verification (deferred behind `verify_sapling`).
- Orchard payment disclosures (not yet specified by ZIP-311).
- Amount reconciliation against a prepared row (today every response is `AmountReconciliation::NotChecked`; the follow-on slice will populate `Match` / `Mismatch` once the verifier reads the disclosed Sapling outputs).
- Translating zinder explorer/wallet plane bytes into `DisclosedTransaction` (the `ZinderTransactionFetcher` scaffolding is here, but the translator lands in a follow-on slice).
