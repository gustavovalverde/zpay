import { useEffect, useRef, useState } from "react";
import {
  type DemoStage,
  type PaymentBody,
  type PaymentMode,
  createPayment,
  paymentEventsUrl,
  settlePayment
} from "../demo-client";
import { friendlyProblem } from "../friendly-problem";

export interface PaymentSession {
  payment: PaymentBody | null;
  isPreparing: boolean;
  isSettling: boolean;
  stageObservedAtMs: Partial<Record<DemoStage, number>>;
  prepareCheckout: () => Promise<void>;
  settleCheckout: () => Promise<void>;
  resetPayment: () => void;
}

export function usePaymentSession(mode: PaymentMode, onNotice: (message: string | null) => void): PaymentSession {
  const [payment, setPayment] = useState<PaymentBody | null>(null);
  const [isPreparing, setIsPreparing] = useState(false);
  const [isSettling, setIsSettling] = useState(false);
  const [stageObservedAtMs, setStageObservedAtMs] = useState<Partial<Record<DemoStage, number>>>({});
  const stageObservationRef = useRef<{ paymentId: string | null; stamps: Partial<Record<DemoStage, number>> }>({
    paymentId: null,
    stamps: {}
  });

  useEffect(() => {
    if (!payment?.payment_id) {
      return;
    }
    const source = new EventSource(paymentEventsUrl(payment.payment_id));
    source.addEventListener("snapshot", (event) => {
      const nextPayment = JSON.parse((event as MessageEvent<string>).data) as PaymentBody;
      setPayment(nextPayment);
    });
    source.onerror = () => {
      source.close();
    };
    return () => source.close();
  }, [payment?.payment_id]);

  useEffect(() => {
    if (!payment) {
      return;
    }
    if (stageObservationRef.current.paymentId !== payment.payment_id) {
      stageObservationRef.current = { paymentId: payment.payment_id, stamps: {} };
    }
    if (stageObservationRef.current.stamps[payment.stage] === undefined) {
      stageObservationRef.current.stamps[payment.stage] = Date.now();
      setStageObservedAtMs({ ...stageObservationRef.current.stamps });
    }
  }, [payment?.payment_id, payment?.stage]);

  async function prepareCheckout() {
    onNotice(null);
    setPayment(null);
    setIsPreparing(true);
    try {
      const nextPayment = await createPayment(mode);
      setPayment(nextPayment);
    } catch (err) {
      onNotice(friendlyProblem(err));
    } finally {
      setIsPreparing(false);
    }
  }

  async function settleCheckout() {
    if (!payment) {
      return;
    }
    onNotice(null);
    setIsSettling(true);
    try {
      const nextPayment = await settlePayment(payment.payment_id);
      setPayment(nextPayment);
    } catch (err) {
      onNotice(friendlyProblem(err));
    } finally {
      setIsSettling(false);
    }
  }

  function resetPayment() {
    setPayment(null);
  }

  return { payment, isPreparing, isSettling, stageObservedAtMs, prepareCheckout, settleCheckout, resetPayment };
}
