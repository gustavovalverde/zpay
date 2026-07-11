import { useEffect, useState } from "react";
import { type PaymentBody, listPayments } from "../demo-client";
import { friendlyProblem } from "../friendly-problem";

export interface PaymentHistory {
  payments: PaymentBody[];
  isLoading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
}

export function usePaymentHistory(): PaymentHistory {
  const [payments, setPayments] = useState<PaymentBody[]>([]);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  async function refresh() {
    setIsLoading(true);
    setError(null);
    try {
      const nextPayments = await listPayments();
      setPayments(nextPayments);
    } catch (err) {
      setError(friendlyProblem(err));
    } finally {
      setIsLoading(false);
    }
  }

  useEffect(() => {
    void refresh();
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return { payments, isLoading, error, refresh };
}
