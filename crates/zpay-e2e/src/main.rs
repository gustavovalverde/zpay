//! End-to-end testnet validator for the zpay facilitator.
//!
//! Drives a real zally wallet through the full lifecycle a payment
//! agent would: `/prepare` against a running zpay-runtime, compose a
//! [`Memo::Arbitrary`] from the prepared protocol memo, propose and
//! sign the spend via zally, hand the raw transaction bytes to a custom
//! [`Submitter`] that POSTs to `/x402/v2/settle`, then poll
//! `/x402/v2/payments/{payment_id}` until the confirmation oracle
//! observes the transaction mine.
//!
//! The harness is intentionally synchronous from the operator's point
//! of view: one binary, one terminal, one wallet directory on disk.
//! Funding happens out-of-band through fauzec; the harness prints a
//! u-address and exits gracefully when the balance is too low.

use std::path::PathBuf;
use std::time::Duration;

use async_trait::async_trait;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;
use zally_chain::{
    ChainSource, ChainSourceError, SubmitOutcome, Submitter, SubmitterError, ZinderChainSource,
    ZinderRemoteOptions,
};
use zally_core::{
    AccountId, BlockHeight, IdempotencyKey, Memo, MemoBytes, Network, PaymentRecipient, TxId,
    Zatoshis,
};
use zally_keys::{AgeFileSealing, AgeFileSealingOptions};
use zally_storage::{Sqlite, SqliteOptions};
use zally_wallet::{SendPaymentPlan, Wallet};
use zpay_core::prepare::PROTOCOL_MEMO_BYTE_COUNT;

/// CLI entry point.
#[derive(Debug, Parser)]
#[command(name = "zpay-e2e", version, about)]
struct Cli {
    /// Path to the wallet data directory. The harness writes the age
    /// identity, sealed seed, and libSQL DB here.
    #[arg(long, env = "ZPAY_E2E_WALLET_DIR")]
    wallet_dir: PathBuf,
    /// Zinder `WalletQuery` gRPC endpoint for the chain source.
    #[arg(
        long,
        env = "ZPAY_E2E_ZINDER_URL",
        default_value = "http://127.0.0.1:19101"
    )]
    zinder_url: String,
    /// zpay-runtime base URL (the `/x402/v2/*` routes are appended).
    #[arg(
        long,
        env = "ZPAY_E2E_ZPAY_URL",
        default_value = "http://127.0.0.1:7402"
    )]
    zpay_url: String,
    /// Birthday height the wallet starts scanning from on first run.
    /// Use a recent testnet tip minus a small margin so initial sync is
    /// fast. Ignored if the wallet already exists at `wallet_dir`.
    #[arg(long, env = "ZPAY_E2E_BIRTHDAY", default_value_t = 4_031_000)]
    birthday: u32,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Print the wallet's first unified address. Use this to fund the
    /// wallet via fauzec or another faucet.
    Address,
    /// Sync the wallet against the zinder chain source and print the
    /// resulting account balance.
    Status,
    /// Run the full e2e flow: prepare against zpay, propose with the
    /// prepared memo, submit through `/settle`, poll until the
    /// confirmation oracle bumps the count.
    Run {
        /// Merchant id registered in zpay's accepts registry.
        #[arg(long, default_value = "aether-ai")]
        merchant_id: String,
        /// Unified address the prepared tx will pay. May be the same
        /// wallet's second u-address; the harness only cares that the
        /// tx broadcasts successfully.
        #[arg(long)]
        recipient_address: Option<String>,
        /// Amount in zatoshis the prepared tx will move.
        #[arg(long, default_value_t = 10_000)]
        amount_zat: u64,
        /// Validity window for the prepared row.
        #[arg(long, default_value_t = 600)]
        validity_seconds: u64,
        /// Maximum seconds to wait for the oracle to observe a
        /// confirmation before exiting.
        #[arg(long, default_value_t = 600)]
        poll_seconds: u64,
    },
}

