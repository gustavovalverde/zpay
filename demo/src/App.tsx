import { useState } from "react";
import { DemoNav } from "./components/DemoNav";
import type { PaymentMode } from "./demo-client";
import { useAgentPaymentLoop } from "./hooks/useAgentPaymentLoop";
import { usePaymentSession } from "./hooks/usePaymentSession";
import { useWalletReadiness } from "./hooks/useWalletReadiness";
import { accessGranted } from "./stage";
import { AgentComparisonView } from "./views/AgentComparisonView";
import { FacilitatorConsoleView } from "./views/FacilitatorConsoleView";
import { GuidedCheckoutView } from "./views/GuidedCheckoutView";
import { ProtocolReceiptView } from "./views/ProtocolReceiptView";
import { ReceiptHistoryView } from "./views/ReceiptHistoryView";

export type DemoView = "checkout" | "receipt" | "agent" | "receipts" | "console";

const DEMO_VIEWS: { id: DemoView; label: string }[] = [
  { id: "checkout", label: "Checkout" },
  { id: "receipt", label: "Timeline" },
  { id: "agent", label: "Agent" },
  { id: "receipts", label: "Receipts" },
  { id: "console", label: "Console" }
];

export function App() {
  const [view, setView] = useState<DemoView>("checkout");
  const [mode, setMode] = useState<PaymentMode>("checkout");
  const [notice, setNotice] = useState<string | null>(null);

  const paymentSession = usePaymentSession(mode, setNotice);
  const walletReadiness = useWalletReadiness(setNotice, Boolean(paymentSession.payment));
  const agentLoop = useAgentPaymentLoop();

  function onModeChange(nextMode: PaymentMode) {
    if (paymentSession.payment && !accessGranted(paymentSession.payment.stage)) {
      paymentSession.resetPayment();
    }
    setMode(nextMode);
    setNotice(null);
  }

  return (
    <>
      <a className="skip-link" href="#checkout-panel">
        Skip to checkout
      </a>
      <header className="app-header">
        <div>
          <p className="eyebrow">Zcash x402 demo</p>
          <h1>Unlock with ZEC</h1>
        </div>
        <DemoNav
          activeView={view}
          views={DEMO_VIEWS}
          onSelectView={setView}
          network={walletReadiness.wallet?.network ?? walletReadiness.readiness?.network ?? "testnet"}
        />
      </header>

      <main className="app-shell" id="main">
        {view === "checkout" && (
          <GuidedCheckoutView
            mode={mode}
            onModeChange={onModeChange}
            notice={notice}
            walletReadiness={walletReadiness}
            paymentSession={paymentSession}
          />
        )}
        {view === "receipt" && <ProtocolReceiptView mode={mode} paymentSession={paymentSession} />}
        {view === "agent" && <AgentComparisonView agentLoop={agentLoop} />}
        {view === "receipts" && <ReceiptHistoryView />}
        {view === "console" && <FacilitatorConsoleView />}
      </main>
    </>
  );
}
