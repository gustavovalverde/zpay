# Error Vocabulary

Every typed error that zpay returns at any boundary is registered here.
Each entry carries: the variant name, the layer that produces it, the
retry posture, the HTTP status code (when produced at a wire boundary),
and a one-line operator hint.

The Rust enums are the source of truth; this table mirrors them. Each
wire-boundary variant maps to a `kind` code and an HTTP status in the
x402 adapter. A PR that adds or renames a variant updates both the enum
and this table in the same change.

## Retry posture vocabulary

- **`retryable`**: the same request, retried, may succeed without the
  caller doing anything. Chain-plane blips, chain-tip oracle
  unavailable, store connection exhaustion, DPoP clock skew inside a
  resignable window.
- **`not_retryable`**: the caller's inputs are wrong. Retrying with the
  same inputs produces the same error. The caller must change the
  request before retrying.
- **`requires_operator`**: the zpay operator must act before the next
  retry can succeed. Schema migration pending, chain-tip oracle
  reporting a zero tip, a stored row that fails to deserialize.

## Wire envelope

At the JSON wire boundary a typed error renders as an
`application/problem+json` document with four fields: `title`, `kind`,
`detail`, and `retryable`. The `kind` is the machine-readable code a
consumer branches on.

### Rate limit (429)

The DPoP-`jkt` and client-IP rate limiters return this when a key exceeds
its fixed-window budget (see
[operational-surfaces.md](../architecture/operational-surfaces.md)).

| Field | Value |
|-------|-------|
| HTTP | 429 Too Many Requests |
| `kind` | `rate_limited` |
| `title` | `Too Many Requests` |
| `detail` | `per-key request rate limit exceeded; retry after the window resets` |
| `retryable` | `true` |
| Header | `Retry-After: <seconds until the window rolls over>` |

## Errors by layer

### `zpay_core::prepare::PrepareError`

Produced by `propose`; rendered at `POST /zpay/v1/prepare`.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `PayeeUnknown { payee_id }` | not_retryable | 404 | `payee_unknown` | Caller named a payee the registry does not hold; register it or use an existing id. |
| `SchemeNetworkUnsupported { payee_id, scheme, network }` | not_retryable | 422 | `scheme_network_unsupported` | Registered payee has no `accepts[]` entry for the requested scheme on that network. |
| `ExpiryHeightInvalid` | requires_operator | 502 | `tip_oracle_zero_tip` | Chain-tip oracle returned zero; point the runtime at a healthy chain plane. |
| `TipOracle(TipError)` | inherits | 502 | `tip_oracle_unavailable` | Chain-tip oracle unreachable; check the `/readyz` chain dependency. |
| `Storage(StoreError)` | inherits | 503 | `prepared_store_unavailable` | Prepared-tx store unreachable; check the `/readyz` store dependency. |

### `zpay_core::settle::SettleError`

Produced by `submit_settlement`; rendered at `POST /zpay/v1/settle`.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `PreparationNotFound { payment_id }` | not_retryable | 404 | `preparation_not_found` | `payment_id` does not exist or the preparation already settled. |
| `RawTxHexInvalid` | not_retryable | 422 | `raw_tx_hex_invalid` | `raw_tx_hex` is empty or not hex; the wallet must resubmit valid hex. |
| `ChainUnavailable { reason }` | retryable | 502 | `chain_unavailable` | Chain plane could not accept the broadcast; retry once it recovers. |
| `TransactionMalformed { reason }` | not_retryable | 422 | `transaction_malformed` | `raw_tx_hex` is hex-shaped but not a Zcash transaction; the wallet must rebuild it. |
| `ExpiryHeightMismatch { prepared_expiry_height, signed_expiry_height }` | not_retryable | 422 | `expiry_height_mismatch` | Signed tx targets a different prepared row; rebuild against this `payment_id`. |
| `ObsoleteMemoVersion { observed }` | not_retryable | 409 | `obsolete_memo_version` | Cached memo version predates this build; re-prepare against this runtime. |
| `DpopMismatch` | not_retryable | 403 | `dpop_mismatch` | Settle presented a different DPoP key than prepare; a foreign agent tried to settle. |
| `Storage(StoreError)` | inherits | 503 | `settle_store_unavailable` | Settle store unreachable; check the `/readyz` store dependency. |

### `zpay_core::verify::VerifyError`

Produced by `verify`; rendered at `POST /zpay/v1/verify`. In-band
verdicts (malformed, invalid signature, inconclusive) flow through the
`VerifyResponse` body; only transport-class failures reach this enum.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `PayloadInvalid { reason }` | not_retryable | 422 | `disclosure_payload_invalid` | `disclosure_payload_hex` is not valid hex; the caller must resubmit. |

