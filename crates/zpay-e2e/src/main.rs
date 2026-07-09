//! End-to-end testnet validator for the zpay facilitator.
//!
//! Drives a real zally wallet through the full lifecycle a payment
//! agent would: `/prepare` against a running zpay-runtime, compose a
//! [`Memo::Arbitrary`] from the prepared protocol memo, propose and
//! sign the spend via zally, or ask zspend for a signed PCZT and settle
//! that PCZT through the official `/x402/v2/settle` facilitator route.
//!
//! The harness is intentionally synchronous from the operator's point
//! of view: one binary, one terminal, one wallet directory on disk.
//! Funding happens out-of-band through fauzec; the harness prints a
//! u-address and exits gracefully when the balance is too low.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use clap::{Parser, Subcommand, ValueEnum};
use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
use p256::ecdsa::SigningKey;
use p256::pkcs8::{EncodePrivateKey as _, LineEnding};
use rand_core::OsRng;
use serde::{Deserialize, Serialize};
use sha2::{Digest as _, Sha256};
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
use zally_wallet::{PaymentRequest, SendPaymentPlan, Wallet};
use zpay_core::prepare::{PROTOCOL_MEMO_BYTE_COUNT, PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE};
use zspend_core::{
    Amount, AmountUnit, ChainId, ExpiresAt, IntentHashString, PaymentAuthorization,
    PaymentAuthorizationType, recompute_intent_hash,
};

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
    /// zpay-runtime base URL (the `/zpay/v1/*` routes are appended).
    #[arg(
        long,
        env = "ZPAY_E2E_ZPAY_URL",
        default_value = "http://127.0.0.1:7402"
    )]
    zpay_url: String,
    /// zspend-runtime base URL (the `/v1/payments/sign` route is appended).
    #[arg(
        long,
        env = "ZPAY_E2E_ZSPEND_URL",
        default_value = "http://127.0.0.1:8090"
    )]
    zspend_url: String,
    /// Birthday height the wallet starts scanning from on first run.
    /// Use a recent tip minus a small margin so initial sync is fast.
    /// Ignored if the wallet already exists at `wallet_dir`.
    #[arg(long, env = "ZPAY_E2E_BIRTHDAY", default_value_t = 4_031_000)]
    birthday: u32,
    /// Network name passed in `/prepare` requests and used to bind the
    /// zally wallet. Accepts `testnet` or `regtest`.
    #[arg(long, env = "ZPAY_E2E_NETWORK", default_value = "testnet")]
    network: String,
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
    /// Shield matured transparent funds into Orchard notes by submitting
    /// a shielding transaction directly to zinder (bypasses zpay). Run
    /// this once after funding the wallet from a coinbase miner so the
    /// `Run` flow has shielded notes to spend from.
    Shield {
        /// Minimum transparent value to shield (zatoshis).
        #[arg(long, default_value_t = 5_000_000)]
        shielding_threshold_zat: u64,
    },
    /// Run the full e2e flow: prepare against zpay, propose with the
    /// prepared memo, submit through `/settle`, poll until the
    /// confirmation oracle bumps the count.
    Run {
        /// Payee id registered in zpay's accepts registry.
        #[arg(long, default_value = "aether-ai")]
        payee_id: String,
        /// Unified address the prepared tx will pay. May be the same
        /// wallet's second u-address; the harness only cares that the
        /// tx broadcasts successfully. Informational: the registry's
        /// `pay_to` is the authoritative target.
        #[arg(long)]
        recipient_address: Option<String>,
        /// Amount in zatoshis used to gate the wallet's balance before
        /// prepare. Informational: the registry's `amount_zat` is the
        /// authoritative value that flows through the URI.
        #[arg(long, default_value_t = 10_000)]
        amount_zat: u64,
        /// Lifecycle state zpay must observe before the command exits.
        #[arg(long, value_enum, default_value_t = SettlementCompletion::Mined)]
        settlement_completion: SettlementCompletion,
        /// Maximum seconds to wait for the requested lifecycle state.
        #[arg(long, default_value_t = 600)]
        poll_seconds: u64,
    },
    /// Run the agent-signed flow through zspend: prepare in zpay,
    /// authorize and sign via `/v1/payments/sign`, settle in zpay,
    /// then poll confirmation and zexplorer visibility.
    AgentRun {
        /// Payee id registered in zpay's accepts registry.
        #[arg(long, default_value = "aether-ai")]
        payee_id: String,
        /// Audience URI zspend expects in the access token.
        #[arg(long, env = "ZPAY_E2E_ZSPEND_AUDIENCE")]
        audience: String,
        /// Ed25519 issuer private key in PKCS#8 PEM or DER format. zspend
        /// must be running with a JWKS containing the matching public key.
        #[arg(long, env = "ZPAY_E2E_ISSUER_KEY_PATH")]
        issuer_key_path: PathBuf,
        /// Issuer JWKS key id.
        #[arg(long, env = "ZPAY_E2E_ISSUER_KID", default_value = "zpay-e2e-dev")]
        issuer_kid: String,
        /// Public zspend URL used for the DPoP `htu` claim. Defaults to
        /// `--zspend-url`.
        #[arg(long, env = "ZPAY_E2E_ZSPEND_PUBLIC_URL")]
        zspend_public_url: Option<String>,
        /// Maximum seconds the minted authorization remains valid.
        #[arg(long, default_value_t = 120)]
        token_ttl_seconds: u64,
        /// Lifecycle state zpay must observe before the command exits.
        #[arg(long, value_enum, default_value_t = SettlementCompletion::Mined)]
        settlement_completion: SettlementCompletion,
        /// Maximum seconds to wait for the requested lifecycle state.
        #[arg(long, default_value_t = 600)]
        poll_seconds: u64,
        /// Base URL for the external transaction visibility check.
        #[arg(
            long,
            env = "ZPAY_E2E_ZEXPLORER_TX_URL",
            default_value = "https://zexplorer.app/testnet/tx"
        )]
        zexplorer_tx_url: String,
    },
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum SettlementCompletion {
    /// A mined transaction with at least one confirmation.
    Mined,
    /// The configured finality depth is reached.
    Final,
    /// Zinder's settled tip has passed the mined transaction block.
    Settled,
}