#[tokio::main]
async fn main() -> Result<(), HarnessError> {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("zpay_e2e=info,zally=info")),
        )
        .try_init();

    let cli = Cli::parse();
    std::fs::create_dir_all(&cli.wallet_dir).map_err(HarnessError::WalletDir)?;

    let network = Network::Testnet;
    let chain = ZinderChainSource::connect_remote(ZinderRemoteOptions {
        endpoint: cli.zinder_url.clone(),
        network,
    })?;

    let (wallet, account_id) =
        open_or_bootstrap_wallet(network, &cli.wallet_dir, &chain, BlockHeight::from(cli.birthday))
            .await?;
    let params = network.to_parameters();

    match cli.command {
        Command::Address => {
            let ua = wallet.derive_next_address(account_id).await?;
            let encoded = ua.encode(&params);
            info!(unified_address = %encoded, "wallet unified address (fund this via fauzec)");
        }
        Command::Status => {
            let outcome = wallet.sync(&chain).await?;
            info!(
                scanned_to_height = outcome.scanned_to_height.as_u32(),
                block_count = outcome.block_count,
                "sync complete",
            );
            let balance = wallet.get_account_balance(account_id).await?;
            info!(
                sapling_zat = balance.sapling_zat.as_u64(),
                orchard_zat = balance.orchard_zat.as_u64(),
                transparent_mature_zat = balance.transparent_mature_zat.as_u64(),
                "account balance",
            );
        }
        Command::Run {
            merchant_id,
            recipient_address,
            amount_zat,
            validity_seconds,
            poll_seconds,
        } => {
            run_flow(
                &wallet,
                account_id,
                &chain,
                &cli.zpay_url,
                merchant_id,
                recipient_address,
                amount_zat,
                validity_seconds,
                poll_seconds,
                network,
            )
            .await?;
        }
    }
    Ok(())
}

async fn open_or_bootstrap_wallet(
    network: Network,
    wallet_dir: &std::path::Path,
    chain: &ZinderChainSource,
    birthday: BlockHeight,
) -> Result<(Wallet, AccountId), HarnessError> {
    let seed_path = wallet_dir.join("wallet.age");
    let db_path = wallet_dir.join("wallet.db");
    let bootstrap_needed = !seed_path.exists();
    let sealing = AgeFileSealing::new(AgeFileSealingOptions::at_path(seed_path.clone()));
    let storage = Sqlite::new(SqliteOptions::for_network(network, db_path.clone()));

    if bootstrap_needed {
        // First run for this wallet directory: generate a mnemonic and
        // seal it before opening the account. Log the mnemonic once so
        // the operator can back it up before depositing TAZ.
        let (wallet, account_id, mnemonic) = Wallet::builder(network, sealing, storage)
            .create(chain, birthday)
            .await?;
        warn!(
            mnemonic = mnemonic.as_phrase(),
            "fresh wallet created; back up this mnemonic before depositing TAZ",
        );
        return Ok((wallet, account_id));
    }
    let pair = Wallet::builder(network, sealing, storage)
        .open_or_create_account(chain, birthday)
        .await?;
    Ok(pair)
}

