import { expect, type Page, test } from "@playwright/test";

test("facilitator console shows rate limits and a settled payment", async ({ page }) => {
  await installGatewayRoutes(page);

  await page.goto("/");
  await page.getByRole("radio", { name: "Console" }).click();

  await expect(page.getByRole("heading", { name: "Facilitator console" })).toBeVisible();
  await expect(page.locator(".console-stat-card", { hasText: "per-jkt / min" })).toContainText("limit 120");
  await expect(page.locator(".console-payments-row").filter({ hasText: "aether-demo" })).toBeVisible();
  await expect(page.locator(".console-payments-row", { hasText: "accepted" })).toBeVisible();
});

test("facilitator console shows an empty state with no settled payments", async ({ page }) => {
  await installGatewayRoutes(page, { emptyPayments: true });

  await page.goto("/");
  await page.getByRole("radio", { name: "Console" }).click();

  await expect(page.getByText("No settled payments yet.")).toBeVisible();
});

async function installGatewayRoutes(page: Page, options?: { emptyPayments?: boolean }) {
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
  await page.route("**/demo/v1/console/payments", async (route) => {
    await route.fulfill({
      json: {
        payments: options?.emptyPayments
          ? []
          : [
              {
                payment_id: "01KX6D3QMFY5RZA8YA7C6S16S6",
                payee_id: "aether-demo",
                amount_zat: 100000,
                broadcast_outcome: { kind: "accepted", transaction_id: "deadbeef" },
                confirmation_count: 3,
                mined_block_height: 4157800,
                reorg_count: 0,
                settled_at_unix_seconds: Math.floor(Date.now() / 1000) - 120
              }
            ],
        rate_limits: {
          per_jkt_per_minute: 120,
          per_ip_per_minute: 600,
          tracked_jkt_count: 1,
          tracked_ip_count: 2,
          limited_total_count: 0
        }
      }
    });
  });
}