impl SettlementCompletion {
    const fn label(self) -> &'static str {
        match self {
            Self::Mined => "mined",
            Self::Final => "final",
            Self::Settled => "settled",
        }
    }

    fn is_observed(self, payment_status: &PaymentStatusData) -> bool {
        match self {
            Self::Mined => payment_status.confirmation_count.unwrap_or(0) >= 1,
            Self::Final => payment_status.status == "final",
            Self::Settled => payment_status.settled,
        }
    }
}

struct HarnessContext {
    wallet: Wallet,
    account_id: AccountId,
    chain: ZinderChainSource,
    zpay_url: String,
    zspend_url: String,
    network: Network,
    network_label: String,
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
    let context = build_harness_context(&cli).await?;
    run_command(context, cli.command).await
}

async fn build_harness_context(cli: &Cli) -> Result<HarnessContext, HarnessError> {
    std::fs::create_dir_all(&cli.wallet_dir).map_err(HarnessError::WalletDir)?;

    let network = parse_harness_network(&cli.network)?;
    let network_label = cli.network.clone();
    let chain = ZinderChainSource::connect_remote(ZinderRemoteOptions {
        endpoint: cli.zinder_url.clone(),
        network,
    })?;

    let (wallet, account_id) = open_or_bootstrap_wallet(
        network,
        &cli.wallet_dir,
        &chain,
        BlockHeight::from(cli.birthday),
    )
    .await?;

    Ok(HarnessContext {
        wallet,
        account_id,
        chain,
        zpay_url: cli.zpay_url.clone(),
        zspend_url: cli.zspend_url.clone(),
        network,
        network_label,
    })
}

async fn run_command(context: HarnessContext, command: Command) -> Result<(), HarnessError> {
    let HarnessContext {
        wallet,
        account_id,
        chain,
        zpay_url,
        zspend_url,
        network,
        network_label,
    } = context;

    match command {
        Command::Address => {
            print_wallet_address(&wallet, account_id, network).await?;
        }
        Command::Status => {
            print_wallet_status(&wallet, account_id, &chain).await?;
        }
        Command::Shield {
            shielding_threshold_zat,
        } => {
            shield_funds(&wallet, account_id, &chain, shielding_threshold_zat).await?;
        }
        Command::Run {
            payee_id,
            recipient_address,
            amount_zat,
            settlement_completion,
            poll_seconds,
        } => {
            run_flow(
                &wallet,
                account_id,
                &chain,
                &zpay_url,
                payee_id,
                recipient_address,
                amount_zat,
                settlement_completion,
                poll_seconds,
                network,
                &network_label,
            )
            .await?;
        }
        Command::AgentRun {
            payee_id,
            audience,
            issuer_key_path,
            issuer_kid,
            zspend_public_url,
            token_ttl_seconds,
            settlement_completion,
            poll_seconds,
            zexplorer_tx_url,
        } => {
            run_agent_signed_flow(AgentRunInputs {
                zpay_url: &zpay_url,
                zspend_url: &zspend_url,
                zspend_public_url: zspend_public_url.as_deref(),
                payee_id,
                audience: &audience,
                issuer_key_path: &issuer_key_path,
                issuer_kid: &issuer_kid,
                token_ttl_seconds,
                settlement_completion,
                poll_seconds,
                zexplorer_tx_url: &zexplorer_tx_url,
                network,
                network_label: &network_label,
            })
            .await?;
        }
    }
    Ok(())
}

async fn print_wallet_address(
    wallet: &Wallet,
    account_id: AccountId,
    network: Network,
) -> Result<(), HarnessError> {
    let ua = wallet
        .derive_next_address_with_transparent(account_id)
        .await?;
    let params = network.to_parameters();
    let encoded = ua.encode(&params);
    info!(unified_address = %encoded, "wallet unified address (fund this via fauzec)");
    Ok(())
}

async fn print_wallet_status(
    wallet: &Wallet,
    account_id: AccountId,
    chain: &ZinderChainSource,
) -> Result<(), HarnessError> {
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
        ironwood_zat = balance.ironwood_zat.as_u64(),
        transparent_mature_zat = balance.transparent_mature_zat.as_u64(),
        "account balance",
    );
    Ok(())
}

fn parse_harness_network(raw: &str) -> Result<Network, HarnessError> {
    match raw {
        "testnet" => Ok(Network::Testnet),
        "regtest" => Ok(Network::regtest()),
        other => Err(HarnessError::NetworkInvalid(other.to_owned())),
    }
}