### `zpay_core::tip::TipError`

Produced by the chain-tip oracle; rendered at `GET /zpay/v1/tip` and
wrapped by `PrepareError::TipOracle`.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `Unavailable { reason }` | retryable | 502 | `tip_oracle_unavailable` | Chain-tip oracle unreachable; check the `/readyz` chain dependency. |
| `NetworkUnsupported { network }` | not_retryable | 422 | `network_unsupported` | Oracle does not serve the requested network; check the runtime's configured network. |

### `zpay_x402` official facilitator reasons

Produced by unauthenticated `POST /x402/v2/verify` and
`POST /x402/v2/settle`. These are official x402 response fields
(`invalidReason` or `errorReason`), not RFC 7807 `kind` strings.

| Reason | Retry | HTTP | Operator hint |
|--------|-------|------|---------------|
| `x402_version_unsupported` | not_retryable | 200 | Client and facilitator are using different x402 protocol versions. |
| `payment_requirements_mismatch` | not_retryable | 200 | The selected payment requirements do not equal the requirements sent for verification. |
| `scheme_network_not_supported` | not_retryable | 200 | The facilitator does not support this scheme and network pair. |
| `zcash_exact_scheme_invalid` | not_retryable | 200 | Zcash exact requirements used a scheme other than `exact`. |
| `zcash_exact_network_invalid` | not_retryable | 200 | Zcash exact requirements used an unknown Zcash network identifier. |
| `zcash_exact_asset_unsupported` | not_retryable | 200 | Zcash exact requirements must use `asset: "ZEC"`. |
| `zcash_exact_amount_invalid` | not_retryable | 200 | Zcash exact requirements must use a positive integer zatoshi amount. |
| `zcash_exact_pay_to_invalid` | not_retryable | 200 | Zcash exact requirements must use a ZIP-316 Unified Address prefix matching the network. |
| `zcash_exact_authorization_malformed` | not_retryable | 200 | Zcash exact authorization must be an object with `format` and base64url PCZT bytes. |
| `zcash_exact_authorization_format_unsupported` | not_retryable | 200 | Zcash exact authorization must use `format: "pczt-v2-extractable"`. |
| `zcash_exact_pczt_malformed` | not_retryable | 200 | PCZT bytes did not parse. |
| `zcash_exact_network_mismatch` | not_retryable | 200 | Requested network does not match the PCZT or configured chain plane. |
| `zcash_exact_pay_to_mismatch` | not_retryable | 200 | PCZT labelled payment recipient does not match `payTo`. |
| `zcash_exact_amount_mismatch` | not_retryable | 200 | PCZT labelled payment amount does not match `amount`. |
| `zcash_exact_pczt_not_verifiable` | not_retryable | 200 | PCZT omitted fields needed for safe effect verification, or used an unsupported labelled output form. |
| `zcash_exact_pczt_not_extractable` | not_retryable | 200 | PCZT verification passed but transaction extraction failed. |
| `zcash_exact_transaction_invalid_encoding` | not_retryable | 200 | Chain plane rejected the extracted transaction bytes as invalid encoding. |
| `zcash_exact_transaction_rejected` | not_retryable | 200 | Chain plane rejected the extracted transaction under consensus or policy rules. |
| `zcash_exact_settlement_unknown` | retryable | 200 | Chain plane did not return a determinate broadcast outcome. |
| `zpay_payment_id_invalid` | not_retryable | 200 | `extra.zpayPaymentId` was present but not a valid zpay payment id. |
| `zpay_payment_not_prepared` | not_retryable | 200 | `extra.zpayPaymentId` named no active prepared row. |
| `zpay_payment_requirements_mismatch` | not_retryable | 200 | The prepared row's amount, recipient, or network did not match the x402 requirements. |
| `zpay_payment_expiry_mismatch` | not_retryable | 200 | The extracted PCZT expiry height did not match the prepared row. |
| `zpay_payment_store_unavailable` | retryable | 200 | The prepared-row store could not be read or updated while linking x402 settlement. |
| `zpay_payment_ledger_unavailable` | retryable | 200 | The settlement ledger could not record a broadcast outcome after x402 settlement. |
| `chain_unavailable` | retryable | 200 | Chain plane could not be reached during x402 settlement. |

### `zpay_x402::dpop::DpopError`

