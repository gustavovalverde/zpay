//! `zspend-runtime init` subcommand: provision the sealed wallet seed.
//!
//! Two modes seal into the same age-based [`AgeFileSealing`] path that `serve`
//! opens at startup. The default generates a fresh BIP-39 mnemonic and reveals
//! it once so the operator can back it up offline. `--restore` reads a mnemonic
//! on stdin and seals the seed it derives, so restoring the same phrase rebuilds
//! the same wallet.
//!
//! Safety posture: refuses to overwrite an existing sealed seed unless the
//! caller passes `--force`. Sealed-seed and identity-sidecar files are
//! chmod'd to `0600` on Unix after writing so the on-disk surface matches
//! the operator expectation for secret material.

use std::path::{Path, PathBuf};

use zally_keys::{
    AgeFileSealing, AgeFileSealingOptions, Mnemonic, MnemonicError, SealingError, SeedMaterial,
    SeedSealing as _,
};

/// Errors returned by [`run`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum InitError {
    #[error(
        "sealed seed already exists at {path}; refusing to overwrite. Pass --force to replace it."
    )]
    AlreadyExists { path: PathBuf },
    #[error("could not probe sealed-seed path {path}: {source}")]
    Probe {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not remove existing sealed seed at {path}: {source}")]
    RemoveExisting {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not remove existing age identity sidecar at {path}: {source}")]
    RemoveIdentity {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("could not read mnemonic from stdin: {source}")]
    StdinRead {
        #[source]
        source: std::io::Error,
    },
    #[error("mnemonic read on stdin is not a valid BIP-39 phrase: {source}")]
    InvalidMnemonic {
        #[source]
        source: MnemonicError,
    },
    #[error("seed sealing failed: {source}")]
    Seal {
        #[source]
        source: SealingError,
    },
    #[error("could not tighten permissions on {path}: {source}")]
    Chmod {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}

/// Provision the sealed seed at `sealed_seed_path`.
///
/// When `restore` is false, generates a fresh mnemonic and seals it. An
/// operator-run init (`auto_provision` false) then prints the mnemonic once so
/// it can be backed up offline; the docker auto-provision path
/// (`auto_provision` true) suppresses the reveal and logs only that an
/// unbacked dev seed was provisioned. When `restore` is true, reads a mnemonic
/// on stdin and seals the seed it derives without revealing anything (the
/// operator already holds it). When `force` is false and the file already
/// exists, returns [`InitError::AlreadyExists`] without touching disk.
pub(crate) async fn run(
    sealed_seed_path: PathBuf,
    force: bool,
    restore: bool,
    auto_provision: bool,
) -> Result<(), InitError> {
    if restore {
        let phrase = read_mnemonic_from_stdin().await?;
        restore_from_phrase(sealed_seed_path, force, &phrase).await
    } else {
        let mnemonic = Mnemonic::generate();
        seal_mnemonic(sealed_seed_path, force, &mnemonic).await?;
        match post_seal_action(auto_provision) {
            PostSealAction::RevealMnemonic => reveal_mnemonic(&mnemonic),
            PostSealAction::LogUnbackedProvision => tracing::info!(
                "provisioned an unbacked throwaway dev seed (ZSPEND_ALLOW_AUTO_PROVISION); the mnemonic is not revealed and this wallet has no backup",
            ),
        }
        Ok(())
    }
}

/// What to do after sealing a freshly generated seed.
#[derive(Debug, PartialEq, Eq)]
enum PostSealAction {
    /// Print the mnemonic once so an operator can back it up offline.
    RevealMnemonic,
    /// Suppress the reveal for the docker auto-provision path and log only
    /// that an unbacked dev seed was provisioned.
    LogUnbackedProvision,
}

/// Decide the post-seal action. The docker auto-provision path never reveals
/// the mnemonic: no operator is watching the terminal to record it, and the
/// wallet is a throwaway dev seed.
const fn post_seal_action(auto_provision: bool) -> PostSealAction {
    if auto_provision {
        PostSealAction::LogUnbackedProvision
    } else {
        PostSealAction::RevealMnemonic
    }
}

/// Seal the seed derived from `phrase`, validating it as BIP-39 first.
pub(crate) async fn restore_from_phrase(
    sealed_seed_path: PathBuf,
    force: bool,
    phrase: &str,
) -> Result<(), InitError> {
    let mnemonic = Mnemonic::from_phrase(phrase.trim())
        .map_err(|source| InitError::InvalidMnemonic { source })?;
    seal_mnemonic(sealed_seed_path, force, &mnemonic).await
}

/// Seal `mnemonic` at `sealed_seed_path`, handling the overwrite policy and
/// tightening on-disk permissions.
async fn seal_mnemonic(
    sealed_seed_path: PathBuf,
    force: bool,
    mnemonic: &Mnemonic,
) -> Result<(), InitError> {
    let existed = sealed_seed_path
        .try_exists()
        .map_err(|source| InitError::Probe {
            path: sealed_seed_path.clone(),
            source,
        })?;

    if existed {
        if !force {
            return Err(InitError::AlreadyExists {
                path: sealed_seed_path,
            });
        }
        std::fs::remove_file(&sealed_seed_path).map_err(|source| InitError::RemoveExisting {
            path: sealed_seed_path.clone(),
            source,
        })?;
        let identity_path = identity_sidecar_path(&sealed_seed_path);
        match std::fs::remove_file(&identity_path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(source) => {
                return Err(InitError::RemoveIdentity {
                    path: identity_path,
                    source,
                });
            }
        }
        tracing::info!(
            sealed_seed_path = %sealed_seed_path.display(),
            "removed existing sealed seed before re-init (--force)",
        );
    }

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(sealed_seed_path.clone()));
    let seed = SeedMaterial::from_mnemonic(mnemonic, "");
    sealing
        .seal_seed(&seed)
        .await
        .map_err(|source| InitError::Seal { source })?;

    tighten_permissions(&sealed_seed_path)?;
    let identity_path = identity_sidecar_path(&sealed_seed_path);
    if identity_path
        .try_exists()
        .map_err(|source| InitError::Probe {
            path: identity_path.clone(),
            source,
        })?
    {
        tighten_permissions(&identity_path)?;
    }

    tracing::info!(
        sealed_seed_path = %sealed_seed_path.display(),
        identity_path = %identity_path.display(),
        replaced = existed,
        "zspend-runtime init sealed the wallet seed",
    );
    Ok(())
}

/// Reveal the freshly generated mnemonic exactly once so the operator backs it
/// up offline. This is the only backup; the runtime never displays it again.
#[allow(
    clippy::print_stdout,
    reason = "seed material must never enter the structured log pipeline that ships to aggregators; the one-time mnemonic reveal is written straight to the operator terminal"
)]
fn reveal_mnemonic(mnemonic: &Mnemonic) {
    println!("WALLET SEED BACKUP");
    println!(
        "Write this BIP-39 mnemonic down and store it offline now. It is the ONLY backup of this \
         wallet, it will not be shown again, and anyone who reads it can spend the funds. Restore \
         it later with `zspend-runtime init --restore`.",
    );
    println!("word count: {}", mnemonic.word_count());
    println!("{}", mnemonic.as_phrase());
}

