import type { PaymentBody } from "../demo-client";
import { integerFormatter, truncateMiddle } from "../format";
import { PaymentFactList } from "./PaymentFactList";

interface WireTracePanelProps {
  payment: PaymentBody | null;
  readinessRows: { name: string; status: string }[];
}

export function WireTracePanel({ payment, readinessRows }: WireTracePanelProps) {
  return (
    <details className="wire-trace-panel">
      <summary>What's happening on the wire</summary>
      <PaymentFactList
        facts={[
          { label: "payment_id", value: payment ? truncateMiddle(payment.payment_id, 12, 10) : "Not prepared" },
          {
            label: "expiry_height",
            value: payment ? integerFormatter.format(payment.expiry_height) : "Not prepared"
          },
          { label: "status", value: payment?.status ?? "Not prepared" },
          { label: "confirmations", value: integerFormatter.format(payment?.confirmation_count ?? 0) },
          {
            label: "transaction_id",
            value: payment?.transaction_id ? truncateMiddle(payment.transaction_id, 12, 12) : "Pending"
          }
        ]}
      />
      {readinessRows.length > 0 && (
        <ul className="readiness-list" aria-label="Readiness">
          {readinessRows.map((row) => (
            <li key={row.name}>
              <span>{row.name}</span>
              <strong>{row.status}</strong>
            </li>
          ))}
        </ul>
      )}
    </details>
  );
}
