# Proposal-0005: zentity MCP `purchase` tool gains `scheme: "zcash"` branch

| Field | Value |
| ----- | ----- |
| Status | Proposed |
| Consumer | zpay (downstream of zentity MCP) |
| Upstream | zentity |
| Pinned at | n/a (HTTP-only dependency) |
| Related | [PRD-42 Phase 6](https://github.com/gustavovalverde/zentity/blob/main/docs/plans/prd-42-zcash-agentic-payments-cross-stack.md), [zentity RFC-0048](https://github.com/gustavovalverde/zentity/blob/main/docs/rfcs/0048-zcash-x402-agent-payments.md) |

## Context

zentity's MCP server (`apps/mcp/src/tools/purchase.ts`) implements an agent-callable `purchase` tool that today supports `scheme: "evm"` only, routing to a Base Sepolia x402 facilitator. PRD-42 extends this to `scheme: "zcash"`, routing to a zpay deployment.

The wire shape stays the same on the MCP side; the agent picks a scheme via the tool call. zentity owns the CIBA approval flow, the PoH-token issuance, and the agent-assertion lifecycle. zpay owns the Zcash-side prepare-settle-confirm.

## Ask

Extend `apps/mcp/src/tools/purchase.ts`:

```typescript
const PurchaseSchema = z.object({
  scheme: z.enum(["evm", "zcash"]),
  // ...
});

async function handlePurchase(input: PurchaseInput, ctx: McpCtx) {
  switch (input.scheme) {
    case "evm":
      return purchaseViaBaseX402(input, ctx);
    case "zcash":
      return purchaseViaZpay(input, ctx);
  }
}
```

`purchaseViaZpay` calls zpay's `/zpay/v1/prepare` and `/zpay/v1/settle` with the same agent assertion + DPoP envelope already used for the Base flow.

zentity registers `zcash_testnet` in `apps/web/src/lib/blockchain/networks.ts` with `paymentScheme: "zcash"`. The CIBA `authorization_details` propagation in `customGrantTypeHandlers/ciba.ts` handles `scheme: "zcash"`, `amount.currency: "ZEC"`, `payTo: "u1..."`, `challengeId`, `memoChallengeHash`, and `minComplianceLevel`.

## Why this lives in zentity, not zpay

zentity is the MCP gateway (PRD-42 Decision 8). Putting an MCP server in zpay creates a second approval path that bypasses CIBA. zentity keeps one approval flow, one identity layer, one MCP tool surface; zpay stays a callable HTTPS-only service.

## Compatibility

Additive. Existing `scheme: "evm"` callers unchanged. Agents that don't know about Zcash never see the new branch.

## Acceptance

- `apps/mcp/src/tools/purchase.ts` exports a working `scheme: "zcash"` branch.
- `apps/web/src/lib/blockchain/networks.ts` includes `zcash_testnet`.
- `customGrantTypeHandlers/ciba.ts` propagates Zcash `authorization_details` onto `ciba_request` and into the access token `act` claim.
- The Aether AI demo at `/aether` completes a confirmed testnet ZEC purchase end-to-end.

Once accepted: zentity bumps its main branch; PRD-42 M3 declares done.