async fn shield_funds(
    wallet: &Wallet,
    account_id: AccountId,
    chain: &ZinderChainSource,
    shielding_threshold_zat: u64,
) -> Result<(), HarnessError> {
    info!("syncing wallet before shielding");
    let outcome = wallet.sync(chain).await?;
    info!(
        scanned_to_height = outcome.scanned_to_height.as_u32(),
        block_count = outcome.block_count,
        "sync complete",
    );
    let submitter = chain.submitter();
    let idempotency_token = format!(
        "zpay-e2e-shield-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    );
    let plan = zally_wallet::ShieldTransparentPlan::new(
        account_id,
        IdempotencyKey::try_from(idempotency_token.as_str())
            .map_err(|err| HarnessError::Idempotency(err.to_string()))?,
        Zatoshis::try_from(shielding_threshold_zat)
            .map_err(|err| HarnessError::Zat(err.to_string()))?,
        &submitter,
    );
    let send_outcome = wallet.shield_transparent_funds(plan).await?;
    info!(
        tx_id = %hex::encode(send_outcome.tx_id().as_bytes()),
        broadcast_at_height = send_outcome.broadcast.broadcast_at_height.as_u32(),
        "shielding tx broadcast",
    );
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

#[allow(
    clippy::too_many_arguments,
    clippy::too_many_lines,
    reason = "one-shot harness; splitting would obscure the linear flow"
)]
async fn run_flow(
    wallet: &Wallet,
    account_id: AccountId,
    chain: &ZinderChainSource,
    zpay_url: &str,
    payee_id: String,
    recipient_address: Option<String>,
    amount_zat: u64,
    settlement_completion: SettlementCompletion,
    poll_seconds: u64,
    network: Network,
    network_label: &str,
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
        ironwood_zat = balance.ironwood_zat.as_u64(),
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
        let ua = wallet
            .derive_next_address_with_transparent(account_id)
            .await?;
        ua.encode(&network.to_parameters())
    };
    info!(recipient = %recipient, "recipient unified address (informational; registry-resolved pay_to is authoritative)");

    // zpay owns the expiry math: it calls its tip oracle and adds
    // DEFAULT_EXPIRY_DELTA_BLOCKS (40, matching zally's wallet
    // DEFAULT_TX_EXPIRY_DELTA). The harness used to pre-compute this
    // and pass it in; commit C removed that knob because zally's
    // chosen expiry must equal whatever zpay returned.
    let chain_tip_height = outcome.scanned_to_height.as_u32();
    info!(
        chain_tip_height,
        informational_expiry = chain_tip_height.saturating_add(41),
        "wallet-side tip for orientation only; zpay's tip oracle is authoritative for expiry_height",
    );
    let zpay_dpop_key = Arc::new(DpopKey::generate()?);
    let prepared = call_prepare(zpay_url, &payee_id, network_label, &zpay_dpop_key).await?;
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
        Arc::clone(&zpay_dpop_key),
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
        tx_id = %hex::encode(send_outcome.tx_id().as_bytes()),
        broadcast_at_height = send_outcome.broadcast.broadcast_at_height.as_u32(),
        "send_payment returned",
    );

    wait_for_settlement_completion(
        zpay_url,
        &prepared.payment_id,
        settlement_completion,
        poll_seconds,
    )
    .await?;
    Ok(())
}

struct AgentRunInputs<'a> {
    zpay_url: &'a str,
    zspend_url: &'a str,
    zspend_public_url: Option<&'a str>,
    payee_id: String,
    audience: &'a str,
    issuer_key_path: &'a std::path::Path,
    issuer_kid: &'a str,
    token_ttl_seconds: u64,
    settlement_completion: SettlementCompletion,
    poll_seconds: u64,
    zexplorer_tx_url: &'a str,
    network: Network,
    network_label: &'a str,
}

async fn run_agent_signed_flow(inputs: AgentRunInputs<'_>) -> Result<(), HarnessError> {
    let zpay_dpop_key = DpopKey::generate()?;
    let prepared = call_prepare(
        inputs.zpay_url,
        &inputs.payee_id,
        inputs.network_label,
        &zpay_dpop_key,
    )
    .await?;
    info!(
        payment_id = %prepared.payment_id,
        expiry_height = prepared.expiry_height,
        "prepared row received from zpay for agent-signed flow",
    );

    let sign_url = format!(
        "{}/v1/payments/sign",
        inputs
            .zspend_public_url
            .unwrap_or(inputs.zspend_url)
            .trim_end_matches('/')
    );
    let call_sign_url = format!(
        "{}/v1/payments/sign",
        inputs.zspend_url.trim_end_matches('/')
    );
    let authorization = build_agent_authorization(&prepared, inputs.network)?;
    let issuer_key = load_issuer_encoding_key(inputs.issuer_key_path)?;
    let dpop_key = DpopKey::generate()?;
    let access_token = mint_access_token(&AccessTokenGrant {
        issuer_key: &issuer_key,
        issuer_kid: inputs.issuer_kid,
        audience: inputs.audience,
        dpop_jkt: &dpop_key.jkt,
        authorization: &authorization,
        token_ttl_seconds: inputs.token_ttl_seconds,
    })?;
    let dpop_proof = dpop_key.mint_access_bound_proof(&access_token, "POST", &sign_url)?;

    let signed = request_zspend_signature(SignPaymentCall {
        call_sign_url: &call_sign_url,
        access_token: &access_token,
        dpop_proof: &dpop_proof,
        prepared: &prepared,
        network_label: inputs.network_label,
    })
    .await?;

    let facilitator_request = build_x402_facilitator_request(
        inputs.network,
        inputs.zpay_url,
        &prepared,
        &signed.pczt_base64,
    )?;
    verify_x402_payment(inputs.zpay_url, &facilitator_request).await?;
    let settled_tx_id = settle_x402_payment(inputs.zpay_url, &facilitator_request).await?;
    if settled_tx_id != signed.tx_id {
        return Err(HarnessError::SignedBytes(format!(
            "x402 settle txid {settled_tx_id} did not match zspend txid {}",
            signed.tx_id
        )));
    }
    confirm_x402_lifecycle_record(inputs.zpay_url, &prepared.payment_id, &settled_tx_id).await?;
    wait_for_settlement_completion(
        inputs.zpay_url,
        &prepared.payment_id,
        inputs.settlement_completion,
        inputs.poll_seconds,
    )
    .await?;
    check_zexplorer(
        inputs.network,
        inputs.zexplorer_tx_url,
        &settled_tx_id,
        inputs.poll_seconds,
    )
    .await?;
    Ok(())
}

