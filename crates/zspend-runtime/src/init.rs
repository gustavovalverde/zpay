//! `zspend-runtime init` subcommand: generate a fresh BIP-39 mnemonic, derive
//! its seed material, and seal it via the same age-based [`AgeFileSealing`]
//! path that `serve` opens at startup.
//!
//! The subcommand exists so operators can boot a wallet image without
//! manually provisioning a sealed seed (Phase 4 follow-on, Proposal-0003):
//! a freshly mounted volume with no seed at `$ZSPEND_SEALED_SEED_PATH` is
//! one `zspend-runtime init` away from being usable.
//!
//! Safety posture: refuses to overwrite an existing sealed seed unless the
//! caller passes `--force`. Sealed-seed and identity-sidecar files are
//! chmod'd to `0600` on Unix after writing so the on-disk surface matches
//! the operator expectation for secret material.

use std::path::{Path, PathBuf};

use zally_keys::{
    AgeFileSealing, AgeFileSealingOptions, Mnemonic, SealingError, SeedMaterial, SeedSealing,
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

/// Generate a fresh sealed seed at `sealed_seed_path`.
///
/// When `force` is false and the file already exists, returns
/// [`InitError::AlreadyExists`] without touching disk. When `force` is true
/// and the file exists, the prior seed and its identity sidecar are removed
/// before the new pair is written so the age sealing implementation generates
/// a fresh identity rather than reusing the prior one.
pub(crate) async fn run(sealed_seed_path: PathBuf, force: bool) -> Result<(), InitError> {
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
    let mnemonic = Mnemonic::generate();
    let seed = SeedMaterial::from_mnemonic(&mnemonic, "");
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
        "zspend-runtime init sealed a fresh wallet seed",
    );
    Ok(())
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
    use super::{InitError, identity_sidecar_path, run};
    use tempfile::tempdir;

    #[tokio::test]
    async fn init_creates_sealed_seed_with_tight_permissions()
    -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempdir()?;
        let path = dir.path().join("wallet.age");

        run(path.clone(), false).await?;

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

        run(path.clone(), false).await?;
        let first_bytes = std::fs::read(&path)?;

        let outcome = run(path.clone(), false).await;
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

        run(path.clone(), false).await?;
        let first_bytes = std::fs::read(&path)?;

        run(path.clone(), true).await?;
        let second_bytes = std::fs::read(&path)?;

        assert_ne!(
            first_bytes, second_bytes,
            "--force should write a freshly generated sealed seed",
        );
        Ok(())
    }
}