Produced while verifying the DPoP proof on authenticated routes; every
variant renders as 401.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `Missing` | not_retryable | 401 | `dpop_missing` | No `DPoP` header on a route that requires one. |
| `InvalidProof { reason }` | not_retryable | 401 | `dpop_invalid_proof` | Proof failed structural or signature checks. |
| `ClockSkew { drift_seconds }` | retryable | 401 | `dpop_clock_skew` | Proof timestamp drift exceeds tolerance; the caller resigns with a corrected clock. |
| `Replay` | not_retryable | 401 | `dpop_replay` | Proof was already used; the caller mints a fresh proof. |

### `zspend_core::ProblemKind`

Produced by `zspend-runtime`; rendered at `POST /v1/payments/sign`.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `PaymentRequestInvalid` | not_retryable | 400 | `payment_request_invalid` | The ZIP-321 request is malformed or names the wrong network. |
| `DpopProofInvalid` | not_retryable | 401 | `dpop_proof_invalid` | The caller must mint a fresh DPoP proof. |
| `AccessTokenInvalid` | not_retryable | 401 | `access_token_invalid` | The caller must re-authenticate and obtain a valid `payment_authorization`. |
| `TokenRevoked` | not_retryable | 401 | `token_revoked` | The issuer revoked this access-token `jti`; request a new authorization. |
| `IntentMismatch` | not_retryable | 403 | `intent_mismatch` | The signed RAR tuple does not match the parsed payment request. |
| `RecipientMismatch` | not_retryable | 403 | `recipient_mismatch` | The recipient differs from the signed RAR tuple. |
| `AmountExceeded` | not_retryable | 403 | `amount_exceeded` | The requested amount exceeds the wallet's local backstop cap. |
| `AudienceMismatch` | not_retryable | 403 | `audience_mismatch` | The access token was minted for another wallet audience. |
| `TokenAlreadyConsumed` | not_retryable | 409 | `token_already_consumed` | This access-token `jti` was already used for a different intent. |
| `TargetExpiryStale` | not_retryable | 409 | `target_expiry_stale` | The prepared expiry is stale; prepare again and sign a new request. |
| `AuthorizationExpired` | not_retryable | 410 | `authorization_expired` | The authorization's block-height expiry has passed. |
| `InsufficientFunds` | not_retryable | 422 | `insufficient_funds` | Fund the wallet and wait for sync. |
| `RarTooManyEntries` | not_retryable | 422 | `rar_too_many_entries` | v1 accepts exactly one `payment_authorization` RAR entry. |
| `SeedUnavailable` | requires_operator | 503 | `seed_unavailable` | Restore or unseal the wallet seed. |
| `ChainUnreachable` | retryable | 503 | `chain_unreachable` | The wallet chain plane is unreachable. |
| `RevocationCacheStale` | retryable | 503 | `revocation_cache_stale` | The revocation cache cannot be proven fresh. |
| `WalletUnavailable` | retryable | 503 | `wallet_unavailable` | Wallet sync is stale, catching up, recovering, or parked; check `/readyz.wallet_sync`. |
| `NotReady` | retryable | 503 | `not_ready` | A transient signer precondition is not satisfied. |
| `TargetExpiryMismatchInternal` | requires_operator | 500 | `target_expiry_mismatch_internal` | The wallet signed a transaction with the wrong expiry height. |

### `zpay_core::oracle::OracleError`

Internal to the confirmation path. The `ConfirmationOracle` surfaces it
to the background subscription task; it never crosses a wire boundary.

| Variant | Retry | Notes |
|---------|-------|-------|
| `Unavailable { reason }` | retryable | Chain plane unreachable; the subscription retries. |
| `ResponseMalformed { reason }` | requires_operator | Chain plane responded but the payload was uninterpretable. |

### `zpay_core::store::StoreError`

Internal; never crosses a wire boundary directly. `PrepareError::Storage`
and `SettleError::Storage` wrap it and choose the outward HTTP status.

| Variant | Retry | Notes |
|---------|-------|-------|
| `Unavailable { reason }` | retryable | libSQL pool exhausted or remote unreachable. |
| `MigrationPending { current_version, required_version }` | requires_operator | Operator must run the migration runner before queries succeed. |
| `IntegrityViolation { constraint }` | not_retryable | Foreign-key, uniqueness, or check-constraint violation. |
| `RowMalformed { reason }` | requires_operator | Stored row did not deserialize; schema drift or corruption. |

## Adding a new error variant

1. Add the variant to the relevant Rust enum.
2. Add a row to the table above.
3. Add a unit test asserting the HTTP mapping (if it crosses a wire
   boundary).
4. If the variant introduces a new retry posture or operator hint, add
   a runbook entry under `docs/runbooks/`.
5. PR title: `errors: <crate>: <variant>` (lowercase).