fn build_agent_authorization(
    prepared: &PreparedPayment,
    network: Network,
) -> Result<PaymentAuthorization, HarnessError> {
    let parsed = PaymentRequest::from_uri(&prepared.payment_uri, network)?;
    let payment = parsed
        .payments()
        .first()
        .ok_or(HarnessError::PaymentMissing)?;
    let reference = chain_reference(network);
    let recipient_caip10 = format!("zcash:{}:{}", reference, payment.recipient.encoded());
    let mut authorization = PaymentAuthorization {
        authorization_type: PaymentAuthorizationType::PaymentAuthorization,
        chain: ChainId {
            namespace: "zcash".to_owned(),
            reference: reference.to_owned(),
        },
        recipient: recipient_caip10.clone(),
        amount: Amount {
            currency: "ZEC".to_owned(),
            value: payment.amount.as_u64().to_string(),
            unit: AmountUnit::Base,
        },
        payment_id: prepared.payment_id.clone(),
        intent_hash: IntentHashString("v1:sha256:placeholder".to_owned()),
        expires_at: ExpiresAt::BlockHeight(prepared.expiry_height),
    };
    authorization.intent_hash = IntentHashString(
        recompute_intent_hash(&authorization, &recipient_caip10, payment.amount.as_u64())
            .map_err(|err| HarnessError::Jwt(err.to_string()))?,
    );
    Ok(authorization)
}

struct SignPaymentCall<'a> {
    call_sign_url: &'a str,
    access_token: &'a str,
    dpop_proof: &'a str,
    prepared: &'a PreparedPayment,
    network_label: &'a str,
}

struct AgentSignedPczt {
    tx_id: String,
    pczt_base64: String,
}

async fn request_zspend_signature(
    call: SignPaymentCall<'_>,
) -> Result<AgentSignedPczt, HarnessError> {
    let sign_body = SignPaymentRequestBody {
        payment_request: WirePaymentRequestBody {
            scheme: "zip321".to_owned(),
            request_uri: call.prepared.payment_uri.clone(),
        },
        network: call.network_label.to_owned(),
        payment_id: call.prepared.payment_id.clone(),
        target_expiry_height: call.prepared.expiry_height,
    };
    let client = reqwest::Client::new();
    let response = client
        .post(call.call_sign_url)
        .header("authorization", format!("DPoP {}", call.access_token))
        .header("dpop", call.dpop_proof)
        .json(&sign_body)
        .send()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(HarnessError::SignFailed { status, body: text });
    }
    let signed: SignResponseBody = response
        .json()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    if signed.signed.format != "pczt-v2-extractable" {
        return Err(HarnessError::SignedBytes(format!(
            "expected pczt-v2-extractable, got {}",
            signed.signed.format
        )));
    }
    let pczt_bytes = URL_SAFE_NO_PAD
        .decode(signed.signed.bytes.as_bytes())
        .map_err(|err| HarnessError::SignedBytes(err.to_string()))?;
    info!(
        tx_id = %signed.signed.tx_id,
        pczt_bytes = pczt_bytes.len(),
        "zspend returned signed PCZT",
    );
    Ok(AgentSignedPczt {
        tx_id: signed.signed.tx_id,
        pczt_base64: signed.signed.bytes,
    })
}

