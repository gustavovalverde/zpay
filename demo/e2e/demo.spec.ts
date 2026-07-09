import { expect, type Page, test } from "@playwright/test";

const paymentId = "01JZPAYDEMO000000000000000";
const transactionId = "c24c8bcdb22d0afc7f34dd82d9ec1b5aafbf06fd2914d567d2f08be1b9e4e732";

test("checkout mode unlocks the report after final confirmation", async ({ page }) => {
  await installGatewayRoutes(page, "checkout");

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Unlock with ZEC" })).toBeVisible();
  await page.getByRole("button", { name: "Pay with ZEC" }).click();
  await expect(page.getByRole("button", { name: "Approve payment" })).toBeVisible();
  await page.getByRole("button", { name: "Approve payment" }).click();

  await expect(page.getByText("Report unlocked")).toBeVisible();
  await expect(page.getByText("paid").first()).toBeVisible();
  await expect(page.getByRole("link", { name: "View transaction" })).toHaveAttribute(
    "href",
    `https://zexplorer.app/testnet/tx/${transactionId}`
  );
});

test("autopay mode unlocks the report after final confirmation", async ({ page }) => {
  await installGatewayRoutes(page, "autopay");

  await page.goto("/");
  await page.getByRole("radio", { name: "Autopay" }).click();
  await page.getByRole("button", { name: "Pay with ZEC" }).click();
  await expect(page.getByRole("button", { name: "Start autopay" })).toBeVisible();
  await page.getByRole("button", { name: "Start autopay" }).click();

  await expect(page.getByText("Report unlocked")).toBeVisible();
  await expect(page.getByText("paid").first()).toBeVisible();
});

async function installGatewayRoutes(page: Page, paymentMode: "checkout" | "autopay") {
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
        mode: paymentMode,
        stage: "review",
        amount_zat: 50000,
        expiry_height: 4152900,
        confirmation_count: 0,
        reorg_count: 0,
        settled: false,
        can_settle: true,
        message:
          paymentMode === "checkout"
            ? "Review the payment before signing"
            : "Review the autopay authorization before signing"
      }
    });
  });
  await page.route("**/demo/v1/payments/*/settle", async (route) => {
    await route.fulfill({
      json: {
        payment_id: paymentId,
        mode: paymentMode,
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
        `data: {\"payment_id\":\"${paymentId}\",\"mode\":\"${paymentMode}\",\"stage\":\"review\",\"amount_zat\":50000,\"expiry_height\":4152900,\"confirmation_count\":0,\"reorg_count\":0,\"settled\":false,\"can_settle\":true,\"message\":\"Review the payment before signing\"}\n\n`
    });
  });
}
