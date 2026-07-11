import { expect, type Page, test } from "@playwright/test";

const paymentId = "01JZPAYDEMO000000000000000";
const transactionId = "c24c8bcdb22d0afc7f34dd82d9ec1b5aafbf06fd2914d567d2f08be1b9e4e732";

test("protocol receipt shows an empty state before any payment exists", async ({ page }) => {
  await installGatewayRoutes(page);

  await page.goto("/");
  await page.getByRole("radio", { name: "Timeline" }).click();

  await expect(page.getByText("Start a checkout to see its protocol receipt here.")).toBeVisible();
});

test("protocol receipt reflects a settled payment's lifecycle", async ({ page }) => {
  await installGatewayRoutes(page);

  await page.goto("/");
  await page.getByRole("button", { name: "Pay with ZEC" }).click();
  await page.getByRole("button", { name: "Approve payment" }).click();
  await expect(page.getByText("Report unlocked")).toBeVisible();

  await page.getByRole("radio", { name: "Timeline" }).click();

  await expect(page.getByText("Payment prepared")).toBeVisible();
  await expect(page.getByText("Signed by your wallet")).toBeVisible();
  await expect(page.getByText("Broadcast, watching for confirmations")).toBeVisible();
  await expect(page.locator(".timeline-node-title", { hasText: "Settled" })).toBeVisible();
  await expect(page.getByText(/final and at or below the settled tip/i)).toBeVisible();
});

async function installGatewayRoutes(page: Page) {
  await page.route("**/demo/v1/readiness", async (route) => {
    await route.fulfill({
      json: {
        network: "testnet",
        zpay: { status: "ready", retryable: false },
        zspend: { status: "ready", retryable: false },
        zinder: { status: "ready", retryable: false, height: 4152766 },
        wallet: { status: "ready", retryable: true, height: 4152760 },
        faucet: { status: "ready", retryable: false }
      }
    });
  });
  await page.route("**/demo/v1/wallet", async (route) => {
    await route.fulfill({
      json: {
        network: "testnet",
        address: "utest1qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqq",
        sapling_zat: 0,
        orchard_zat: 120000,
        ironwood_zat: 0,
        transparent_mature_zat: 0,
        transparent_immature_zat: 0,
        total_zat: 120000,
        is_funded: true,
        as_of_height: 4152760
      }
    });
  });
  await page.route("**/demo/v1/payments", async (route) => {
    await route.fulfill({
      json: {
        payment_id: paymentId,
        mode: "checkout",
        stage: "review",
        amount_zat: 50000,
        expiry_height: 4152900,
        confirmation_count: 0,
        reorg_count: 0,
        settled: false,
        can_settle: true,
        message: "Review the payment before signing"
      }
    });
  });
  await page.route("**/demo/v1/payments/*/settle", async (route) => {
    await route.fulfill({
      json: {
        payment_id: paymentId,
        mode: "checkout",
        stage: "paid",
        amount_zat: 50000,
        expiry_height: 4152900,
        status: "settled",
        confirmation_count: 3,
        mined_block_height: 4152888,
        reorg_count: 0,
        settled: true,
        transaction_id: transactionId,
        zexplorer_url: `https://zexplorer.app/testnet/tx/${transactionId}`,
        can_settle: false,
        message: "Payment settled"
      }
    });
  });
  await page.route("**/demo/v1/payments/*/events", async (route) => {
    await route.fulfill({
      status: 200,
      headers: { "content-type": "text/event-stream" },
      body:
        "event: snapshot\n" +
        `data: {"payment_id":"${paymentId}","mode":"checkout","stage":"review","amount_zat":50000,"expiry_height":4152900,"confirmation_count":0,"reorg_count":0,"settled":false,"can_settle":true,"message":"Review the payment before signing"}\n\n`
    });
  });
}
