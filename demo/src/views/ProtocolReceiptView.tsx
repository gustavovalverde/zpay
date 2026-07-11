import { CircleAlert } from "lucide-react";
import type { PaymentMode } from "../demo-client";
import { formatZec, integerFormatter, truncateMiddle } from "../format";
import type { PaymentSession } from "../hooks/usePaymentSession";
import { isReachedStep, observedAtForStep, paymentStepFor, type PaymentStep } from "../payment-step";
import { PaymentFactList } from "../components/PaymentFactList";

interface ProtocolReceiptViewProps {
  mode: PaymentMode;
  paymentSession: PaymentSession;
}

interface TimelineNode {
  step: PaymentStep;
  title: string;
  detail: string;
  facts: { label: string; value: string }[];
}

export function ProtocolReceiptView({ mode, paymentSession }: ProtocolReceiptViewProps) {
  const { payment, stageObservedAtMs } = paymentSession;

  if (!payment) {
    return (
      <section className="protocol-receipt-empty">
        <p>Start a checkout to see its protocol receipt here.</p>
      </section>
    );
  }

  const currentStep = paymentStepFor(payment.stage);
  const isTerminalError = payment.stage === "failed" || payment.stage === "expired";
  const authorizeReached = isReachedStep("authorize", currentStep);
  const broadcastReached = isReachedStep("broadcast", currentStep);
  const settledReached = isReachedStep("settled", currentStep);

  const nodes: TimelineNode[] = [
    {
      step: "quote",
      title: "Payment prepared",
      detail: "zpay composed the protocol memo and issued a payment ID. Nothing was signed yet.",
      facts: [
        { label: "payment_id", value: truncateMiddle(payment.payment_id, 12, 10) },
        { label: "expiry_height", value: integerFormatter.format(payment.expiry_height) }
      ]
    },
    {
      step: "authorize",
      title: "Signed by your wallet",
      detail: authorizeReached
        ? mode === "autopay"
          ? "zspend signed under your bounded authorization grant. zpay never saw your keys."
          : "You approved the payment and the demo wallet signed it. zpay never saw your keys."
        : "Awaiting your approval to sign.",
      facts: authorizeReached ? [{ label: "mode", value: mode }] : []
    },
    {
      step: "broadcast",
      title: "Broadcast, watching for confirmations",
      detail: broadcastReached
        ? "zinder accepted the broadcast. The demo gateway watches the chain over SSE."
        : "Awaiting broadcast once the signed transaction is submitted.",
      facts: broadcastReached
        ? [
            {
              label: "transaction_id",
              value: payment.transaction_id ? truncateMiddle(payment.transaction_id, 12, 12) : "Pending"
            },
            { label: "confirmations", value: integerFormatter.format(payment.confirmation_count ?? 0) }
          ]
        : []
    },
    {
      step: "settled",
      title: "Settled",
      detail: settledReached
        ? payment.settled
          ? "Final and at or below the settled tip. A reorg can no longer move this payment."
          : "Final, but not yet settled: a reorg could still return this payment to broadcast."
        : "Awaiting enough confirmations to reach finality.",
      facts: settledReached ? [{ label: "settled", value: payment.settled ? "yes" : "no" }] : []
    }
  ];

  return (
    <section className="protocol-receipt" aria-label="Protocol receipt">
      <div className="protocol-receipt-heading">
        <p className="eyebrow">Paying Aether Research</p>
        <p className="protocol-receipt-amount">{formatZec(payment.amount_zat)} ZEC</p>
      </div>

      {isTerminalError ? (
        <p className="payment-stepper-error" role="status">
          <CircleAlert aria-hidden="true" size={16} />
          {payment.stage === "failed" ? "Payment failed" : "Payment expired"}
        </p>
      ) : (
        <ol className="protocol-timeline">
          {nodes.map((node) => {
            const reached = isReachedStep(node.step, currentStep);
            const observedAtMs = observedAtForStep(node.step, stageObservedAtMs);
            return (
              <li key={node.step} className={reached ? "is-reached" : ""}>
                <span className="timeline-node-dot" aria-hidden="true" />
                <div>
                  <div className="timeline-node-heading">
                    <p className="timeline-node-title">{node.title}</p>
                    {observedAtMs !== null && (
                      <span className="timeline-node-observed-at">observed {formatObservedTime(observedAtMs)}</span>
                    )}
                  </div>
                  <p className="timeline-node-detail">{node.detail}</p>
                  {reached && node.facts.length > 0 && <PaymentFactList facts={node.facts} />}
                </div>
              </li>
            );
          })}
        </ol>
      )}
    </section>
  );
}

function formatObservedTime(ms: number): string {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit", second: "2-digit" });
}
