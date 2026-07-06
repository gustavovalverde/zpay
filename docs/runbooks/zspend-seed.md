# Wallet seed lifecycle: zspend-runtime

Operator reference for provisioning, backing up, and restoring the wallet
seed that `zspend-runtime` signs with. The wallet holds spending keys; a lost
seed with no backup is lost funds. Read this before running `init` against a
wallet that will hold real value.

## Where the seed lives

`zspend-runtime` seals its seed at `ZSPEND_SEALED_SEED_PATH` (in the compose
stack, `/var/lib/zspend/wallet.age`). Sealing uses an `age` identity written
to a sidecar next to it (`wallet.age.age-identity`). Both files are chmod
`0600` on Unix. They sit on the wallet's persistent volume (`zspend-data`),
alongside the wallet storage database (`ZSPEND_STORAGE_PATH`) and the
single-use ledger (`ZSPEND_LEDGER_URL`, default `usage-ledger.db` beside the
storage path). A remote (`libsql://`) ledger URL requires
`ZSPEND_LEDGER_AUTH_TOKEN`; startup refuses to open an unauthenticated remote
connection when the token is unset. A file-backed ledger ignores the token.

The sealed seed and its age identity are a pair: the sealed file is
ciphertext, and the identity sidecar is the key that opens it. Losing either
one makes the on-disk seed unrecoverable.

## Init ceremony and the one-time reveal

Provision a wallet a human will own:

```text
zspend-runtime init
```

This generates a fresh BIP-39 mnemonic, seals the seed it derives, and then
prints the mnemonic to the terminal **exactly once**, written straight to
stdout and never through the log pipeline. That reveal is the only backup the
runtime will ever produce.

When you see it:

1. Write the mnemonic down on paper (or another offline medium). Do not
   screenshot it, paste it into a chat, or store it on the same host.
2. Confirm the word count matches what was printed.
3. Store it offline. Anyone who reads it can spend the wallet's funds.

`init` refuses to overwrite an existing sealed seed unless you pass `--force`.
`--force` generates a brand-new seed and discards the old identity sidecar, so
only use it when you intend to replace the wallet.

## Restore

Rebuild the same wallet from a mnemonic backup:

```text
zspend-runtime init --restore
```

`--restore` reads a mnemonic on stdin and seals the seed it derives, revealing
nothing (you already hold the phrase). Restoring the same phrase always
derives the same seed material, so the restored wallet controls the same
funds. An invalid BIP-39 phrase is rejected and nothing is sealed.

## The auto-provision gate (docker dev only)

The container entrypoint (`docker/start-zspend.sh`) never provisions a seed on
its own unless `ZSPEND_ALLOW_AUTO_PROVISION=1` is set. When it is, and no
sealed seed exists, the entrypoint runs `zspend-runtime init --auto-provision`,
which seals a throwaway dev seed **without revealing the mnemonic**: no
operator is watching the container's terminal to record it, so revealing it
would only leak a secret into the logs.

The compose dev stack sets `ZSPEND_ALLOW_AUTO_PROVISION=1` on purpose so the
wallet boots unattended. The seed it provisions has no backup and is
disposable. Never set `ZSPEND_ALLOW_AUTO_PROVISION` for a wallet that will
hold real funds; provision those with `init` (record the mnemonic) or
`init --restore` (seal a backed-up phrase).

## Posture gate

`SeedSealing` reports a posture, surfaced so an operator can see how the seed
is protected at rest.

| Env / surface | Behavior |
|---------------|----------|
| `ZSPEND_ALLOW_DEV_SEED=1` | Required to serve a `Dev`-posture (age-file) seal. Without it, startup refuses with a dev-seed-posture error. |
| `/readyz` | Reports `sealed_seed` (`dev` / `hsm` / `kms`) and the operational `posture`. |
| `/metrics` | Exposes `zspend_seal_posture_info{posture}`. |

A `production` operational posture additionally refuses to start without
`ZSPEND_ISSUER_URL` (so token revocation is enforced) and a non-empty issuer
JWKS (`ZSPEND_JWKS_FILE`). The age-file seal is the dev posture; a KMS or HSM
sealing reports `hsm`/`kms` and needs no override.

## Losing the volume or the identity

The sealed seed only decrypts with its age identity sidecar, and both live on
the `zspend-data` volume. If you lose the volume, or delete or corrupt the
`.age-identity` sidecar, the on-disk seed cannot be opened and the wallet's
funds are unrecoverable **unless** you hold the offline mnemonic backup.

Recovery in that case is exactly the restore path: provision a fresh volume
and run `zspend-runtime init --restore` with the backed-up mnemonic. This is
why the one-time reveal must be recorded offline for any wallet holding real
value, and why the auto-provisioned dev seed (no reveal, no backup) is only
ever for throwaway testnet use.
