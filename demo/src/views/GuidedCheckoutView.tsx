import { Bot, Check, CircleAlert, Copy, CreditCard, Droplet, ExternalLink, FileText, RotateCcw, WalletCards } from "lucide-react";
import { useMemo, useState } from "react";
import { PaymentStepper } from "../components/PaymentStepper";
import { WireTracePanel } from "../components/WireTracePanel";
import type { DemoStage, PaymentMode } from "../demo-client";
import { formatZec, truncateMiddle } from "../format";
import type { PaymentSession } from "../hooks/usePaymentSession";
import type { WalletReadiness } from "../hooks/useWalletReadiness";
import { accessGranted, stageLabelFor } from "../stage";

const readinessKeys = ["zpay", "zspend", "zinder", "wallet", "faucet"] as const;

interface GuidedCheckoutViewProps {
  mode: PaymentMode;
  onModeChange: (mode: PaymentMode) => void;
  notice: string | null;
  walletReadiness: WalletReadiness;
  paymentSession: PaymentSession;
}

export function GuidedCheckoutView({ mode, onModeChange, notice, walletReadiness, paymentSession }: GuidedCheckoutViewProps) {
  const { readiness, wallet, faucetClaim, isClaiming, isFaucetOpen, onFaucetClick } = walletReadiness;
  const { payment, isPreparing, isSettling, prepareCheckout, settleCheckout, resetPayment } = paymentSession;
  const [copiedLabel, setCopiedLabel] = useState<string | null>(null);

  const currentStage: DemoStage = payment?.stage ?? (wallet?.is_funded ? "ready" : "needs_funds");
  const canPrepare = Boolean(wallet?.is_funded) && !isPreparing && !isSettling;
  const canSettle = Boolean(payment?.can_settle) && !isSettling;
  const hasAccess = accessGranted(currentStage);
  const stageLabel = stageLabelFor(currentStage);
  const amountZec = payment ? formatZec(payment.amount_zat) : "0.0005";
  const primaryButtonLabel = payment ? settleLabel(mode) : "Pay with ZEC";
  const primaryButtonIcon = payment ? modeIcon(mode) : <CreditCard aria-hidden="true" size={18} />;
  const primaryButtonBusyLabel = payment ? busySettleLabel(currentStage) : "Preparing…";

  const readinessRows = useMemo(() => {
    if (!readiness) {
      return [];
    }
    return readinessKeys.map((key) => ({ name: key, status: readiness[key].status }));
  }, [readiness]);

  async function onCopyClick(text: string, label: string) {
    await navigator.clipboard.writeText(text);
    setCopiedLabel(label);
    window.setTimeout(() => setCopiedLabel(null), 1800);
  }

  return (
    <>
      <div className="checkout-primary">
        <section className="order-summary-card" aria-labelledby="order-summary-title">
          <div className="order-summary-heading">
            <div>
              <p className="eyebrow">Aether research</p>
              <h2 id="order-summary-title">Q3 private market signal</h2>
            </div>
            <span className={`access-chip ${hasAccess ? "paid" : "locked"}`}>{hasAccess ? "paid" : "locked"}</span>
          </div>

          <div className="order-line-item">
            <span>Report access</span>
            <strong>{amountZec} ZEC</strong>
          </div>

          <article className="order-summary-content" aria-live="polite">
            {hasAccess ? (
              <>
                <h3>Report unlocked</h3>
                <p>
                  Renewable demand rose across synthetic fuels, private credit spreads tightened, and
                  settlement activity clustered around two late-cycle liquidity windows.
                </p>
                <a className="secondary-button" href="#order-summary-title">
                  <FileText aria-hidden="true" size={18} />
                  Open report
                </a>
              </>
            ) : (
              <>
                <h3>Locked preview</h3>
                <p>Pay {amountZec} ZEC on testnet to reveal the report and explorer link.</p>
              </>
            )}
          </article>
        </section>

        <section className="trust-card" aria-label="Why this is different">
          <p className="eyebrow">Why this is different</p>
          <ul>
            <li>
              <strong>The facilitator never holds funds.</strong> zpay brokers the handoff; your wallet signs,
              zinder broadcasts.
            </li>
            <li>
              <strong>Terms are server-composed.</strong> Amount, recipient, and expiry come from the payee's
              registered offer.
            </li>
            <li>
              <strong>Finality is explicit.</strong> A payment settles only once its block sits at or below the
              settled tip.
            </li>
          </ul>
        </section>
      </div>

      <aside className="payment-card" id="checkout-panel" aria-labelledby="payment-title">
        <div className="panel-heading">
          <div>
            <p className="eyebrow">Checkout</p>
            <h2 id="payment-title">ZEC payment</h2>
          </div>
          <span className={`stage-chip stage-${currentStage}`}>{stageLabel}</span>
        </div>

        <div className="mode-switch" role="radiogroup" aria-label="Payment mode">
          <button
            type="button"
            role="radio"
            aria-checked={mode === "checkout"}
            className={mode === "checkout" ? "is-selected" : ""}
            onClick={() => onModeChange("checkout")}
          >
            <WalletCards aria-hidden="true" size={18} />
            Checkout
          </button>
          <button
            type="button"
            role="radio"
            aria-checked={mode === "autopay"}
            className={mode === "autopay" ? "is-selected" : ""}
            onClick={() => onModeChange("autopay")}
          >
            <Bot aria-hidden="true" size={18} />
            Autopay
          </button>
        </div>

        <div className="amount-row">
          <span>Amount</span>
          <strong>{amountZec} ZEC</strong>
        </div>

        {notice && (
          <div className="notice" role="alert">
            <CircleAlert aria-hidden="true" size={18} />
            <span>{notice}</span>
          </div>
        )}

        <div className="wallet-sheet" aria-labelledby="wallet-title">
          <div>
            <h3 id="wallet-title">Demo wallet</h3>
            <p>{wallet ? truncateMiddle(wallet.address, 20, 16) : "Loading wallet"}</p>
          </div>
          <div className="wallet-actions">
            {wallet && (
              <button
                type="button"
                className="icon-button"
                aria-label="Copy address"
                title="Copy address"
                onClick={() => onCopyClick(wallet.address, "address")}
              >
                {copiedLabel === "address" ? <Check aria-hidden="true" size={18} /> : <Copy aria-hidden="true" size={18} />}
              </button>
            )}
            <button type="button" className="secondary-button compact" onClick={onFaucetClick} disabled={isClaiming}>
              <Droplet aria-hidden="true" size={18} />
              {isClaiming ? "Preparing…" : "Use faucet"}
            </button>
          </div>
        </div>

        {isFaucetOpen && (
          <section className="faucet-drawer" aria-labelledby="faucet-title">
            <div>
              <h3 id="faucet-title">Faucet claim</h3>
              <p>{faucetClaimText(faucetClaim, isClaiming)}</p>
            </div>
            {faucetClaim?.txid && (
              <button
                type="button"
                className="icon-button"
                aria-label="Copy faucet transaction"
                title="Copy faucet transaction"
                onClick={() => onCopyClick(faucetClaim.txid!, "faucet transaction")}
              >
                {copiedLabel === "faucet transaction" ? (
                  <Check aria-hidden="true" size={18} />
                ) : (
                  <Copy aria-hidden="true" size={18} />
                )}
              </button>
            )}
          </section>
        )}

        <PaymentStepper stage={currentStage} />

        <p className="status-line" aria-live="polite">
          {isPreparing
            ? "Preparing…"
            : isSettling
              ? primaryButtonBusyLabel
              : payment?.message ?? readinessMessage(wallet, readiness)}
        </p>

        <div className="action-row">
          {!payment && (
            <button type="button" className="primary-button" onClick={prepareCheckout} disabled={!canPrepare}>
              {isPreparing ? "Preparing…" : primaryButtonLabel}
              {primaryButtonIcon}
            </button>
          )}
          {payment && !accessGranted(payment.stage) && (
            <button type="button" className="primary-button" onClick={settleCheckout} disabled={!canSettle}>
              {isSettling ? primaryButtonBusyLabel : primaryButtonLabel}
              {primaryButtonIcon}
            </button>
          )}
          {(payment?.stage === "failed" || payment?.stage === "expired") && (
            <button type="button" className="secondary-button" onClick={resetPayment}>
              <RotateCcw aria-hidden="true" size={18} />
              Try again
            </button>
          )}
          {payment?.zexplorer_url && (
            <a className="secondary-button" href={payment.zexplorer_url} target="_blank" rel="noreferrer">
              <ExternalLink aria-hidden="true" size={18} />
              View transaction
            </a>
          )}
        </div>

        <WireTracePanel payment={payment} readinessRows={readinessRows} />
      </aside>
    </>
  );
}

