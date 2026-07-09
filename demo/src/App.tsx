import {
  Bot,
  Check,
  CircleAlert,
  Copy,
  CreditCard,
  Droplet,
  ExternalLink,
  FileText,
  LockKeyhole,
  RotateCcw,
  ShieldCheck,
  WalletCards
} from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import {
  DemoProblem,
  type DemoStage,
  type FaucetClaimBody,
  type PaymentBody,
  type PaymentMode,
  type ReadinessBody,
  type WalletBody,
  createFaucetClaim,
  createPayment,
  getReadiness,
  getWallet,
  paymentEventsUrl,
  settlePayment
} from "./demo-client";

const zecFormatter = new Intl.NumberFormat("en-US", {
  maximumFractionDigits: 8,
  minimumFractionDigits: 0
});

const integerFormatter = new Intl.NumberFormat("en-US");

const stageLabels: DemoStage[] = ["ready", "review", "signing", "confirming", "paid"];

const stageRanks: Record<DemoStage, number> = {
  ready: 0,
  needs_funds: 0,
  review: 1,
  signing: 2,
  settling: 2,
  confirming: 3,
  mined: 3,
  final: 4,
  paid: 4,
  failed: 0,
  expired: 0
};

const readinessKeys = ["zpay", "zspend", "zinder", "wallet", "faucet"] as const;

