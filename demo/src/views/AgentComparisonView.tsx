import { Bot, CircleAlert, RotateCcw } from "lucide-react";
import type { AgentPaymentLoop } from "../hooks/useAgentPaymentLoop";
import { integerFormatter } from "../format";

interface AgentComparisonViewProps {
  agentLoop: AgentPaymentLoop;
}

export function AgentComparisonView({ agentLoop }: AgentComparisonViewProps) {
  const {
    isRunning,
    callsCompletedCount,
    spentTotalZat,
    lastError,
    agentCallLimitCount,
    agentSpendCeilingZat,
    hasReachedCeiling,
    startLoop,
    stopLoop,
    resetLoop
  } = agentLoop;

  return (
    <section className="agent-comparison" aria-label="Human versus agent">
      <div className="agent-comparison-intro">
        <h2>An agent that pays per API call</h2>
        <p>
          Scenario: an agent meters a paid endpoint per call. The same task, two trust models: a
          browser web3 wallet that stops on every payment, or zpay's bounded autopay grant that
          doesn't.
        </p>
      </div>

      <div className="agent-comparison-grid">
        <div className="friction-panel">
          <p className="eyebrow">With a browser web3 wallet</p>
          <h3>Every payment stops the agent</h3>
          <ul>
            <li>A human must approve each spend, or the agent holds the seed phrase itself.</li>
            <li>The common workaround is an unlimited token approval: one bug can drain the wallet.</li>
            <li>There is no per-payment cap and no server-side terms to check the request against.</li>
          </ul>
        </div>

        <div className="agent-run-panel">
          <p className="eyebrow">With zpay autopay</p>
          <h3>One bounded grant per payment, no manual approval</h3>
          <p className="agent-run-copy">
            Each call below is a real prepare and settle against the demo stack, signed under a
            fresh single-use zspend authorization grant per payment. zspend grants aren't
            budget-pooled across payments today, so the ceiling below is a demo-side stop
            condition, not a protocol one.
          </p>

          <div className="agent-loop-stats">
            <div>
              <span>Calls completed</span>
              <strong>{integerFormatter.format(callsCompletedCount)}</strong>
            </div>
            <div>
              <span>Spent</span>
              <strong>{integerFormatter.format(spentTotalZat)} zat</strong>
            </div>
          </div>

          <p className="agent-loop-limits">
            Demo ceiling: {agentCallLimitCount} calls or {integerFormatter.format(agentSpendCeilingZat)} zat,
            whichever comes first.
          </p>

          {lastError && (
            <div className="notice" role="alert">
              <CircleAlert aria-hidden="true" size={18} />
              <span>{lastError}</span>
            </div>
          )}

          <div className="action-row">
            {!isRunning && !hasReachedCeiling && (
              <button type="button" className="primary-button" onClick={startLoop}>
                Run agent
                <Bot aria-hidden="true" size={18} />
              </button>
            )}
            {isRunning && (
              <button type="button" className="secondary-button" onClick={stopLoop}>
                Stop
              </button>
            )}
            {!isRunning && hasReachedCeiling && (
              <button type="button" className="secondary-button" onClick={resetLoop}>
                <RotateCcw aria-hidden="true" size={18} />
                Reset
              </button>
            )}
          </div>
        </div>
      </div>
    </section>
  );
}
