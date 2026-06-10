//! Account bootstrap for the `serve` entrypoint.
//!
//! `init` seals a fresh BIP-39 seed at `$ZSPEND_SEALED_SEED_PATH` and lets the
//! zally storage migrations stand up `wallet.db`, but the underlying schema
//! has no account row until one is materialised from the unsealed seed plus a
//! chain anchor. Without that row, [`zally_wallet::WalletBuilder::open`] short-
//! circuits with [`zally_wallet::WalletError::AccountNotFound`] and the
//! container restart-loops on a fresh volume.
//!
//! This module wraps [`zally_wallet::WalletBuilder::open_or_create_account`]:
//! when the account row already exists the chain source is never contacted;
//! when it does not, the bootstrap reads the chain tip from a
//! [`zally_chain::ZinderChainSource`] (with an env-var override and a sensible
//! testnet default fallback) and materialises the account at that height.

use std::sync::Arc;

use zally_chain::{ChainSource, ZinderChainSource, ZinderRemoteOptions};
use zally_core::{AccountId, BlockHeight, Network};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions};
use zally_wallet::{Wallet, WalletError};

/// Mainnet birthday fallback.
///
/// Used when no env-var override is set and the chain plane is unreachable at
/// bootstrap time. Picked close to the activation of the Orchard pool so the
/// wallet doesn't waste scan work on pre-shielded history.
pub(crate) const DEFAULT_MAINNET_BIRTHDAY: u32 = 2_500_000;

/// Testnet birthday fallback.
///
/// Used when no env-var override is set and the chain plane is unreachable at
/// bootstrap time. Roughly tracks recent testnet tip so a fresh wallet doesn't
/// scan a long, empty history before reaching the tip.
pub(crate) const DEFAULT_TESTNET_BIRTHDAY: u32 = 4_047_000;

/// Regtest birthday fallback.
///
/// Regtest chains restart from genesis, so the bootstrap anchors at height 1
/// (the wallet builder reads the tree state at `birthday - 1`, which is 0 /
/// genesis).
pub(crate) const DEFAULT_REGTEST_BIRTHDAY: u32 = 1;

/// Inputs the bootstrap needs to open or materialise the wallet account.
pub(crate) struct BootstrapInputs {
    pub network: Network,
    pub sealed_seed_path: std::path::PathBuf,
    pub storage_path: std::path::PathBuf,
    pub indexer_grpc_addr: Option<String>,
    pub birthday_override: Option<u32>,
    pub chain_source_factory: ChainSourceFactory,
}

/// Strategy for building the chain source the bootstrap consults when the
/// account row is missing.
///
/// `Live` is the production path: parse the configured zinder endpoint URI
/// (lazy gRPC channel; no network round-trip at construction time) and use
/// it both to probe the live chain tip and to anchor the account.
///
/// `Custom` is a test injection point: pass any [`ChainSource`] (e.g.
/// `zally_testkit::MockChainSource`) so the bootstrap can be exercised
/// without standing up a real zinder.
pub(crate) enum ChainSourceFactory {
    Live,
    #[allow(
        dead_code,
        reason = "constructed only from #[cfg(test)] code; the runtime binary only uses Live"
    )]
    Custom(Arc<dyn ChainSource>),
}

/// Errors returned by [`bootstrap`].
#[derive(Debug, thiserror::Error)]
pub(crate) enum BootstrapError {
    #[error("wallet open failed: {source}")]
    WalletOpen {
        #[source]
        source: WalletError,
    },
    #[error("wallet sync after open failed: {source}")]
    WalletSync {
        #[source]
        source: WalletError,
    },
    #[error(
        "ZSPEND_CHAIN_SOURCE_URL is unset; required to materialise the wallet account on a fresh volume"
    )]
    IndexerAddrMissing,
    #[error("invalid ZSPEND_CHAIN_SOURCE_URL {endpoint:?}: {source}")]
    IndexerAddrInvalid {
        endpoint: String,
        #[source]
        source: zally_chain::ChainSourceError,
    },
}

