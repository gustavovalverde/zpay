import { expect, type Page, test } from "@playwright/test";

const agentCallLimitCount = 5;

test("agent loop runs to its demo call ceiling and can be reset", async ({ page }) => {
  await installGatewayRoutes(page);

  await page.goto("/");
  await page.getByRole("radio", { name: "Agent" }).click();

  await expect(page.getByRole("heading", { name: "An agent that pays per API call" })).toBeVisible();
  await page.getByRole("button", { name: "Run agent" }).click();

  await expect(page.getByRole("button", { name: "Reset" })).toBeVisible({ timeout: 15000 });
  await expect(page.locator(".agent-loop-stats strong").first()).toHaveText(String(agentCallLimitCount));

  await page.getByRole("button", { name: "Reset" }).click();
  await expect(page.locator(".agent-loop-stats strong").first()).toHaveText("0");
  await expect(page.getByRole("button", { name: "Run agent" })).toBeVisible();
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

  let callIndex = 0;
  await page.route("**/demo/v1/payments", async (route) => {
    callIndex += 1;
    const paymentId = `01JZPAYAGENT00000000000${String(callIndex).padStart(3, "0")}`;
    await route.fulfill({
      json: {
        payment_id: paymentId,
        mode: "autopay",
        stage: "review",
        amount_zat: 2500,
        expiry_height: 4152900,
        confirmation_count: 0,
        reorg_count: 0,
        settled: false,
        can_settle: true,
        message: "Review the autopay authorization before signing"
      }
    });
  });
  await page.route("**/demo/v1/payments/*/settle", async (route) => {
    const paymentId = route.request().url().split("/").slice(-2, -1)[0];
    await route.fulfill({
      json: {
        payment_id: paymentId,
        mode: "autopay",
        stage: "paid",
        amount_zat: 2500,
        expiry_height: 4152900,
        status: "settled",
        confirmation_count: 3,
        mined_block_height: 4152888,
        reorg_count: 0,
        settled: true,
        transaction_id: `${paymentId}txid`,
        can_settle: false,
        message: "Payment settled"
      }
    });
  });
}