async fn read_mnemonic_from_stdin() -> Result<String, InitError> {
    let joined = tokio::task::spawn_blocking(|| {
        let mut buffer = String::new();
        std::io::Read::read_to_string(&mut std::io::stdin(), &mut buffer).map(|_| buffer)
    })
    .await;
    match joined {
        Ok(Ok(buffer)) => Ok(buffer),
        Ok(Err(source)) => Err(InitError::StdinRead { source }),
        Err(join_err) => Err(InitError::StdinRead {
            source: std::io::Error::other(join_err.to_string()),
        }),
    }
}

fn identity_sidecar_path(sealed_seed_path: &Path) -> PathBuf {
    let mut s = sealed_seed_path.to_path_buf().into_os_string();
    s.push(".age-identity");
    s.into()
}

#[cfg(unix)]
fn tighten_permissions(path: &Path) -> Result<(), InitError> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o600);
    std::fs::set_permissions(path, perms).map_err(|source| InitError::Chmod {
        path: path.to_path_buf(),
        source,
    })
}

#[cfg(not(unix))]
fn tighten_permissions(_path: &Path) -> Result<(), InitError> {
    // No-op on non-Unix: the runtime is only shipped in a Linux container.
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        InitError, PostSealAction, identity_sidecar_path, post_seal_action, restore_from_phrase,
        run,
    };
    use tempfile::tempdir;
    use zally_keys::{AgeFileSealing, AgeFileSealingOptions, SeedSealing as _};

    #[test]
    fn auto_provision_suppresses_the_reveal() {
        assert_eq!(
            post_seal_action(true),
            PostSealAction::LogUnbackedProvision,
            "the docker auto-provision path must not reveal the mnemonic",
        );
        assert_eq!(
            post_seal_action(false),
            PostSealAction::RevealMnemonic,
            "an operator-run init must reveal the mnemonic once",
        );
    }

    #[tokio::test]
    async fn auto_provision_seals_a_seed_without_revealing()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("wallet.age");
        run(path.clone(), false, false, true).await?;
        assert!(path.exists(), "auto-provision must still seal a seed");
        Ok(())
    }

    /// Canonical all-zero-entropy 24-word BIP-39 vector.
    const KNOWN_PHRASE: &str = "abandon abandon abandon abandon abandon abandon abandon abandon \
        abandon abandon abandon abandon abandon abandon abandon abandon abandon abandon \
        abandon abandon abandon abandon abandon art";

    #[tokio::test]
    async fn init_creates_sealed_seed_with_tight_permissions()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("wallet.age");

        run(path.clone(), false, false, false).await?;

        assert!(path.exists(), "sealed seed file was not written");
        let identity_path = identity_sidecar_path(&path);
        assert!(
            identity_path.exists(),
            "age identity sidecar was not written"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let seed_mode = std::fs::metadata(&path)?.permissions().mode() & 0o777;
            let identity_mode = std::fs::metadata(&identity_path)?.permissions().mode() & 0o777;
            assert_eq!(seed_mode, 0o600, "sealed seed mode should be 0600");
            assert_eq!(identity_mode, 0o600, "identity sidecar mode should be 0600");
        }
        Ok(())
    }

    #[tokio::test]
    async fn init_refuses_to_overwrite_without_force() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("wallet.age");

        run(path.clone(), false, false, false).await?;
        let first_bytes = std::fs::read(&path)?;

        let outcome = run(path.clone(), false, false, false).await;
        assert!(matches!(outcome, Err(InitError::AlreadyExists { .. })));

        let second_bytes = std::fs::read(&path)?;
        assert_eq!(
            first_bytes, second_bytes,
            "sealed seed must not change when --force is absent",
        );
        Ok(())
    }

    #[tokio::test]
    async fn init_force_overwrites_existing_seed() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("wallet.age");

        run(path.clone(), false, false, false).await?;
        let first_bytes = std::fs::read(&path)?;

        run(path.clone(), true, false, false).await?;
        let second_bytes = std::fs::read(&path)?;

        assert_ne!(
            first_bytes, second_bytes,
            "--force should write a freshly generated sealed seed",
        );
        Ok(())
    }

    #[tokio::test]
    async fn restore_round_trip_yields_identical_seed_material()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let first_path = dir.path().join("first.age");
        let second_path = dir.path().join("second.age");

        restore_from_phrase(first_path.clone(), false, KNOWN_PHRASE).await?;
        restore_from_phrase(second_path.clone(), false, KNOWN_PHRASE).await?;

        let first_seed = AgeFileSealing::new(AgeFileSealingOptions::at_path(first_path))
            .unseal_seed()
            .await?;
        let second_seed = AgeFileSealing::new(AgeFileSealingOptions::at_path(second_path))
            .unseal_seed()
            .await?;

        assert_eq!(
            first_seed.expose_secret(),
            second_seed.expose_secret(),
            "restoring the same phrase must derive identical seed material",
        );
        Ok(())
    }

    #[tokio::test]
    async fn restore_rejects_an_invalid_phrase() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("wallet.age");

        let outcome = restore_from_phrase(path.clone(), false, "not a valid mnemonic").await;
        assert!(matches!(outcome, Err(InitError::InvalidMnemonic { .. })));
        assert!(!path.exists(), "an invalid phrase must not seal a seed");
        Ok(())
    }
}