/// Opens the wallet, creating the single account row from the sealed seed
/// when storage has none yet.
///
/// Idempotent: the second call surfaces the same `AccountId` and does not
/// contact the chain source. The first call resolves a wallet birthday in
/// this order: the `ZSPEND_BIRTHDAY_HEIGHT` override (parsed from
/// [`BootstrapInputs::birthday_override`]), the live chain tip if reachable,
/// otherwise the network-appropriate default ([`DEFAULT_TESTNET_BIRTHDAY`],
/// [`DEFAULT_MAINNET_BIRTHDAY`], or [`DEFAULT_REGTEST_BIRTHDAY`]).
pub(crate) async fn bootstrap(
    inputs: BootstrapInputs,
) -> Result<(Wallet, AccountId), BootstrapError> {
    let BootstrapInputs {
        network,
        sealed_seed_path,
        storage_path,
        indexer_grpc_addr,
        birthday_override,
        chain_source_factory,
    } = inputs;

    let chain: Arc<dyn ChainSource> = match chain_source_factory {
        ChainSourceFactory::Live => Arc::new(build_zinder_chain_source(
            network,
            indexer_grpc_addr.as_deref(),
        )?),
        ChainSourceFactory::Custom(custom) => custom,
    };

    let birthday = resolve_birthday(network, birthday_override, chain.as_ref()).await;
    tracing::info!(
        birthday_height = birthday.as_u32(),
        sealed_seed_path = %sealed_seed_path.display(),
        storage_path = %storage_path.display(),
        "zspend wallet bootstrap: opening account",
    );

    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(sealed_seed_path));
    let storage = Sqlite::new(SqliteOptions::for_network(network, storage_path));
    let (wallet, account_id) = Wallet::builder(network, sealing, storage)
        .open_or_create_account(chain.as_ref(), birthday)
        .await
        .map_err(|source| BootstrapError::WalletOpen { source })?;

    tracing::info!(
        account_id = ?account_id,
        "zspend wallet bootstrap: catching up to chain tip",
    );
    let mut total_blocks = 0u64;
    let mut iterations = 0u32;
    loop {
        let outcome = wallet
            .sync(chain.as_ref())
            .await
            .map_err(|source| BootstrapError::WalletSync { source })?;
        total_blocks = total_blocks.saturating_add(outcome.block_count);
        iterations = iterations.saturating_add(1);
        if outcome.block_count == 0 {
            tracing::info!(
                iterations,
                total_blocks_scanned = total_blocks,
                "zspend wallet bootstrap: sync caught up to chain tip",
            );
            break;
        }
    }

    Ok((wallet, account_id))
}

fn build_zinder_chain_source(
    network: Network,
    indexer_grpc_addr: Option<&str>,
) -> Result<ZinderChainSource, BootstrapError> {
    let endpoint = indexer_grpc_addr
        .map(str::trim)
        .filter(|raw| !raw.is_empty())
        .ok_or(BootstrapError::IndexerAddrMissing)?
        .to_owned();
    ZinderChainSource::connect_remote(ZinderRemoteOptions {
        endpoint: endpoint.clone(),
        network,
    })
    .map_err(|source| BootstrapError::IndexerAddrInvalid { endpoint, source })
}

async fn resolve_birthday(
    network: Network,
    override_height: Option<u32>,
    chain: &dyn ChainSource,
) -> BlockHeight {
    if let Some(height) = override_height {
        tracing::info!(
            birthday_source = "env_override",
            birthday_height = height,
            "ZSPEND_BIRTHDAY_HEIGHT pinned the wallet birthday",
        );
        return BlockHeight::from(height);
    }
    match chain.chain_tip().await {
        Ok(tip) => {
            tracing::info!(
                birthday_source = "chain_tip",
                birthday_height = tip.as_u32(),
                "chain tip resolved as wallet birthday",
            );
            tip
        }
        Err(err) => {
            let fallback = default_birthday(network);
            tracing::warn!(
                birthday_source = "default_fallback",
                birthday_height = fallback,
                reason = %err,
                "chain tip unreachable; using network-default birthday",
            );
            BlockHeight::from(fallback)
        }
    }
}

