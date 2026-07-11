import { render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { MockEventSource, finalPayment, preparedPayment, stubGateway } from "./test/stub-gateway";

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
        network: "testnet",
        zpay: { status: "ready", retryable: false },
        zspend: { status: "ready", retryable: false },
        zinder: { status: "ready", retryable: false, height: 4_152_766 },
        wallet: { status: "needs_funds", retryable: true, detail: "The demo wallet needs testnet funds" },
        faucet: { status: "ready", retryable: false }
      },
      wallet: {
        network: "testnet",
        address: "utest1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq",
        sapling_zat: 0,
        orchard_zat: 0,
        ironwood_zat: 0,
        transparent_mature_zat: 0,
        transparent_immature_zat: 0,
        total_zat: 0,
        is_funded: false,
        as_of_height: 4_152_760
      },
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

  it("verifies a wallet-produced disclosure from a settled receipt", async () => {
    stubGateway({ payments: [finalPayment] });
    render(<App />);

    await screen.findByText("Ready to start checkout");
    await userEvent.click(screen.getByRole("radio", { name: "Receipts" }));
    expect((await screen.findAllByText("0.0005 ZEC")).length).toBeGreaterThan(0);
    await userEvent.click(screen.getByRole("button", { name: "Verify payment disclosure" }));

    expect(await screen.findByText("cryptographic_verdict")).toBeInTheDocument();
    expect(screen.getAllByText("match")).toHaveLength(3);
    expect(screen.queryByText(/empty disclosure payload/i)).not.toBeInTheDocument();
  });
});