export function App() {
  const [mode, setMode] = useState<PaymentMode>("checkout");
  const [readiness, setReadiness] = useState<ReadinessBody | null>(null);
  const [wallet, setWallet] = useState<WalletBody | null>(null);
  const [payment, setPayment] = useState<PaymentBody | null>(null);
  const [faucetClaim, setFaucetClaim] = useState<FaucetClaimBody | null>(null);
  const [isPreparing, setIsPreparing] = useState(false);
  const [isSettling, setIsSettling] = useState(false);
  const [isClaiming, setIsClaiming] = useState(false);
  const [isFaucetOpen, setIsFaucetOpen] = useState(false);
  const [copiedLabel, setCopiedLabel] = useState<string | null>(null);
  const [notice, setNotice] = useState<string | null>(null);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    void refreshState();
    return () => {
      mountedRef.current = false;
    };
  }, []);

  useEffect(() => {
    if (wallet?.is_funded || payment) {
      return;
    }
    const refreshTimer = window.setInterval(() => {
      refreshState();
    }, 10_000);
    return () => window.clearInterval(refreshTimer);
  }, [payment, wallet?.is_funded]);

  useEffect(() => {
    if (!payment?.payment_id) {
      return;
    }
    const source = new EventSource(paymentEventsUrl(payment.payment_id));
    source.addEventListener("snapshot", (event) => {
      const nextPayment = JSON.parse((event as MessageEvent<string>).data) as PaymentBody;
      setPayment(nextPayment);
    });
    source.onerror = () => {
      source.close();
    };
    return () => source.close();
  }, [payment?.payment_id]);

  const currentStage = payment?.stage ?? (wallet?.is_funded ? "ready" : "needs_funds");
  const canPrepare = Boolean(wallet?.is_funded) && !isPreparing && !isSettling;
  const canSettle = Boolean(payment?.can_settle) && !isSettling;
  const hasAccess = accessGranted(currentStage);
  const stageLabel = stageLabelFor(currentStage);
  const amountZec = payment ? formatZec(payment.amount_zat) : "0.0005";
  const primaryButtonLabel = payment ? settleLabel(mode) : "Pay with ZEC";
  const primaryButtonIcon = payment ? modeIcon(mode) : <CreditCard aria-hidden="true" size={18} />;
  const primaryButtonBusyLabel = payment ? busySettleLabel(currentStage, mode) : "Preparing…";

  const readinessRows = useMemo(() => {
    if (!readiness) {
      return [];
    }
    return readinessKeys.map((key) => ({
      name: key,
      dependency: readiness[key]
    }));
  }, [readiness]);

  function refreshState() {
    void getReadiness()
      .then((nextReadiness) => {
        if (!mountedRef.current) {
          return;
        }
        setReadiness(nextReadiness);
        if (nextReadiness.wallet.status === "needs_funds") {
          setNotice("The demo wallet needs testnet funds");
        }
      })
      .catch((err: unknown) => {
        if (mountedRef.current) {
          setNotice(friendlyProblem(err));
        }
      })
      .finally(() => {
        void getWallet()
          .then((nextWallet) => {
            if (!mountedRef.current) {
              return;
            }
            setWallet(nextWallet);
            if (!nextWallet.is_funded) {
              setNotice("The demo wallet needs testnet funds");
            }
          })
          .catch((err: unknown) => {
            if (mountedRef.current) {
              setNotice(friendlyProblem(err));
            }
          });
      });
  }

  async function onPrepareClick() {
    setNotice(null);
    setPayment(null);
    setIsPreparing(true);
    try {
      const nextPayment = await createPayment(mode);
      setPayment(nextPayment);
    } catch (err) {
      setNotice(friendlyProblem(err));
    } finally {
      setIsPreparing(false);
    }
  }

  async function onSettleClick() {
    if (!payment) {
      return;
    }
    setNotice(null);
    setIsSettling(true);
    try {
      const nextPayment = await settlePayment(payment.payment_id);
      setPayment(nextPayment);
    } catch (err) {
      setNotice(friendlyProblem(err));
    } finally {
      setIsSettling(false);
    }
  }

  async function onFaucetClick() {
    setIsFaucetOpen(true);
    setNotice(null);
    setIsClaiming(true);
    try {
      const claim = await createFaucetClaim(wallet?.address);
      setFaucetClaim(claim);
    } catch (err) {
      setNotice(friendlyProblem(err));
    } finally {
      setIsClaiming(false);
    }
  }

  async function onCopyClick(text: string, label: string) {
    await navigator.clipboard.writeText(text);
    setCopiedLabel(label);
    window.setTimeout(() => setCopiedLabel(null), 1800);
  }

  function onModeClick(nextMode: PaymentMode) {
    if (payment && !accessGranted(payment.stage)) {
      setPayment(null);
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
        <div className="network-pill" aria-label={`Network ${wallet?.network ?? readiness?.network ?? "testnet"}`}>
          <ShieldCheck aria-hidden="true" size={16} />
          {wallet?.network ?? readiness?.network ?? "testnet"}
        </div>
      </header>

      <main className="app-shell" id="main">
        <section className="report-panel" aria-labelledby="report-title">
          <div className="report-toolbar">
            <div>
              <p className="eyebrow">Aether research</p>
              <h2 id="report-title">Q3 private market signal</h2>
            </div>
            <span className={`access-chip ${hasAccess ? "paid" : "locked"}`}>{hasAccess ? "paid" : "locked"}</span>
          </div>

          <div className={`report-preview ${hasAccess ? "is-open" : ""}`}>
            <div className="report-visual" aria-hidden="true">
              <span style={{ height: "48%" }} />
              <span style={{ height: "64%" }} />
              <span style={{ height: "38%" }} />
              <span style={{ height: "82%" }} />
              <span style={{ height: "57%" }} />
              <span style={{ height: "72%" }} />
            </div>
            <div className="report-lines" aria-hidden="true">
              <span />
              <span />
              <span />
              <span />
            </div>
            {!hasAccess && (
              <div className="lock-layer">
                <LockKeyhole aria-hidden="true" size={28} />
                <p>Payment required</p>
              </div>
            )}
          </div>

          <article className="report-content" id="report-content" aria-live="polite">
            {hasAccess ? (
              <>
                <h3>Report unlocked</h3>
                <p>
                  Renewable demand rose across synthetic fuels, private credit spreads tightened, and
                  settlement activity clustered around two late-cycle liquidity windows.
                </p>
                <a className="secondary-button" href="#report-content">
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

        <aside className="payment-panel" id="checkout-panel" aria-labelledby="payment-title">
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
              onClick={() => onModeClick("checkout")}
            >
              <WalletCards aria-hidden="true" size={18} />
              Checkout
            </button>
            <button
              type="button"
              role="radio"
              aria-checked={mode === "autopay"}
              className={mode === "autopay" ? "is-selected" : ""}
              onClick={() => onModeClick("autopay")}
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

          <div className="timeline" aria-label="Payment progress">
            {stageLabels.map((label) => (
              <div
                key={label}
                className={stageRanks[currentStage] >= stageRanks[label] ? "is-active" : ""}
                aria-current={currentStage === label ? "step" : undefined}
              >
                <span />
                <p>{label}</p>
              </div>
            ))}
          </div>

          <p className="status-line" aria-live="polite">
            {isPreparing
              ? "Preparing…"
              : isSettling
                ? primaryButtonBusyLabel
                : payment?.message ?? readinessMessage(wallet, readiness)}
          </p>

          <div className="action-row">
            {!payment && (
              <button type="button" className="primary-button" onClick={onPrepareClick} disabled={!canPrepare}>
                {isPreparing ? "Preparing…" : primaryButtonLabel}
                {primaryButtonIcon}
              </button>
            )}
            {payment && !accessGranted(payment.stage) && (
              <button type="button" className="primary-button" onClick={onSettleClick} disabled={!canSettle}>
                {isSettling ? primaryButtonBusyLabel : primaryButtonLabel}
                {primaryButtonIcon}
              </button>
            )}
            {(payment?.stage === "failed" || payment?.stage === "expired") && (
              <button type="button" className="secondary-button" onClick={() => setPayment(null)}>
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

          <details className="payment-details">
            <summary>Payment details</summary>
            <dl>
              <div>
                <dt>Payment ID</dt>
                <dd>{payment ? truncateMiddle(payment.payment_id, 12, 10) : "Not prepared"}</dd>
              </div>
              <div>
                <dt>Expiry height</dt>
                <dd>{payment ? integerFormatter.format(payment.expiry_height) : "Not prepared"}</dd>
              </div>
              <div>
                <dt>Confirmations</dt>
                <dd>{integerFormatter.format(payment?.confirmation_count ?? 0)}</dd>
              </div>
              <div>
                <dt>Settled</dt>
                <dd>{payment?.settled ? "yes" : "no"}</dd>
              </div>
              <div>
                <dt>Transaction</dt>
                <dd>{payment?.transaction_id ? truncateMiddle(payment.transaction_id, 12, 12) : "Pending"}</dd>
              </div>
            </dl>
            {readinessRows.length > 0 && (
              <ul className="readiness-list" aria-label="Readiness">
                {readinessRows.map((row) => (
                  <li key={row.name}>
                    <span>{row.name}</span>
                    <strong>{row.dependency.status}</strong>
                  </li>
                ))}
              </ul>
            )}
          </details>
        </aside>
      </main>
    </>
  );
}

function modeIcon(mode: PaymentMode) {
  return mode === "checkout" ? <Check aria-hidden="true" size={18} /> : <Bot aria-hidden="true" size={18} />;
}

function settleLabel(mode: PaymentMode): string {
  return mode === "checkout" ? "Approve payment" : "Start autopay";
}

function busySettleLabel(stage: DemoStage, mode: PaymentMode): string {
  if (stage === "settling") {
    return "Settling…";
  }
  if (stage === "confirming" || stage === "mined" || stage === "final") {
    return "Confirming…";
  }
  return mode === "checkout" ? "Signing…" : "Signing…";
}

function accessGranted(stage: DemoStage): boolean {
  return stage === "final" || stage === "paid";
}

function stageLabelFor(stage: DemoStage): string {
  if (stage === "needs_funds") {
    return "ready";
  }
  if (stage === "final") {
    return "paid";
  }
  if (stage === "settling" || stage === "mined") {
    return "confirming";
  }
  return stage;
}

function readinessMessage(wallet: WalletBody | null, readiness: ReadinessBody | null): string {
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

function faucetClaimText(claim: FaucetClaimBody | null, isClaiming: boolean): string {
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

function friendlyProblem(err: unknown): string {
  if (err instanceof DemoProblem) {
    if (err.kind.includes("wallet_needs_funds")) {
      return "The demo wallet needs testnet funds";
    }
    if (err.kind.includes("zinder") || err.kind.includes("settle_failed")) {
      return "zpay can't reach zinder. Check readiness, then try again.";
    }
    if (err.kind.includes("zspend")) {
      return "zspend isn't ready. Wait for sync, then try again.";
    }
    if (err.kind.includes("issuer") || err.kind.includes("access_token")) {
      return "Autopay isn't configured. Check zspend JWKS, then try again.";
    }
    if (err.kind.includes("expired")) {
      return "This payment expired. Start a new checkout";
    }
    return err.message;
  }
  return "zpay can't reach zinder. Check readiness, then try again.";
}

function formatZec(zat: number): string {
  return zecFormatter.format(zat / 100_000_000);
}

function truncateMiddle(text: string, startCount: number, endCount: number): string {
  if (text.length <= startCount + endCount + 3) {
    return text;
  }
  return `${text.slice(0, startCount)}…${text.slice(-endCount)}`;
}
