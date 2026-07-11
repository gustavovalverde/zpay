import { CircleAlert } from "lucide-react";
import type { DemoStage } from "../demo-client";
import { STEP_ORDER, isReachedStep, paymentStepFor } from "../payment-step";

const STEP_LABELS: Record<(typeof STEP_ORDER)[number], string> = {
  quote: "Quote",
  authorize: "Authorize",
  broadcast: "Broadcast",
  settled: "Settled"
};

interface PaymentStepperProps {
  stage: DemoStage;
}

export function PaymentStepper({ stage }: PaymentStepperProps) {
  if (stage === "failed" || stage === "expired") {
    return (
      <p className="payment-stepper-error" role="status">
        <CircleAlert aria-hidden="true" size={16} />
        {stage === "failed" ? "Payment failed" : "Payment expired"}
      </p>
    );
  }

  const currentStep = paymentStepFor(stage);

  return (
    <ol className="payment-stepper" aria-label="Payment progress">
      {STEP_ORDER.map((step) => (
        <li
          key={step}
          className={isReachedStep(step, currentStep) ? "is-reached" : ""}
          aria-current={step === currentStep ? "step" : undefined}
        >
          <span />
          <p>{STEP_LABELS[step]}</p>
        </li>
      ))}
    </ol>
  );
}