fn default_birthday(network: Network) -> u32 {
    match network {
        Network::Mainnet => DEFAULT_MAINNET_BIRTHDAY,
        Network::Regtest(_) => DEFAULT_REGTEST_BIRTHDAY,
        // Testnet plus any future non_exhaustive variant share the testnet
        // fallback: dev never panics on a network the runtime doesn't yet
        // recognise. Operators pin the birthday via ZSPEND_BIRTHDAY_HEIGHT
        // when the default is wrong for them.
        Network::Testnet | _ => DEFAULT_TESTNET_BIRTHDAY,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BootstrapInputs, ChainSourceFactory, DEFAULT_REGTEST_BIRTHDAY, bootstrap, default_birthday,
        resolve_birthday,
    };
    use crate::init;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;
    use zally_chain::ChainSourceError;
    use zally_core::{BlockHeight, Network};
    use zally_testkit::MockChainSource;

    struct WalletPaths {
        _dir: TempDir,
        sealed_seed: PathBuf,
        storage: PathBuf,
    }

    impl WalletPaths {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let dir = tempfile::tempdir()?;
            let sealed_seed = dir.path().join("wallet.age");
            let storage = dir.path().join("wallet.db");
            Ok(Self {
                _dir: dir,
                sealed_seed,
                storage,
            })
        }
    }

    fn inputs(
        paths: &WalletPaths,
        network: Network,
        factory: ChainSourceFactory,
        birthday_override: Option<u32>,
    ) -> BootstrapInputs {
        BootstrapInputs {
            network,
            sealed_seed_path: paths.sealed_seed.clone(),
            storage_path: paths.storage.clone(),
            indexer_grpc_addr: None,
            birthday_override,
            chain_source_factory: factory,
        }
    }

    #[tokio::test]
    async fn bootstrap_materialises_account_on_fresh_volume()
    -> Result<(), Box<dyn std::error::Error>> {
        let paths = WalletPaths::new()?;
        let network = Network::regtest();
        init::run(paths.sealed_seed.clone(), false).await?;

        let mock = Arc::new(MockChainSource::new(network));
        let (_wallet, first_account_id) = bootstrap(inputs(
            &paths,
            network,
            ChainSourceFactory::Custom(mock),
            Some(DEFAULT_REGTEST_BIRTHDAY),
        ))
        .await?;

        let warm_mock = Arc::new(MockChainSource::new(network));
        let (_wallet_again, second_account_id) = bootstrap(inputs(
            &paths,
            network,
            ChainSourceFactory::Custom(warm_mock),
            None,
        ))
        .await?;

        assert_eq!(
            first_account_id, second_account_id,
            "warm boot must reuse the account row materialised by the cold boot",
        );
        Ok(())
    }

    #[tokio::test]
    async fn bootstrap_idempotent_under_repeated_cold_construction()
    -> Result<(), Box<dyn std::error::Error>> {
        let paths = WalletPaths::new()?;
        let network = Network::regtest();
        init::run(paths.sealed_seed.clone(), false).await?;

        let mock = Arc::new(MockChainSource::new(network));
        let (_w1, first) = bootstrap(inputs(
            &paths,
            network,
            ChainSourceFactory::Custom(Arc::clone(&mock) as Arc<dyn zally_chain::ChainSource>),
            Some(DEFAULT_REGTEST_BIRTHDAY),
        ))
        .await?;
        let (_w2, second) = bootstrap(inputs(
            &paths,
            network,
            ChainSourceFactory::Custom(Arc::clone(&mock) as Arc<dyn zally_chain::ChainSource>),
            Some(99_999),
        ))
        .await?;

        assert_eq!(
            first, second,
            "open_or_create_account must surface the prior account on warm storage",
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_birthday_falls_back_to_default_when_chain_unreachable()
    -> Result<(), Box<dyn std::error::Error>> {
        let network = Network::Testnet;
        let chain = MockChainSource::new(network);
        chain
            .handle()
            .fail_chain_tip_next(1, || ChainSourceError::Unavailable {
                reason: "chain plane unreachable".to_owned(),
            });
        let resolved = resolve_birthday(network, None, &chain).await;
        assert_eq!(
            resolved,
            BlockHeight::from(default_birthday(network)),
            "unreachable chain plane must fall back to the network-default birthday",
        );
        Ok(())
    }

    #[tokio::test]
    async fn resolve_birthday_prefers_env_override_over_chain_tip()
    -> Result<(), Box<dyn std::error::Error>> {
        let network = Network::regtest();
        let mock = MockChainSource::new(network);
        let resolved = resolve_birthday(network, Some(123_456), &mock).await;
        assert_eq!(
            resolved,
            BlockHeight::from(123_456),
            "env override must take priority over the live chain tip",
        );
        Ok(())
    }
}
