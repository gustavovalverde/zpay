import { CircleAlert, RotateCcw } from "lucide-react";
import type { ConsolePaymentRow } from "../demo-client";
import { formatZec, integerFormatter, truncateMiddle } from "../format";
import { useConsolePayments } from "../hooks/useConsolePayments";

export function FacilitatorConsoleView() {
  const { data, isLoading, error, refresh } = useConsolePayments();

  return (
    <section className="facilitator-console" aria-label="Facilitator console">
      <div className="facilitator-console-heading">
        <div>
          <p className="eyebrow">Operator view</p>
          <h2>Facilitator console</h2>
        </div>
        <button type="button" className="icon-button" aria-label="Refresh" title="Refresh" onClick={refresh}>
          <RotateCcw aria-hidden="true" size={16} />
        </button>
      </div>

      {error && (
        <div className="notice" role="alert">
          <CircleAlert aria-hidden="true" size={18} />
          <span>{error}</span>
        </div>
      )}

      {isLoading && !data && <p className="payment-history-empty">Loading…</p>}

      {data && (
        <>
          <div className="console-rate-limit-cards">
            <div className="console-stat-card">
              <p>per-jkt / min</p>
              <strong>
                {data.rate_limits.tracked_jkt_count} tracked · limit {data.rate_limits.per_jkt_per_minute || "off"}
              </strong>
            </div>
            <div className="console-stat-card">
              <p>per-IP / min</p>
              <strong>
                {data.rate_limits.tracked_ip_count} tracked · limit {data.rate_limits.per_ip_per_minute || "off"}
              </strong>
            </div>
            <div className="console-stat-card">
              <p>429s since start</p>
              <strong>{integerFormatter.format(data.rate_limits.limited_total_count)}</strong>
            </div>
            <div className="console-stat-card">
              <p>Payments shown</p>
              <strong>{data.payments.length}</strong>
            </div>
          </div>

          <div className="console-payments-table">
            <div className="console-payments-row console-payments-header">
              <span>payment_id</span>
              <span>payee</span>
              <span>amount</span>
              <span>status</span>
              <span>confirmations</span>
              <span>age</span>
            </div>
            {data.payments.length === 0 && <p className="payment-history-empty">No settled payments yet.</p>}
            {data.payments.map((row) => (
              <ConsolePaymentTableRow key={row.payment_id} row={row} />
            ))}
          </div>
        </>
      )}
    </section>
  );
}

function ConsolePaymentTableRow({ row }: { row: ConsolePaymentRow }) {
  return (
    <div className="console-payments-row">
      <span className="console-mono">{truncateMiddle(row.payment_id, 10, 8)}</span>
      <span>{row.payee_id}</span>
      <span className="console-mono">{formatZec(row.amount_zat)} ZEC</span>
      <span className={`stage-chip console-outcome-${row.broadcast_outcome.kind}`}>{row.broadcast_outcome.kind}</span>
      <span>{integerFormatter.format(row.confirmation_count ?? 0)}</span>
      <span>{formatAge(row.settled_at_unix_seconds)}</span>
    </div>
  );
}

function formatAge(unixSeconds: number): string {
  const deltaSeconds = Math.max(0, Math.floor(Date.now() / 1000) - unixSeconds);
  if (deltaSeconds < 60) {
    return `${deltaSeconds}s ago`;
  }
  if (deltaSeconds < 3600) {
    return `${Math.floor(deltaSeconds / 60)}m ago`;
  }
  if (deltaSeconds < 86_400) {
    return `${Math.floor(deltaSeconds / 3600)}h ago`;
  }
  return `${Math.floor(deltaSeconds / 86_400)}d ago`;
}