fn build_x402_facilitator_request(
    network: Network,
    zpay_url: &str,
    prepared: &PreparedPayment,
    pczt_base64: &str,
) -> Result<serde_json::Value, HarnessError> {
    let parsed = PaymentRequest::from_uri(&prepared.payment_uri, network)?;
    let payment = parsed
        .payments()
        .first()
        .ok_or(HarnessError::PaymentMissing)?;
    let network_id = x402_network_id(network);
    let requirements = serde_json::json!({
        "scheme": "exact",
        "network": network_id,
        "amount": payment.amount.as_u64().to_string(),
        "asset": "ZEC",
        "payTo": payment.recipient.encoded(),
        "maxTimeoutSeconds": 120,
        "extra": {
            "binding": "x402-zcash-exact-v1",
            "amountUnit": "zat",
            "authorizationFormat": "pczt-v2-extractable",
            "zpayPaymentId": prepared.payment_id.as_str()
        }
    });
    let resource = serde_json::json!({
        "url": format!("{}/zpay-e2e/payments/{}", zpay_url.trim_end_matches('/'), prepared.payment_id),
        "description": "zpay e2e x402 payment",
        "mimeType": "application/json",
        "serviceName": "zpay-e2e",
        "tags": ["e2e", "zcash"],
    });
    Ok(serde_json::json!({
        "x402Version": 2,
        "paymentPayload": {
            "x402Version": 2,
            "resource": resource,
            "accepted": requirements,
            "payload": {
                "format": "pczt-v2-extractable",
                "pczt": pczt_base64,
            },
        },
        "paymentRequirements": requirements,
    }))
}

async fn verify_x402_payment(
    zpay_url: &str,
    facilitator_request: &serde_json::Value,
) -> Result<(), HarnessError> {
    let client = reqwest::Client::new();
    let verify_url = format!("{}/x402/v2/verify", zpay_url.trim_end_matches('/'));
    let response = client
        .post(&verify_url)
        .json(facilitator_request)
        .send()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(HarnessError::VerifyFailed { status, body: text });
    }
    let verify: X402VerifyResponseBody = response
        .json()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    if !verify.is_valid {
        return Err(HarnessError::VerifyInvalid {
            reason: verify
                .invalid_reason
                .unwrap_or_else(|| "unknown".to_owned()),
        });
    }
    info!("x402 verify accepted signed PCZT");
    Ok(())
}

async fn settle_x402_payment(
    zpay_url: &str,
    facilitator_request: &serde_json::Value,
) -> Result<String, HarnessError> {
    let client = reqwest::Client::new();
    let settle_url = format!("{}/x402/v2/settle", zpay_url.trim_end_matches('/'));
    let response = client
        .post(&settle_url)
        .json(facilitator_request)
        .send()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(HarnessError::SettleFailed { status, body: text });
    }
    let settle: X402SettleResponseBody = response
        .json()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    if !settle.success {
        return Err(HarnessError::SettleUnsuccessful {
            reason: settle.error_reason.unwrap_or_else(|| "unknown".to_owned()),
        });
    }
    let tx_id = settle.transaction.ok_or_else(|| {
        HarnessError::SignedBytes("x402 settle succeeded without transaction id".to_owned())
    })?;
    info!(
        tx_id = %tx_id,
        network = %settle.network,
        amount = %settle.amount,
        "x402 settle accepted signed PCZT",
    );
    Ok(tx_id)
}

fn x402_network_id(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "zcash:mainnet",
        Network::Regtest(_) => "zcash:regtest",
        Network::Testnet | _ => "zcash:testnet",
    }
}

