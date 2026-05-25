# Error Vocabulary

Every typed error that zpay returns at any boundary is registered here.
Each entry carries: the variant name, the layer that produces it, the
retry posture, the HTTP status code (when produced at a wire boundary),
and a one-line operator hint.

This document is the source of truth. Code generates from it (when the
`utoipa` schema needs an enum), tests assert against it, and PRs that
add a new error variant must update this table in the same change.

## Retry posture vocabulary

- **`retryable`**: the same request, retried, may succeed without the
  caller doing anything. Network blips, chain-stale conditions, JWKS
  cache miss with re-fetch in progress.
- **`not_retryable`**: the caller's inputs are wrong. Retrying with the
  same inputs produces the same error. The caller must change the
  request before retrying.
- **`requires_operator`**: the zpay operator must act before the next
  retry can succeed. Database unavailable, JWKS endpoint down,
  capability disabled, schema migration pending.

## Errors by layer

### `zpay-core::error::PrepareError`

| Variant | Retry | HTTP | Operator hint |
|---------|-------|------|---------------|
| `MerchantUnknown { merchant_id }` | not_retryable | 404 | Caller used an unregistered merchant. Operator: confirm TOML config has the merchant. |
| `RecipientAddressInvalid { reason }` | not_retryable | 422 | Caller's recipient does not parse or is wrong network. |
| `MemoOversize { actual_bytes, max_bytes }` | not_retryable | 422 | Caller requested a memo over 512 bytes. |
| `AgentAssertionInvalid { reason }` | not_retryable | 401 | Agent-Assertion JWT failed verification. |
| `DpopProofInvalid { reason }` | not_retryable | 401 | DPoP proof header malformed or mismatched. |
| `StoreInsertFailed { source }` | requires_operator | 503 | libSQL connection failed; check `/readyz` dependencies. |
| `ChainStale { derive_lag_blocks }` | retryable | 503 | Upstream zinder is lagging; retry after derive_lag_blocks < 16. |

### `zpay-core::error::SettleError`

| Variant | Retry | HTTP | Operator hint |
|---------|-------|------|---------------|
| `PreparationNotFound { payment_id }` | not_retryable | 404 | Caller's payment_id does not exist or expired. |
| `PreparationExpired { payment_id, expires_at_unix_seconds }` | not_retryable | 410 | Prepared transaction expired; agent must re-prepare. |
| `TransactionMalformed { reason }` | not_retryable | 422 | raw_tx_hex did not parse as a valid v5 transaction. |
| `TransactionRecipientMismatch` | not_retryable | 422 | Signed tx's recipient does not match the prepared recipient. |
| `TransactionAmountMismatch { expected_zat, actual_zat }` | not_retryable | 422 | Signed tx's amount does not match the prepared amount. |
| `TransactionMemoMismatch` | not_retryable | 422 | Signed tx's memo does not match the prepared memo bytes. |
| `TransactionExpiryHeightStale { current_height, expiry_height }` | not_retryable | 422 | Signed tx's expiry_height has passed since prepare. |
| `PohTokenInvalid { reason }` | not_retryable | 401 | PoH token failed signature or claim verification. |
| `PohVerificationLevelTooLow { required, actual }` | not_retryable | 403 | Payer's verification level is below the merchant's minimum. |
| `BroadcastRejected { zinder_reason }` | depends | 502 | zinder rejected the broadcast; reason describes the next step. |
| `BroadcastDuplicate { existing_txid }` | not_retryable | 200 | Already broadcast; this is an idempotent success. |
| `IndexerUnavailable { source }` | requires_operator | 503 | zinder unreachable; check `/readyz`. |
| `StoreUnavailable { source }` | requires_operator | 503 | libSQL unreachable; check `/readyz`. |
| `JwksUnavailable { source }` | requires_operator | 503 | zentity JWKS endpoint unreachable; check `/readyz`. |

### `zpay-core::error::OracleError`

| Variant | Retry | HTTP | Operator hint |
|---------|-------|------|---------------|
| `PaymentNotFound { payment_id }` | not_retryable | 404 | Caller's payment_id never reached settle. |
| `LedgerNotFound { txid }` | not_retryable | 404 | Caller's txid not associated with any zpay payment. |
| `LedgerStale { last_check_age_seconds }` | retryable | 200 | Ledger has not been refreshed by the subscription; oracle is degraded. |
| `WatchEndpointUnavailable { source }` | requires_operator | 503 | Fallback zexplorer watch endpoint unreachable. |

### `zpay-core::error::VerifyError`

| Variant | Retry | HTTP | Operator hint |
|---------|-------|------|---------------|
| `DisclosureInvalid { reason }` | not_retryable | 422 | ZIP-311 disclosure payload did not parse. |
| `DisclosureSignatureMismatch` | not_retryable | 401 | Disclosure signature did not verify. |
| `DisclosureTransactionNotFound { txid }` | not_retryable | 404 | Disclosed txid not on chain. |
| `DisclosureAmountMismatch { expected_zat, actual_zat }` | not_retryable | 422 | Disclosed amount does not match expected. |
| `DisclosureRecipientMismatch` | not_retryable | 422 | Disclosed recipient does not match expected. |
| `VerifierCapabilityDisabled` | requires_operator | 503 | zinder's ZIP-311 verifier capability is off; enable upstream. |

### `zpay-core::error::ComplianceError`

| Variant | Retry | HTTP | Operator hint |
|---------|-------|------|---------------|
| `JwksFetchFailed { source }` | retryable | 503 | Transient JWKS fetch failure; cache-miss + network blip. |
| `JwksKeyNotFound { kid }` | not_retryable | 401 | PoH token's `kid` is not in the JWKS document. |
| `SignatureInvalid` | not_retryable | 401 | PoH token signature does not verify. |
| `AudienceMismatch { expected, actual }` | not_retryable | 401 | PoH `aud` claim does not match merchant origin. |
| `DpopBindingMismatch { expected_jkt, actual_jkt }` | not_retryable | 401 | PoH `cnf.jkt` does not match prepare-time JKT. |
| `Expired { exp_unix_seconds, now_unix_seconds }` | not_retryable | 401 | PoH `exp` claim has passed. |
| `IssuerNotTrusted { iss }` | not_retryable | 401 | PoH `iss` is not in `ZPAY_COMPLIANCE__ACCEPTED_ISSUERS`. |

### `zpay-store::error::StoreError`

Internal; never crosses a wire boundary directly. Always wrapped by a
higher-layer error.

| Variant | Retry | Notes |
|---------|-------|-------|
| `ConnectionFailed { source }` | retryable | libSQL pool exhausted or remote unreachable. |
| `MigrationPending { current_version, required_version }` | requires_operator | Operator must run `zpay-ops migrate`. |
| `IntegrityViolation { constraint }` | not_retryable | Application bug; surfaces via 500. |

### `zpay-x402::error::WireError` and `zpay-mpp::error::WireError`

Per-adapter wire errors that map onto each protocol's error vocabulary.
The mapping is one-to-one with the `*ProblemType` URI in the RFC 9457
Problem Details document.

## Adding a new error variant

1. Add the variant to the relevant Rust enum.
2. Add a row to the table above.
3. Add a unit test asserting the HTTP mapping (if it crosses a wire
   boundary).
4. If the variant introduces a new retry posture or operator hint, add
   a runbook entry under `docs/runbooks/`.
5. PR title: `errors: <crate>: <variant>` (lowercase).
