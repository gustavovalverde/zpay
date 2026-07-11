import { useEffect, useState } from "react";
import { type ConsolePaymentsBody, getConsolePayments } from "../demo-client";
import { friendlyProblem } from "../friendly-problem";

export interface ConsolePayments {
  data: ConsolePaymentsBody | null;
  isLoading: boolean;
  error: string | null;
  refresh: () => Promise<void>;
}

export function useConsolePayments(): ConsolePayments {
  const [data, setData] = useState<ConsolePaymentsBody | null>(null);
  const [isLoading, setIsLoading] = useState(true);
  const [error, setError] = useState<string | null>(null);

  async function refresh() {
    setIsLoading(true);
    setError(null);
    try {
      const next = await getConsolePayments();
      setData(next);
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

  return { data, isLoading, error, refresh };
}