async fn check_zexplorer(
    network: Network,
    zexplorer_tx_url: &str,
    tx_id: &str,
    poll_seconds: u64,
) -> Result<(), HarnessError> {
    if !matches!(network, Network::Testnet) {
        warn!("zexplorer check skipped outside testnet");
        return Ok(());
    }
    let url = format!("{}/{}", zexplorer_tx_url.trim_end_matches('/'), tx_id);
    let client = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + Duration::from_secs(poll_seconds);
    loop {
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|err| HarnessError::Http(err.to_string()))?;
        if response.status().is_success() {
            info!(url = %url, "zexplorer returned a successful response for tx");
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(HarnessError::ZexplorerFailed {
                url,
                status: response.status(),
            });
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

fn load_issuer_encoding_key(path: &std::path::Path) -> Result<EncodingKey, HarnessError> {
    let raw = std::fs::read(path).map_err(|source| HarnessError::KeyRead {
        path: path.to_path_buf(),
        source,
    })?;
    if raw.starts_with(b"-----BEGIN") {
        EncodingKey::from_ed_pem(&raw).map_err(|err| HarnessError::Jwt(err.to_string()))
    } else {
        Ok(EncodingKey::from_ed_der(&raw))
    }
}

struct DpopKey {
    encoding: EncodingKey,
    x: String,
    y: String,
    jkt: String,
}

impl DpopKey {
    fn generate() -> Result<Self, HarnessError> {
        let signing_key = SigningKey::random(&mut OsRng);
        let point = signing_key.verifying_key().to_encoded_point(false);
        let x = URL_SAFE_NO_PAD.encode(
            point
                .x()
                .ok_or_else(|| HarnessError::Jwt("P-256 point missing x coordinate".to_owned()))?,
        );
        let y = URL_SAFE_NO_PAD.encode(
            point
                .y()
                .ok_or_else(|| HarnessError::Jwt("P-256 point missing y coordinate".to_owned()))?,
        );
        let pem = signing_key
            .to_pkcs8_pem(LineEnding::LF)
            .map_err(|err| HarnessError::Jwt(err.to_string()))?
            .to_string();
        let encoding = EncodingKey::from_ec_pem(pem.as_bytes())
            .map_err(|err| HarnessError::Jwt(err.to_string()))?;
        let jkt = zspend_core::ec_jwk_thumbprint("P-256", "EC", &x, &y);
        Ok(Self {
            encoding,
            x,
            y,
            jkt,
        })
    }

    fn mint_proof(&self, method: &str, proof_url: &str) -> Result<String, HarnessError> {
        self.mint_proof_inner(method, proof_url, None)
    }

    fn mint_access_bound_proof(
        &self,
        access_token: &str,
        method: &str,
        proof_url: &str,
    ) -> Result<String, HarnessError> {
        let ath = URL_SAFE_NO_PAD.encode(Sha256::digest(access_token.as_bytes()));
        self.mint_proof_inner(method, proof_url, Some(ath))
    }

    fn mint_proof_inner(
        &self,
        method: &str,
        proof_url: &str,
        ath: Option<String>,
    ) -> Result<String, HarnessError> {
        let mut header = Header::new(Algorithm::ES256);
        header.typ = Some("dpop+jwt".to_owned());
        header.jwk = Some(
            serde_json::from_value(serde_json::json!({
                "kty": "EC",
                "crv": "P-256",
                "x": self.x,
                "y": self.y,
            }))
            .map_err(|err| HarnessError::Jwt(err.to_string()))?,
        );
        let mut claims = serde_json::json!({
            "htm": method,
            "htu": proof_url,
            "jti": format!("zpay-e2e-dpop-{}", unix_now_ms()),
            "iat": unix_now_seconds(),
        });
        if let Some(ath) = ath {
            claims["ath"] = serde_json::Value::String(ath);
        }
        encode(&header, &claims, &self.encoding).map_err(|err| HarnessError::Jwt(err.to_string()))
    }
}

struct AccessTokenGrant<'a> {
    issuer_key: &'a EncodingKey,
    issuer_kid: &'a str,
    audience: &'a str,
    dpop_jkt: &'a str,
    authorization: &'a PaymentAuthorization,
    token_ttl_seconds: u64,
}

fn mint_access_token(grant: &AccessTokenGrant<'_>) -> Result<String, HarnessError> {
    let mut header = Header::new(Algorithm::EdDSA);
    header.kid = Some(grant.issuer_kid.to_owned());
    let claims = serde_json::json!({
        "aud": grant.audience,
        "jti": format!("zpay-e2e-at-{}", unix_now_ms()),
        "exp": unix_now_seconds().saturating_add(grant.token_ttl_seconds),
        "cnf": { "jkt": grant.dpop_jkt },
        "authorization_details": [grant.authorization],
    });
    encode(&header, &claims, grant.issuer_key).map_err(|err| HarnessError::Jwt(err.to_string()))
}

fn unix_now_seconds() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs())
}

fn unix_now_ms() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis())
}

fn chain_reference(network: Network) -> &'static str {
    match network {
        Network::Mainnet => "main",
        Network::Regtest(_) => "regtest",
        Network::Testnet | _ => "test",
    }
}

async fn wait_for_settlement_completion(
    zpay_url: &str,
    payment_id: &str,
    settlement_completion: SettlementCompletion,
    poll_seconds: u64,
) -> Result<(), HarnessError> {
    let deadline = std::time::Instant::now() + Duration::from_secs(poll_seconds);
    let client = reqwest::Client::new();
    let url = format!(
        "{}/zpay/v1/payments/{}",
        zpay_url.trim_end_matches('/'),
        payment_id
    );
    let mut last_status = String::new();
    loop {
        let response = client
            .get(&url)
            .send()
            .await
            .map_err(|err| HarnessError::Http(err.to_string()))?;
        let body: PaymentStatusData = response
            .json()
            .await
            .map_err(|err| HarnessError::Http(err.to_string()))?;
        let summary = format!(
            "status={} confirmation_count={:?} mined_height={:?} settled={}",
            body.status, body.confirmation_count, body.mined_block_height, body.settled,
        );
        if summary != last_status {
            info!(?summary, "payment status");
            last_status = summary;
        }
        if settlement_completion.is_observed(&body) {
            info!(
                settlement_completion = settlement_completion.label(),
                "requested lifecycle completion observed"
            );
            return Ok(());
        }
        if std::time::Instant::now() >= deadline {
            return Err(HarnessError::SettlementCompletionTimedOut {
                settlement_completion: settlement_completion.label(),
            });
        }
        tokio::time::sleep(Duration::from_secs(15)).await;
    }
}

async fn confirm_x402_lifecycle_record(
    zpay_url: &str,
    payment_id: &str,
    settled_tx_id: &str,
) -> Result<(), HarnessError> {
    let client = reqwest::Client::new();
    let url = format!(
        "{}/zpay/v1/payments/{}",
        zpay_url.trim_end_matches('/'),
        payment_id
    );
    let response = client
        .get(&url)
        .send()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    let body: PaymentStatusData = response
        .json()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    let recorded_tx_id = body
        .broadcast_outcome
        .and_then(|outcome| outcome.transaction_id)
        .unwrap_or_default();
    if recorded_tx_id != settled_tx_id {
        return Err(HarnessError::LifecycleStatus(format!(
            "expected zpay status for {payment_id} to record txid {settled_tx_id}, got status={} txid={recorded_tx_id}",
            body.status
        )));
    }
    info!(
        payment_id,
        status = %body.status,
        tx_id = %recorded_tx_id,
        "zpay lifecycle status recorded x402 settlement",
    );
    Ok(())
}