#[allow(clippy::too_many_arguments, reason = "one-shot harness; splitting would obscure the linear flow")]
async fn run_flow(
    wallet: &Wallet,
    account_id: AccountId,
    chain: &ZinderChainSource,
    zpay_url: &str,
    merchant_id: String,
    recipient_address: Option<String>,
    amount_zat: u64,
    validity_seconds: u64,
    poll_seconds: u64,
    network: Network,
) -> Result<(), HarnessError> {
    info!("syncing wallet against zinder before propose");
    let outcome = wallet.sync(chain).await?;
    info!(
        scanned_to_height = outcome.scanned_to_height.as_u32(),
        block_count = outcome.block_count,
        "sync complete",
    );

    let balance = wallet.get_account_balance(account_id).await?;
    info!(
        sapling_zat = balance.sapling_zat.as_u64(),
        orchard_zat = balance.orchard_zat.as_u64(),
        transparent_mature_zat = balance.transparent_mature_zat.as_u64(),
        "account balance after sync",
    );
    let spendable = balance.total_zat().as_u64();
    if spendable < amount_zat + 5_000 {
        warn!(
            spendable_zat = spendable,
            requested_zat = amount_zat,
            "wallet does not have enough spendable balance; fund the address printed by the `address` subcommand and retry",
        );
        return Err(HarnessError::InsufficientFunds {
            spendable_zat: spendable,
            requested_zat: amount_zat,
        });
    }

    let recipient = if let Some(addr) = recipient_address {
        addr
    } else {
        let ua = wallet.derive_next_address(account_id).await?;
        ua.encode(&network.to_parameters())
    };
    info!(recipient = %recipient, "recipient unified address");

    let prepared = call_prepare(zpay_url, &merchant_id, &recipient, amount_zat, validity_seconds)
        .await?;
    info!(
        payment_id = %prepared.payment_id,
        expiry_height = prepared.expiry_height,
        "prepared row received from zpay",
    );

    let memo = memo_from_protocol_prefix(&prepared.memo_bytes)?;
    let submitter = ZpaySettleSubmitter::new(
        zpay_url.to_owned(),
        prepared.payment_id.clone(),
        chain.network(),
    );
    let plan = SendPaymentPlan::conventional(
        account_id,
        IdempotencyKey::try_from(prepared.payment_id.as_str())
            .map_err(|err| HarnessError::Idempotency(err.to_string()))?,
        PaymentRecipient::UnifiedAddress {
            encoded: recipient.clone(),
            network,
        },
        Zatoshis::try_from(amount_zat).map_err(|err| HarnessError::Zat(err.to_string()))?,
        &submitter,
    )
    .with_memo(memo);

    info!("invoking wallet.send_payment with custom zpay submitter");
    let send_outcome = wallet.send_payment(plan).await?;
    info!(
        tx_id = %hex::encode(send_outcome.tx_id.as_bytes()),
        broadcast_at_height = send_outcome.broadcast_at_height.as_u32(),
        "send_payment returned",
    );

    poll_until_confirmed(zpay_url, &prepared.payment_id, poll_seconds).await?;
    Ok(())
}

async fn poll_until_confirmed(
    zpay_url: &str,
    payment_id: &str,
    poll_seconds: u64,
) -> Result<(), HarnessError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(poll_seconds);
    let client = reqwest::Client::new();
    let url = format!("{}/x402/v2/payments/{}", zpay_url.trim_end_matches('/'), payment_id);
    let mut last_status = String::new();
    loop {
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|err| HarnessError::Http(err.to_string()))?;
        let body: PaymentStatusEnvelope = response
            .json()
            .await
            .map_err(|err| HarnessError::Http(err.to_string()))?;
        let summary = format!(
            "status={} confirmation_count={:?} mined_height={:?}",
            body.data.status, body.data.confirmation_count, body.data.mined_block_height,
        );
        if summary != last_status {
            info!(?summary, "payment status");
            last_status = summary;
        }
        if body.data.confirmation_count.unwrap_or(0) >= 1 {
            info!("first confirmation observed; harness exiting");
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(HarnessError::PollTimedOut);
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

async fn call_prepare(
    zpay_url: &str,
    merchant_id: &str,
    recipient_unified_address: &str,
    amount_zat: u64,
    validity_seconds: u64,
) -> Result<PreparedPayment, HarnessError> {
    let body = PrepareRequestBody {
        merchant_id: merchant_id.to_owned(),
        network: "testnet".to_owned(),
        scheme: "zcash".to_owned(),
        recipient_unified_address: recipient_unified_address.to_owned(),
        amount_zat,
        challenge_hash: vec![0x11; 32],
        resource_hash: vec![0x22; 32],
        evidence_pack_hash: vec![0x33; 32],
        expiry_height: 4_032_000,
        validity_seconds,
        idempotency_key: None,
    };
    let client = reqwest::Client::new();
    let response = client
        .post(format!("{}/x402/v2/prepare", zpay_url.trim_end_matches('/')))
        .json(&body)
        .send()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(HarnessError::PrepareFailed { status, body: text });
    }
    let envelope: PrepareResponseEnvelope = response
        .json()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    Ok(envelope.data)
}

