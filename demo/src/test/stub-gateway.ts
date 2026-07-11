import { vi } from "vitest";
import type { PaymentBody, ReadinessBody, WalletBody } from "../demo-client";

export class MockEventSource extends EventTarget {
  static instances: MockEventSource[] = [];
  readonly url: string;

  constructor(url: string) {
    super();
    this.url = url;
    MockEventSource.instances.push(this);
  }

  close() {}

  emitSnapshot(payment: PaymentBody) {
    this.dispatchEvent(new MessageEvent("snapshot", { data: JSON.stringify(payment) }));
  }
}

export const readiness: ReadinessBody = {
  network: "testnet",
  zpay: { status: "ready", retryable: false },
  zspend: { status: "ready", retryable: false },
  zinder: { status: "ready", retryable: false, height: 4_152_766 },
  wallet: { status: "ready", retryable: true, height: 4_152_760 },
  faucet: { status: "ready", retryable: false }
};

export const fundedWallet: WalletBody = {
  network: "testnet",
  address: "utest1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq",
  sapling_zat: 0,
  orchard_zat: 120_000,
  ironwood_zat: 0,
  transparent_mature_zat: 0,
  transparent_immature_zat: 0,
  total_zat: 120_000,
  is_funded: true,
  as_of_height: 4_152_760
};

export const preparedPayment: PaymentBody = {
  payment_id: "01JZPAYDEMO000000000000000",
  mode: "checkout",
  stage: "review",
  amount_zat: 50_000,
  expiry_height: 4_152_900,
  confirmation_count: 0,
  reorg_count: 0,
  settled: false,
  can_settle: true,
  message: "Review the payment before signing"
};

export const finalPayment: PaymentBody = {
  ...preparedPayment,
  stage: "paid",
  status: "settled",
  confirmation_count: 3,
  mined_block_height: 4_152_898,
  settled: true,
  transaction_id: "c24c8bcdb22d0afc7f34dd82d9ec1b5aafbf06fd2914d567d2f08be1b9e4e732",
  zexplorer_url:
    "https://zexplorer.app/testnet/tx/c24c8bcdb22d0afc7f34dd82d9ec1b5aafbf06fd2914d567d2f08be1b9e4e732",
  can_settle: false,
  message: "Payment settled"
};

export function stubGateway(options?: {
  readiness?: ReadinessBody;
  wallet?: WalletBody;
  payment?: PaymentBody;
  faucetClaim?: unknown;
  paymentProblem?: unknown;
}) {
  const fetchMock = vi.fn((input: RequestInfo | URL, init?: RequestInit) => {
    const url = String(input);
    if (url.endsWith("/demo/v1/readiness")) {
      return Promise.resolve(jsonResponse(options?.readiness ?? readiness));
    }
    if (url.endsWith("/demo/v1/wallet")) {
      return Promise.resolve(jsonResponse(options?.wallet ?? fundedWallet));
    }
    if (url.endsWith("/demo/v1/faucet-claims") && init?.method === "POST") {
      return Promise.resolve(jsonResponse(options?.faucetClaim ?? { request_id: "fauzec-1" }));
    }
    if (url.endsWith("/demo/v1/payments") && init?.method === "POST") {
      if (options?.paymentProblem) {
        return Promise.resolve(problemResponse(options.paymentProblem));
      }
      return Promise.resolve(jsonResponse(options?.payment ?? preparedPayment));
    }
    return Promise.resolve(
      problemResponse({
        title: "Unexpected route",
        kind: "unexpected_route",
        detail: url,
        retryable: false
      })
    );
  });
  vi.stubGlobal("fetch", fetchMock);
}

export function jsonResponse(responseBody: unknown): Response {
  return new Response(JSON.stringify(responseBody), {
    status: 200,
    headers: { "content-type": "application/json" }
  });
}

export function problemResponse(responseBody: unknown): Response {
  return new Response(JSON.stringify(responseBody), {
    status: 503,
    headers: { "content-type": "application/json" }
  });
}