async fn call_prepare(
    zpay_url: &str,
    payee_id: &str,
    network_label: &str,
    dpop_key: &DpopKey,
) -> Result<PreparedPayment, HarnessError> {
    let idempotency_key = format!(
        "zpay-e2e-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_millis())
    );
    let body = PrepareRequestBody {
        payee_id: payee_id.to_owned(),
        network: network_label.to_owned(),
        scheme: "zcash".to_owned(),
        resource_uri: format!("zpay-e2e/items/{payee_id}"),
        nonce: idempotency_key.clone(),
        evidence_pack_hash: None,
        idempotency_key: Some(idempotency_key),
    };
    let client = reqwest::Client::new();
    let prepare_url = format!("{}/zpay/v1/prepare", zpay_url.trim_end_matches('/'));
    let dpop_proof = dpop_key.mint_proof("POST", &prepare_url)?;
    let response = client
        .post(&prepare_url)
        .header("dpop", dpop_proof)
        .json(&body)
        .send()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    if !response.status().is_success() {
        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        return Err(HarnessError::PrepareFailed { status, body: text });
    }
    let prepared: PreparedPayment = response
        .json()
        .await
        .map_err(|err| HarnessError::Http(err.to_string()))?;
    Ok(prepared)
}

fn memo_from_protocol_prefix(memo_bytes: &[u8]) -> Result<Memo, HarnessError> {
    let len = memo_bytes.len();
    if len != PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE && len != PROTOCOL_MEMO_BYTE_COUNT {
        return Err(HarnessError::MemoLength { len });
    }
    // ZIP-302: 0xFF declares an Arbitrary memo whose remaining 511
    // bytes carry application-defined data. The prepared protocol memo
    // is either 66 bytes (no evidence pack) or 98 bytes (with one);
    // anything past the prefix is zero padding.
    let mut buf = [0u8; 512];
    buf[..len].copy_from_slice(memo_bytes);
    let memo_bytes = MemoBytes::from_bytes(&buf).map_err(|err| HarnessError::MemoCompose {
        reason: err.to_string(),
    })?;
    Memo::try_from(&memo_bytes).map_err(|err| HarnessError::MemoCompose {
        reason: err.to_string(),
    })
}

/// Custom `Submitter` that hands the signed transaction bytes to zpay's
/// `/zpay/v1/settle` endpoint instead of broadcasting directly. The
/// payment id was issued by the same zpay-runtime at prepare time.
struct ZpaySettleSubmitter {
    zpay_url: String,
    payment_id: String,
    network: Network,
    dpop_key: Arc<DpopKey>,
    client: reqwest::Client,
}

impl ZpaySettleSubmitter {
    fn new(zpay_url: String, payment_id: String, network: Network, dpop_key: Arc<DpopKey>) -> Self {
        Self {
            zpay_url,
            payment_id,
            network,
            dpop_key,
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
        let settle_url = format!("{}/zpay/v1/settle", self.zpay_url.trim_end_matches('/'));
        let dpop_proof = self
            .dpop_key
            .mint_proof("POST", &settle_url)
            .map_err(|err| SubmitterError::Unavailable {
                reason: err.to_string(),
            })?;
        let response = self
            .client
            .post(&settle_url)
            .header("dpop", dpop_proof)
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
        let outcome: SettlementResponseData =
            response
                .json()
                .await
                .map_err(|err| SubmitterError::Unavailable {
                    reason: err.to_string(),
                })?;
        zpay_outcome_to_submit_outcome(&outcome)
    }
}

fn zpay_outcome_to_submit_outcome(
    outcome: &SettlementResponseData,
) -> Result<SubmitOutcome, SubmitterError> {
    let tx_id_hex = outcome
        .broadcast_outcome
        .transaction_id
        .clone()
        .unwrap_or_default();
    let tx_id_bytes = decode_txid(&tx_id_hex)?;
    let tx_id = TxId::from_bytes(tx_id_bytes);
    // Commit A renamed the broadcast outcome's serde tag to `kind`.
    match outcome.broadcast_outcome.kind.as_str() {
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

/// Wire types for `POST /v1/payments/sign`.

#[derive(Debug, Serialize)]
struct SignPaymentRequestBody {
    payment_request: WirePaymentRequestBody,
    network: String,
    payment_id: String,
    target_expiry_height: u32,
}

#[derive(Debug, Serialize)]
struct WirePaymentRequestBody {
    scheme: String,
    #[serde(rename = "value")]
    request_uri: String,
}

#[derive(Debug, Deserialize)]
struct SignResponseBody {
    #[serde(rename = "signed_payload")]
    signed: SignedSpendWire,
}

#[derive(Debug, Deserialize)]
struct SignedSpendWire {
    format: String,
    bytes: String,
    tx_id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct X402VerifyResponseBody {
    is_valid: bool,
    invalid_reason: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct X402SettleResponseBody {
    success: bool,
    error_reason: Option<String>,
    transaction: Option<String>,
    network: String,
    amount: String,
}

/// Wire types for `/zpay/v1/prepare`.

#[derive(Debug, Serialize)]
struct PrepareRequestBody {
    payee_id: String,
    network: String,
    scheme: String,
    resource_uri: String,
    nonce: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    evidence_pack_hash: Option<Vec<u8>>,
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize)]
struct PreparedPayment {
    payment_id: String,
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "wire-shape mirror of zpay response; not all fields are read by the harness"
    )]
    payment_uri: String,
    memo_bytes: Vec<u8>,
    expiry_height: u32,
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "wire-shape mirror of zpay response; not all fields are read by the harness"
    )]
    amount_zat: u64,
}

