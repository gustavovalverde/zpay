import { useRef, useState } from "react";
import { createPayment, settlePayment } from "../demo-client";
import { friendlyProblem } from "../friendly-problem";

const AGENT_CALL_LIMIT_COUNT = 5;
// The per-call amount is server-configured (the demo payee's registered accepts[] entry), not
// controlled here. This ceiling is a generous backstop so the call count above is what normally
// governs the loop; it only bites if the configured amount is unexpectedly large.
const AGENT_SPEND_CEILING_ZAT = 5_000_000;

export interface AgentPaymentLoop {
  isRunning: boolean;
  callsCompletedCount: number;
  spentTotalZat: number;
  lastError: string | null;
  agentCallLimitCount: number;
  agentSpendCeilingZat: number;
  hasReachedCeiling: boolean;
  startLoop: () => void;
  stopLoop: () => void;
  resetLoop: () => void;
}

export function useAgentPaymentLoop(): AgentPaymentLoop {
  const [isRunning, setIsRunning] = useState(false);
  const [callsCompletedCount, setCallsCompletedCount] = useState(0);
  const [spentTotalZat, setSpentTotalZat] = useState(0);
  const [lastError, setLastError] = useState<string | null>(null);
  const stopRequestedRef = useRef(false);
  const runningRef = useRef(false);

  const hasReachedCeiling = callsCompletedCount >= AGENT_CALL_LIMIT_COUNT || spentTotalZat >= AGENT_SPEND_CEILING_ZAT;

  function stopLoop() {
    stopRequestedRef.current = true;
  }

  function resetLoop() {
    stopRequestedRef.current = true;
    setCallsCompletedCount(0);
    setSpentTotalZat(0);
    setLastError(null);
  }

  async function startLoop() {
    if (runningRef.current || hasReachedCeiling) {
      return;
    }
    runningRef.current = true;
    stopRequestedRef.current = false;
    setIsRunning(true);
    setLastError(null);

    let calls = callsCompletedCount;
    let spent = spentTotalZat;

    while (!stopRequestedRef.current && calls < AGENT_CALL_LIMIT_COUNT && spent < AGENT_SPEND_CEILING_ZAT) {
      try {
        const prepared = await createPayment("autopay");
        const settled = await settlePayment(prepared.payment_id);
        calls += 1;
        spent += settled.amount_zat;
        setCallsCompletedCount(calls);
        setSpentTotalZat(spent);
      } catch (err) {
        setLastError(friendlyProblem(err));
        break;
      }
    }

    runningRef.current = false;
    setIsRunning(false);
  }

  return {
    isRunning,
    callsCompletedCount,
    spentTotalZat,
    lastError,
    agentCallLimitCount: AGENT_CALL_LIMIT_COUNT,
    agentSpendCeilingZat: AGENT_SPEND_CEILING_ZAT,
    hasReachedCeiling,
    startLoop,
    stopLoop,
    resetLoop
  };
}
