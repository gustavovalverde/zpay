# Proposal-0003: fauzec `CaptchaMode::Bearer`

| Field | Value |
| ----- | ----- |
| Status | Proposed |
| Consumer | zpay (test infrastructure) |
| Upstream | fauzec |
| Pinned at | n/a (HTTP-only dependency) |
| Related | [PRD-42 Phase 1](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md), [Upstream platform binding](../architecture/upstream-platform-binding.md) |

## Context

fauzec's `CaptchaMode` enum (`fauzec/crates/fauzec-core/src/lib.rs`) currently has two variants:

- `Turnstile`: Cloudflare Turnstile token validation.
- `LocalTest`: accepts any non-empty token string, used by dev and CI.

`LocalTest` is the only programmatic-access path today and is unsafe to ship in any environment reachable from the open internet. Agentic-payment dev flows (zpay's T3 live tests, zentity's MCP demo) need a way to claim testnet ZEC programmatically against a deployment that is reachable from CI.

## Ask

Add a third `CaptchaMode` variant:

```rust
pub enum CaptchaMode {
    Turnstile { /* ... */ },
    LocalTest,
    Bearer {
        allowed_key_hashes: Vec<KeyHash>,
    },
}
```

Where `KeyHash` is a SHA-256 hash of the raw bearer key with a per-deployment salt. The raw key never persists; the operator generates a key, hashes it, stores the hash in the fauzec config, and shares the raw key with consumers out-of-band.

Verification arm in `fauzec-runtime/src/dispense.rs::verify_captcha`:

```rust
CaptchaMode::Bearer { allowed_key_hashes } => {
    let presented_hash = sha256_with_salt(presented_token, deployment_salt);
    if !allowed_key_hashes.iter().any(|expected| constant_time_eq(expected, &presented_hash)) {
        return Err(ErrorCode::CaptchaInvalid);
    }
    Ok(())
}
```

The web layer (`apps/fauzec-web/lib/hono-app.ts`) accepts an `Authorization: Bearer <token>` header and forwards `<token>` to the runtime over the existing private gRPC. The runtime hashes and compares; the web layer never touches the allowlist.

`GET /api/v1/network` surfaces `supported_auth_modes: ["turnstile", "bearer"]` so consumers can discover the available paths.

## Why this lives in fauzec, not zpay

The captcha layer is fauzec's. Adding a sibling auth proxy in front of fauzec (or in zpay) would duplicate fauzec's existing rate-limit, cooldown, and claim-tracking infrastructure.

## Compatibility

Additive. `Turnstile` and `LocalTest` arms unchanged. Existing deployments that do not configure `Bearer` keep behaving identically.

## Acceptance

- `CaptchaMode::Bearer { allowed_key_hashes }` exists on the public surface.
- `verify_captcha` covers the variant with a constant-time compare.
- `apps/fauzec-web` accepts the `Authorization: Bearer` header.
- `GET /api/v1/network` includes `supported_auth_modes`.
- A T1 integration test asserts: valid bearer claim returns 200 with a txid; invalid bearer returns 401.

Once accepted: zpay's testnet smoke tests configure a per-CI bearer key, set `ZPAY_TEST_FAUCET_HTTP_ADDR`, and call fauzec programmatically.