/// Wire types for `/zpay/v1/settle`.

#[derive(Debug, Serialize)]
struct SettleRequestBody {
    payment_id: String,
    raw_tx_hex: String,
}

#[derive(Debug, Deserialize)]
struct SettlementResponseData {
    #[allow(
        dead_code,
        reason = "wire-shape mirror of zpay response; not all fields are read by the harness"
    )]
    payment_id: String,
    broadcast_outcome: BroadcastOutcomeBody,
    #[allow(
        dead_code,
        reason = "wire-shape mirror of zpay response; not all fields are read by the harness"
    )]
    watch_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BroadcastOutcomeBody {
    /// Discriminator name: matches Commit A's `kind` serde tag.
    kind: String,
    transaction_id: Option<String>,
    #[serde(default)]
    #[allow(
        dead_code,
        reason = "wire-shape mirror of zpay response; not all fields are read by the harness"
    )]
    upstream_message: Option<String>,
}

/// Wire types for `/zpay/v1/payments/{payment_id}`.

#[derive(Debug, Deserialize)]
struct PaymentStatusData {
    #[allow(
        dead_code,
        reason = "wire-shape mirror of zpay response; not all fields are read by the harness"
    )]
    payment_id: String,
    status: String,
    #[serde(default)]
    confirmation_count: Option<u32>,
    #[serde(default)]
    mined_block_height: Option<u64>,
    #[serde(default)]
    settled: bool,
    #[serde(default)]
    broadcast_outcome: Option<BroadcastOutcomeBody>,
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
    #[error("zspend /v1/payments/sign returned {status}: {body}")]
    SignFailed {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("zpay /x402/v2/verify returned {status}: {body}")]
    VerifyFailed {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("zpay /x402/v2/verify rejected payment: {reason}")]
    VerifyInvalid { reason: String },
    #[error("zpay /settle returned {status}: {body}")]
    SettleFailed {
        status: reqwest::StatusCode,
        body: String,
    },
    #[error("zpay /x402/v2/settle did not settle payment: {reason}")]
    SettleUnsuccessful { reason: String },
    #[error("http error: {0}")]
    Http(String),
    #[error("issuer key read failed at {path:?}: {source}")]
    KeyRead {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("jwt error: {0}")]
    Jwt(String),
    #[error("signed bytes invalid: {0}")]
    SignedBytes(String),
    #[error("x402 lifecycle status mismatch: {0}")]
    LifecycleStatus(String),
    #[error("prepared payment URI carried no payment")]
    PaymentMissing,
    #[error("zexplorer check failed for {url}: {status}")]
    ZexplorerFailed {
        url: String,
        status: reqwest::StatusCode,
    },
    #[error(
        "insufficient funds: spendable={spendable_zat}, requested={requested_zat} (+ ~5000 zat fee)"
    )]
    InsufficientFunds {
        spendable_zat: u64,
        requested_zat: u64,
    },
    #[error(
        "prepared memo_bytes length expected {PROTOCOL_MEMO_BYTE_COUNT_NO_EVIDENCE} or {PROTOCOL_MEMO_BYTE_COUNT}, got {len}"
    )]
    MemoLength { len: usize },
    #[error("memo compose failed: {reason}")]
    MemoCompose { reason: String },
    #[error("idempotency key invalid: {0}")]
    Idempotency(String),
    #[error("zatoshi amount invalid: {0}")]
    Zat(String),
    #[error("polling /payments/{{payment_id}} timed out before {settlement_completion} completion")]
    SettlementCompletionTimedOut { settlement_completion: &'static str },
    #[error("unsupported --network value: {0} (expected 'testnet' or 'regtest')")]
    NetworkInvalid(String),
}

#[cfg(test)]
mod tests {
    use super::{PaymentStatusData, SettlementCompletion};

    fn payment_status(
        status: &str,
        confirmation_count: Option<u32>,
        settled: bool,
    ) -> PaymentStatusData {
        PaymentStatusData {
            payment_id: "payment-id".to_owned(),
            status: status.to_owned(),
            confirmation_count,
            mined_block_height: Some(4_156_026),
            settled,
            broadcast_outcome: None,
        }
    }

    #[test]
    fn lifecycle_completion_distinguishes_mined_final_and_settled() {
        let mined = payment_status("mined", Some(1), false);
        let final_payment = payment_status("final", Some(3), false);
        let settled_payment = payment_status("final", Some(100), true);

        assert!(SettlementCompletion::Mined.is_observed(&mined));
        assert!(!SettlementCompletion::Final.is_observed(&mined));
        assert!(!SettlementCompletion::Settled.is_observed(&mined));

        assert!(SettlementCompletion::Mined.is_observed(&final_payment));
        assert!(SettlementCompletion::Final.is_observed(&final_payment));
        assert!(!SettlementCompletion::Settled.is_observed(&final_payment));

        assert!(SettlementCompletion::Mined.is_observed(&settled_payment));
        assert!(SettlementCompletion::Final.is_observed(&settled_payment));
        assert!(SettlementCompletion::Settled.is_observed(&settled_payment));
    }
}
