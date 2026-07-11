import type { DemoStage } from "./demo-client";

export type PaymentStep = "quote" | "authorize" | "broadcast" | "settled";

export const STEP_ORDER: PaymentStep[] = ["quote", "authorize", "broadcast", "settled"];

const CURRENT_STEP_BY_STAGE: Partial<Record<DemoStage, PaymentStep>> = {
  review: "authorize",
  signing: "authorize",
  settling: "authorize",
  confirming: "broadcast",
  mined: "broadcast",
  final: "settled",
  paid: "settled"
};

const STAGES_BY_STEP: Record<PaymentStep, DemoStage[]> = {
  quote: ["review"],
  authorize: ["signing", "settling"],
  broadcast: ["confirming", "mined"],
  settled: ["final", "paid"]
};

export function paymentStepFor(stage: DemoStage): PaymentStep | null {
  return CURRENT_STEP_BY_STAGE[stage] ?? null;
}

export function isReachedStep(step: PaymentStep, currentStep: PaymentStep | null): boolean {
  if (!currentStep) {
    return false;
  }
  return STEP_ORDER.indexOf(step) <= STEP_ORDER.indexOf(currentStep);
}

export function observedAtForStep(
  step: PaymentStep,
  stageObservedAtMs: Partial<Record<DemoStage, number>>
): number | null {
  const candidates = STAGES_BY_STEP[step]
    .map((stage) => stageObservedAtMs[stage])
    .filter((value): value is number => value !== undefined);
  if (candidates.length === 0) {
    return null;
  }
  return Math.min(...candidates);
}
