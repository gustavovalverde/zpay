import { CircleAlert, RotateCcw } from "lucide-react";
import { useState } from "react";
import {
  type AmountReconciliation,
  type ChainPresence,
  type CryptographicVerdict,
  type MessageReconciliation,
  type RecipientReconciliation,
  type VerifyResponseBody,
  verifyPaymentReceipt
} from "../demo-client";
import { PaymentFactList } from "../components/PaymentFactList";
import { formatZec, integerFormatter, truncateMiddle } from "../format";
import { friendlyProblem } from "../friendly-problem";
import { usePaymentHistory } from "../hooks/usePaymentHistory";
import { stageLabelFor } from "../stage";

type VerdictTone = "positive" | "negative" | "neutral";

export function ReceiptHistoryView() {
  const { payments, isLoading, error, refresh } = usePaymentHistory();
  const [selectedPaymentId, setSelectedPaymentId] = useState<string | null>(null);
  const [verifyResult, setVerifyResult] = useState<VerifyResponseBody | null>(null);
  const [isVerifying, setIsVerifying] = useState(false);
  const [verifyError, setVerifyError] = useState<string | null>(null);

  const selected = payments.find((payment) => payment.payment_id === selectedPaymentId) ?? payments[0] ?? null;

  function onSelectPayment(paymentId: string) {
    setSelectedPaymentId(paymentId);
    setVerifyResult(null);
    setVerifyError(null);
  }

  async function onVerifyClick() {
    if (!selected?.transaction_id) {
      return;
    }
    setIsVerifying(true);
    setVerifyError(null);
    setVerifyResult(null);
    try {
      const result = await verifyPaymentReceipt(selected.payment_id);
      setVerifyResult(result);
    } catch (err) {
      setVerifyError(friendlyProblem(err));
    } finally {
      setIsVerifying(false);
    }
  }

  return (
    <section className="receipt-layout" aria-label="Receipts and verification">
      <div className="payment-history-list">
        <div className="payment-history-heading">
          <p className="eyebrow">History</p>
          <button type="button" className="icon-button" aria-label="Refresh history" title="Refresh" onClick={refresh}>
            <RotateCcw aria-hidden="true" size={16} />
          </button>
        </div>

        {isLoading && payments.length === 0 && <p className="payment-history-empty">Loading…</p>}
        {error && (
          <div className="notice" role="alert">
            <CircleAlert aria-hidden="true" size={18} />
            <span>{error}</span>
          </div>
        )}
        {!isLoading && !error && payments.length === 0 && (
          <p className="payment-history-empty">No payments made this session yet.</p>
        )}

        <ul>
          {payments.map((payment) => (
            <li key={payment.payment_id}>
              <button
                type="button"
                className={payment.payment_id === selected?.payment_id ? "is-selected" : ""}
                onClick={() => onSelectPayment(payment.payment_id)}
              >
                <span className="payment-history-amount">{formatZec(payment.amount_zat)} ZEC</span>
                <span className={`stage-chip stage-${payment.stage}`}>{stageLabelFor(payment.stage)}</span>
              </button>
            </li>
          ))}
        </ul>
      </div>

      <div className="receipt-detail-panel">
        {!selected && <p>Select a payment to see its receipt.</p>}
        {selected && (
          <>
            <div className="receipt-detail-heading">
              <div>
                <p className="eyebrow">Receipt</p>
                <p className="receipt-detail-amount">{formatZec(selected.amount_zat)} ZEC</p>
              </div>
              <span className={`stage-chip stage-${selected.stage}`}>{stageLabelFor(selected.stage)}</span>
            </div>

            <PaymentFactList
              facts={[
                { label: "payment_id", value: truncateMiddle(selected.payment_id, 12, 10) },
                {
                  label: "transaction_id",
                  value: selected.transaction_id ? truncateMiddle(selected.transaction_id, 12, 12) : "Pending"
                },
                { label: "confirmations", value: integerFormatter.format(selected.confirmation_count ?? 0) },
                { label: "settled", value: selected.settled ? "yes" : "no" }
              ]}
            />

            <div className="action-row">
              <button
                type="button"
                className="secondary-button"
                onClick={onVerifyClick}
                disabled={!selected.transaction_id || isVerifying}
              >
                {isVerifying ? "Verifying…" : "Verify payment disclosure"}
              </button>
            </div>

            {verifyError && (
              <div className="notice" role="alert">
                <CircleAlert aria-hidden="true" size={18} />
                <span>{verifyError}</span>
              </div>
            )}

            {verifyResult && (
              <div className="verdict-panel">
                <div className="verdict-chip-row">
                  <VerdictChip
                    label="cryptographic_verdict"
                    value={verifyResult.cryptographic_verdict}
                    tone={cryptographicTone(verifyResult.cryptographic_verdict)}
                  />
                  <VerdictChip
                    label="chain_presence"
                    value={verifyResult.chain_presence}
                    tone={chainPresenceTone(verifyResult.chain_presence)}
                  />
                  <VerdictChip
                    label="amount_reconciliation"
                    value={verifyResult.amount_reconciliation}
                    tone={amountReconciliationTone(verifyResult.amount_reconciliation)}
                  />
                  <VerdictChip
                    label="recipient_reconciliation"
                    value={verifyResult.recipient_reconciliation}
                    tone={recipientReconciliationTone(verifyResult.recipient_reconciliation)}
                  />
                  <VerdictChip
                    label="message_reconciliation"
                    value={verifyResult.message_reconciliation}
                    tone={messageReconciliationTone(verifyResult.message_reconciliation)}
                  />
                </div>
                <p className="verdict-note">
                  ZIP-311 Draft1 and the Zally Ironwood extension check spend authority, recipient, amount, and
                  challenge independently against the mined transaction
                </p>
              </div>
            )}
          </>
        )}
      </div>
    </section>
  );
}

function VerdictChip({ label, value, tone }: { label: string; value: string; tone: VerdictTone }) {
  return (
    <div className={`verdict-chip verdict-chip-${tone}`}>
      <p className="verdict-chip-label">{label}</p>
      <p className="verdict-chip-value">{value}</p>
    </div>
  );
}

function cryptographicTone(value: CryptographicVerdict): VerdictTone {
  if (value === "valid") {
    return "positive";
  }
  if (value === "inconclusive") {
    return "neutral";
  }
  return "negative";
}

function chainPresenceTone(value: ChainPresence): VerdictTone {
  if (value === "mined") {
    return "positive";
  }
  if (value === "oracle_unavailable") {
    return "neutral";
  }
  return "negative";
}

function amountReconciliationTone(value: AmountReconciliation): VerdictTone {
  if (value === "match") {
    return "positive";
  }
  if (value === "not_checked") {
    return "neutral";
  }
  return "negative";
}

function recipientReconciliationTone(value: RecipientReconciliation): VerdictTone {
  return reconciliationTone(value);
}

function messageReconciliationTone(value: MessageReconciliation): VerdictTone {
  return reconciliationTone(value);
}

function reconciliationTone(value: "match" | "mismatch" | "not_checked"): VerdictTone {
  if (value === "match") {
    return "positive";
  }
  if (value === "not_checked") {
    return "neutral";
  }
  return "negative";
}
