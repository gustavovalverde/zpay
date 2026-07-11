import { DemoProblem } from "./demo-client";

export function friendlyProblem(err: unknown): string {
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
