import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import type { PaymentBody, ReadinessBody, WalletBody } from "./demo-client";

class MockEventSource extends EventTarget {
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

const readiness: ReadinessBody = {
  network: "testnet",
  zpay: { status: "ready", retryable: false },
  zspend: { status: "ready", retryable: false },
  zinder: { status: "ready", retryable: false, height: 4_152_766 },
  wallet: { status: "ready", retryable: true, height: 4_152_760 },
  faucet: { status: "ready", retryable: false }
};

const fundedWallet: WalletBody = {
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

const preparedPayment: PaymentBody = {
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

const finalPayment: PaymentBody = {
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

beforeEach(() => {
  MockEventSource.instances = [];
  vi.stubGlobal("EventSource", MockEventSource);
  Object.assign(navigator, {
    clipboard: {
      writeText: vi.fn().mockResolvedValue(undefined)
    }
  });
});

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("App", () => {
  it("switches modes and prepares an autopay checkout", async () => {
    stubGateway({ payment: { ...preparedPayment, mode: "autopay" } });
    render(<App />);

    await screen.findByText("Ready to start checkout");
    await userEvent.click(screen.getByRole("radio", { name: /autopay/i }));
    await userEvent.click(screen.getByRole("button", { name: /pay with zec/i }));

    expect(await screen.findByRole("button", { name: /start autopay/i })).toBeEnabled();
    expect(screen.getAllByText("review").length).toBeGreaterThan(0);
  });

  it("shows the funded error and faucet drawer", async () => {
    stubGateway({
      readiness: {
        ...readiness,
        wallet: { status: "needs_funds", retryable: true, detail: "The demo wallet needs testnet funds" }
      },
      wallet: { ...fundedWallet, total_zat: 0, orchard_zat: 0, is_funded: false },
      faucetClaim: { request_id: "fauzec-1", txid: "abc123", state: "submitted" }
    });
    render(<App />);

    expect((await screen.findAllByText("The demo wallet needs testnet funds")).length).toBeGreaterThan(0);
    await userEvent.click(screen.getByRole("button", { name: /use faucet/i }));
    expect(await screen.findByText(/claim submitted/i)).toBeInTheDocument();
  });

  it("unlocks the report from an SSE update", async () => {
    stubGateway({ payment: preparedPayment });
    render(<App />);

    await screen.findByText("Ready to start checkout");
    await userEvent.click(screen.getByRole("button", { name: /pay with zec/i }));
    await screen.findByRole("button", { name: /approve payment/i });

    await waitFor(() => expect(MockEventSource.instances.length).toBeGreaterThanOrEqual(1));
    MockEventSource.instances.at(-1)!.emitSnapshot(finalPayment);

    expect(await screen.findByText("Report unlocked")).toBeInTheDocument();
    expect(screen.getByRole("link", { name: /view transaction/i })).toHaveAttribute(
      "href",
      finalPayment.zexplorer_url
    );
  });

  it("maps zspend readiness errors to the expected next step", async () => {
    stubGateway({
      paymentProblem: {
        title: "Demo gateway unavailable",
        kind: "zspend_unavailable",
        detail: "connect refused",
        retryable: true
      }
    });
    render(<App />);

    await screen.findByText("Ready to start checkout");
    await userEvent.click(screen.getByRole("radio", { name: /autopay/i }));
    await userEvent.click(screen.getByRole("button", { name: /pay with zec/i }));

    expect(await screen.findByText("zspend isn't ready. Wait for sync, then try again.")).toBeInTheDocument();
  });

  it("maps issuer errors to the autopay setup next step", async () => {
    stubGateway({
      paymentProblem: {
        title: "Rejected",
        kind: "issuer_key_invalid",
        detail: "issuer key must be Ed25519 or P-256 PKCS#8 PEM",
        retryable: false
      }
    });
    render(<App />);

    await screen.findByText("Ready to start checkout");
    await userEvent.click(screen.getByRole("button", { name: /pay with zec/i }));

    expect(await screen.findByText("Autopay isn't configured. Check zspend JWKS, then try again.")).toBeInTheDocument();
  });
});

function stubGateway(options?: {
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
    return Promise.resolve(problemResponse({
      title: "Unexpected route",
      kind: "unexpected_route",
      detail: url,
      retryable: false
    }));
  });
  vi.stubGlobal("fetch", fetchMock);
}

function jsonResponse(responseBody: unknown): Response {
  return new Response(JSON.stringify(responseBody), {
    status: 200,
    headers: { "content-type": "application/json" }
  });
}

function problemResponse(responseBody: unknown): Response {
  return new Response(JSON.stringify(responseBody), {
    status: 503,
    headers: { "content-type": "application/json" }
  });
}