fn memo_from_protocol_prefix(memo_bytes: &[u8]) -> Result<Memo, HarnessError> {
    if memo_bytes.len() != PROTOCOL_MEMO_BYTE_COUNT {
        return Err(HarnessError::MemoLength {
            len: memo_bytes.len(),
        });
    }
    // ZIP-302: 0xFF declares an Arbitrary memo whose remaining 511
    // bytes carry application-defined data. The prepared protocol memo
    // is exactly the leading 98 bytes (tag + version + three hashes);
    // the remaining 413 bytes are zero padding.
    let mut buf = [0u8; 512];
    buf[..PROTOCOL_MEMO_BYTE_COUNT].copy_from_slice(memo_bytes);
    let memo_bytes = MemoBytes::from_bytes(&buf).map_err(|err| HarnessError::MemoCompose {
        reason: err.to_string(),
    })?;
    Memo::try_from(&memo_bytes).map_err(|err| HarnessError::MemoCompose {
        reason: err.to_string(),
    })
}

/// Custom `Submitter` that hands the signed transaction bytes to zpay's
/// `/x402/v2/settle` endpoint instead of broadcasting directly. The
/// payment id was issued by the same zpay-runtime at prepare time.
struct ZpaySettleSubmitter {
    zpay_url: String,
    payment_id: String,
    network: Network,
    client: reqwest::Client,
}

impl ZpaySettleSubmitter {
    fn new(zpay_url: String, payment_id: String, network: Network) -> Self {
        Self {
            zpay_url,
            payment_id,
            network,
            client: reqwest::Client::new(),
        }
    }
}

#[async_trait]
impl Submitter for ZpaySettleSubmitter {
    fn network(&self) -> Network {
        self.network
    }

    async fn submit(&self, raw_tx: &[u8]) -> Result<SubmitOutcome, SubmitterError> {
        let body = SettleRequestBody {
            payment_id: self.payment_id.clone(),
            raw_tx_hex: hex::encode(raw_tx),
        };
        let response = self
            .client
            .post(format!(
                "{}/x402/v2/settle",
                self.zpay_url.trim_end_matches('/')
            ))
            .json(&body)
            .send()
            .await
            .map_err(|err| SubmitterError::Unavailable {
                reason: err.to_string(),
            })?;
        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(SubmitterError::Unavailable {
                reason: format!("zpay /settle returned {status}: {text}"),
            });
        }
        let envelope: SettleResponseEnvelope =
            response
                .json()
                .await
                .map_err(|err| SubmitterError::Unavailable {
                    reason: err.to_string(),
                })?;
        zpay_outcome_to_submit_outcome(envelope.data)
    }
}

fn zpay_outcome_to_submit_outcome(
    outcome: SettlementResponseData,
) -> Result<SubmitOutcome, SubmitterError> {
    let tx_id_hex = outcome.broadcast_outcome.transaction_id.unwrap_or_default();
    let tx_id_bytes = decode_txid(&tx_id_hex)?;
    let tx_id = TxId::from_bytes(tx_id_bytes);
    match outcome.broadcast_outcome.outcome.as_str() {
        "accepted" => Ok(SubmitOutcome::Accepted { tx_id }),
        "duplicate" => Ok(SubmitOutcome::Duplicate { tx_id }),
        kind => Err(SubmitterError::Unavailable {
            reason: format!("zpay broadcast outcome was non-success: {kind}"),
        }),
    }
}

