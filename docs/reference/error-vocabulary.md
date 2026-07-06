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

At the x402 boundary a typed error renders as an
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

Produced by `propose`; rendered at `POST /x402/v2/prepare`.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `PayeeUnknown { payee_id }` | not_retryable | 404 | `payee_unknown` | Caller named a payee the registry does not hold; register it or use an existing id. |
| `SchemeNetworkUnsupported { payee_id, scheme, network }` | not_retryable | 422 | `scheme_network_unsupported` | Registered payee has no `accepts[]` entry for the requested scheme on that network. |
| `ExpiryHeightInvalid` | requires_operator | 502 | `tip_oracle_zero_tip` | Chain-tip oracle returned zero; point the runtime at a healthy chain plane. |
| `TipOracle(TipError)` | inherits | 502 | `tip_oracle_unavailable` | Chain-tip oracle unreachable; check the `/readyz` chain dependency. |
| `Storage(StoreError)` | inherits | 503 | `prepared_store_unavailable` | Prepared-tx store unreachable; check the `/readyz` store dependency. |

### `zpay_core::settle::SettleError`

Produced by `submit_settlement`; rendered at `POST /x402/v2/settle`.

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

Produced by `verify`; rendered at `POST /x402/v2/verify`. In-band
verdicts (malformed, invalid signature, inconclusive) flow through the
`VerifyResponse` body; only transport-class failures reach this enum.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `PayloadInvalid { reason }` | not_retryable | 422 | `disclosure_payload_invalid` | `disclosure_payload_hex` is not valid hex; the caller must resubmit. |

### `zpay_core::tip::TipError`

Produced by the chain-tip oracle; rendered at `GET /x402/v2/tip` and
wrapped by `PrepareError::TipOracle`.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `Unavailable { reason }` | retryable | 502 | `tip_oracle_unavailable` | Chain-tip oracle unreachable; check the `/readyz` chain dependency. |
| `NetworkUnsupported { network }` | not_retryable | 422 | `network_unsupported` | Oracle does not serve the requested network; check the runtime's configured network. |

### `zpay_x402::dpop::DpopError`

Produced while verifying the DPoP proof on authenticated routes; every
variant renders as 401.

| Variant | Retry | HTTP | `kind` | Operator hint |
|---------|-------|------|--------|---------------|
| `Missing` | not_retryable | 401 | `dpop_missing` | No `DPoP` header on a route that requires one. |
| `InvalidProof { reason }` | not_retryable | 401 | `dpop_invalid_proof` | Proof failed structural or signature checks. |
| `ClockSkew { drift_seconds }` | retryable | 401 | `dpop_clock_skew` | Proof timestamp drift exceeds tolerance; the caller resigns with a corrected clock. |
| `Replay` | not_retryable | 401 | `dpop_replay` | Proof was already used; the caller mints a fresh proof. |

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
