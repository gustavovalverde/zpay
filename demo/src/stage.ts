import type { DemoStage } from "./demo-client";

export function accessGranted(stage: DemoStage): boolean {
  return stage === "final" || stage === "paid";
}

export function stageLabelFor(stage: DemoStage): string {
  if (stage === "needs_funds") {
    return "ready";
  }
  if (stage === "final") {
    return "paid";
  }
  if (stage === "settling" || stage === "mined") {
    return "confirming";
  }
  return stage;
}
