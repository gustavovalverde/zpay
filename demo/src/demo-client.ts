export type PaymentMode = "checkout" | "autopay";

export type DemoStage =
  | "ready"
  | "needs_funds"
  | "review"
  | "signing"
  | "settling"
  | "confirming"
  | "mined"
  | "final"
  | "paid"
  | "failed"
  | "expired";

export interface ProblemBody {
  title: string;
  kind: string;
  detail: string;
  retryable: boolean;
}

export interface DependencyBody {
  status: string;
  kind?: string;
  detail?: string;
  retryable: boolean;
  height?: number;
}

export interface ReadinessBody {
  network: string;
  zpay: DependencyBody;
  zspend: DependencyBody;
  zinder: DependencyBody;
  wallet: DependencyBody;
  faucet: DependencyBody;
}

export interface WalletBody {
  network: string;
  address: string;
  sapling_zat: number;
  orchard_zat: number;
  ironwood_zat: number;
  transparent_mature_zat: number;
  transparent_immature_zat: number;
  total_zat: number;
  is_funded: boolean;
  as_of_height?: number;
}

export interface FaucetClaimBody {
  request_id?: string;
  txid?: string;
  state?: string;
  outcome?: string;
  confirmed_height?: number;
  error_code?: string;
  next_eligible_at_ms?: number;
}

export interface PaymentBody {
  payment_id: string;
  mode: PaymentMode;
  stage: DemoStage;
  amount_zat: number;
  expiry_height: number;
  status?: string;
  confirmation_count?: number;
  mined_block_height?: number;
  reorg_count: number;
  settled: boolean;
  transaction_id?: string;
  zexplorer_url?: string;
  can_settle: boolean;
  message: string;
}

export type CryptographicVerdict = "valid" | "invalid_signature" | "malformed" | "inconclusive";
export type InconclusiveReason = "unsupported_pool" | "unknown_version" | "prevout_unresolved";
export type ChainPresence = "mined" | "not_found" | "oracle_unavailable";
export type AmountReconciliation = "match" | "mismatch" | "not_checked";

export interface VerifyRequestBody {
  txid: string;
  expected_amount_zat: number;
  disclosure_payload_hex: string;
}

export interface VerifyResponseBody {
  cryptographic_verdict: CryptographicVerdict;
  inconclusive_reason?: InconclusiveReason;
  chain_presence: ChainPresence;
  amount_reconciliation: AmountReconciliation;
  transaction_id?: string;
  payment_id?: string;
  disclosed_value_zat?: number;
}

export interface ConsoleBroadcastOutcomeBody {
  kind: string;
  transaction_id?: string;
  upstream_message?: string;
}

export interface ConsolePaymentRow {
  payment_id: string;
  payee_id: string;
  amount_zat: number;
  broadcast_outcome: ConsoleBroadcastOutcomeBody;
  confirmation_count?: number;
  mined_block_height?: number;
  reorg_count: number;
  settled_at_unix_seconds: number;
}

export interface ConsoleRateLimitsBody {
  per_jkt_per_minute: number;
  per_ip_per_minute: number;
  tracked_jkt_count: number;
  tracked_ip_count: number;
  limited_total_count: number;
}

export interface ConsolePaymentsBody {
  payments: ConsolePaymentRow[];
  rate_limits: ConsoleRateLimitsBody;
}

export class DemoProblem extends Error {
  readonly kind: string;
  readonly retryable: boolean;
  readonly title: string;

  constructor(problem: ProblemBody) {
    super(problem.detail);
    this.kind = problem.kind;
    this.retryable = problem.retryable;
    this.title = problem.title;
  }
}

async function requestJson<T>(path: string, init?: RequestInit): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: {
      "content-type": "application/json",
      ...init?.headers
    }
  });
  const responseBody = (await response.json().catch(() => null)) as T | ProblemBody | null;
  if (!response.ok) {
    if (isProblemBody(responseBody)) {
      throw new DemoProblem(responseBody);
    }
    throw new DemoProblem({
      title: "Demo request failed",
      kind: "demo_request_failed",
      detail: `The demo gateway returned HTTP ${response.status}`,
      retryable: true
    });
  }
  return responseBody as T;
}

function isProblemBody(candidate: unknown): candidate is ProblemBody {
  if (candidate === null || typeof candidate !== "object") {
    return false;
  }
  const fields = candidate as Partial<ProblemBody>;
  return (
    typeof fields.title === "string" &&
    typeof fields.kind === "string" &&
    typeof fields.detail === "string" &&
    typeof fields.retryable === "boolean"
  );
}

export function getReadiness(): Promise<ReadinessBody> {
  return requestJson<ReadinessBody>("/demo/v1/readiness");
}

export function getWallet(): Promise<WalletBody> {
  return requestJson<WalletBody>("/demo/v1/wallet");
}

export function createFaucetClaim(address?: string): Promise<FaucetClaimBody> {
  return requestJson<FaucetClaimBody>("/demo/v1/faucet-claims", {
    method: "POST",
    body: JSON.stringify({ address })
  });
}

export function getFaucetClaim(requestId: string): Promise<FaucetClaimBody> {
  return requestJson<FaucetClaimBody>(`/demo/v1/faucet-claims/${requestId}`);
}

export function createPayment(mode: PaymentMode): Promise<PaymentBody> {
  return requestJson<PaymentBody>("/demo/v1/payments", {
    method: "POST",
    body: JSON.stringify({ mode })
  });
}

export function settlePayment(paymentId: string): Promise<PaymentBody> {
  return requestJson<PaymentBody>(`/demo/v1/payments/${paymentId}/settle`, {
    method: "POST",
    body: "{}"
  });
}

export function paymentEventsUrl(paymentId: string): string {
  return `/demo/v1/payments/${paymentId}/events`;
}

export function listPayments(): Promise<PaymentBody[]> {
  return requestJson<PaymentBody[]>("/demo/v1/payments");
}

export function verifyPaymentReceipt(request: VerifyRequestBody): Promise<VerifyResponseBody> {
  return requestJson<VerifyResponseBody>("/demo/v1/verify", {
    method: "POST",
    body: JSON.stringify(request)
  });
}

export function getConsolePayments(): Promise<ConsolePaymentsBody> {
  return requestJson<ConsolePaymentsBody>("/demo/v1/console/payments");
}