fn decode_txid(tx_id_hex: &str) -> Result<[u8; 32], SubmitterError> {
    let bytes = hex::decode(tx_id_hex).map_err(|err| SubmitterError::Unavailable {
        reason: format!("zpay broadcast outcome txid not hex: {err}"),
    })?;
    bytes.try_into().map_err(|_| SubmitterError::Unavailable {
        reason: "zpay broadcast outcome txid was not 32 bytes".to_owned(),
    })
}

/// Wire types for `/x402/v2/prepare`.

#[derive(Debug, Serialize)]
struct PrepareRequestBody {
    merchant_id: String,
    network: String,
    scheme: String,
    recipient_unified_address: String,
    amount_zat: u64,
    challenge_hash: Vec<u8>,
    resource_hash: Vec<u8>,
    evidence_pack_hash: Vec<u8>,
    expiry_height: u32,
    validity_seconds: u64,
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PrepareResponseEnvelope {
    data: PreparedPayment,
}

#[derive(Debug, Deserialize)]
struct PreparedPayment {
    payment_id: String,
    #[serde(default)]
    #[allow(dead_code, reason = "wire-shape mirror of zpay response; not all fields are read by the harness")]
    payment_uri: String,
    memo_bytes: Vec<u8>,
    expiry_height: u32,
}

/// Wire types for `/x402/v2/settle`.

#[derive(Debug, Serialize)]
struct SettleRequestBody {
    payment_id: String,
    raw_tx_hex: String,
}

#[derive(Debug, Deserialize)]
struct SettleResponseEnvelope {
    data: SettlementResponseData,
}

#[derive(Debug, Deserialize)]
struct SettlementResponseData {
    #[allow(dead_code, reason = "wire-shape mirror of zpay response; not all fields are read by the harness")]
    payment_id: String,
    broadcast_outcome: BroadcastOutcomeBody,
    #[allow(dead_code, reason = "wire-shape mirror of zpay response; not all fields are read by the harness")]
    watch_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BroadcastOutcomeBody {
    outcome: String,
    transaction_id: Option<String>,
    #[serde(default)]
    #[allow(dead_code, reason = "wire-shape mirror of zpay response; not all fields are read by the harness")]
    upstream_message: Option<String>,
}

/// Wire types for `/x402/v2/payments/{payment_id}`.

#[derive(Debug, Deserialize)]
struct PaymentStatusEnvelope {
    data: PaymentStatusData,
}

#[derive(Debug, Deserialize)]
struct PaymentStatusData {
    #[allow(dead_code, reason = "wire-shape mirror of zpay response; not all fields are read by the harness")]
    payment_id: String,
    status: String,
    #[serde(default)]
    confirmation_count: Option<u32>,
    #[serde(default)]
    mined_block_height: Option<u64>,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
enum HarnessError {
    #[error("wallet directory create failed: {0}")]
    WalletDir(#[source] std::io::Error),
    #[error("wallet error: {0}")]
    Wallet(#[from] zally_wallet::WalletError),
    #[error("chain source error: {0}")]
    ChainSource(#[from] ChainSourceError),
    #[error("zpay /prepare returned {status}: {body}")]
    PrepareFailed {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("http error: {0}")]
    Http(String),
    #[error(
        "insufficient funds: spendable={spendable_zat}, requested={requested_zat} (+ ~5000 zat fee)"
    )]
    InsufficientFunds {
        spendable_zat: u64,
        requested_zat: u64,
    },
    #[error("prepared memo_bytes length expected {PROTOCOL_MEMO_BYTE_COUNT}, got {len}")]
    MemoLength { len: usize },
    #[error("memo compose failed: {reason}")]
    MemoCompose { reason: String },
    #[error("idempotency key invalid: {0}")]
    Idempotency(String),
    #[error("zatoshi amount invalid: {0}")]
    Zat(String),
    #[error("polling /payments/{{payment_id}} timed out before a confirmation was observed")]
    PollTimedOut,
}