function modeIcon(mode: PaymentMode) {
  return mode === "checkout" ? <Check aria-hidden="true" size={18} /> : <Bot aria-hidden="true" size={18} />;
}

function settleLabel(mode: PaymentMode): string {
  return mode === "checkout" ? "Approve payment" : "Start autopay";
}

function busySettleLabel(stage: DemoStage): string {
  if (stage === "settling") {
    return "Settling…";
  }
  if (stage === "confirming" || stage === "mined" || stage === "final") {
    return "Confirming…";
  }
  return "Signing…";
}

function readinessMessage(
  wallet: WalletReadiness["wallet"],
  readiness: WalletReadiness["readiness"]
): string {
  if (readiness?.wallet.status === "needs_funds") {
    return "The demo wallet needs testnet funds";
  }
  if (!wallet) {
    return "Preparing…";
  }
  if (!wallet.is_funded) {
    return "The demo wallet needs testnet funds";
  }
  return "Ready to start checkout";
}

function faucetClaimText(claim: WalletReadiness["faucetClaim"], isClaiming: boolean): string {
  if (isClaiming) {
    return "Preparing…";
  }
  if (!claim) {
    return "The demo wallet needs testnet funds";
  }
  if (claim.error_code) {
    return "Try again";
  }
  if (claim.txid) {
    return `Claim submitted: ${truncateMiddle(claim.txid, 12, 12)}`;
  }
  return claim.state ?? claim.outcome ?? "Confirming…";
}
