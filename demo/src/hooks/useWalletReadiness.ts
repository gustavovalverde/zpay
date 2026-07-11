import { useEffect, useRef, useState } from "react";
import {
  type FaucetClaimBody,
  type ReadinessBody,
  type WalletBody,
  createFaucetClaim,
  getReadiness,
  getWallet
} from "../demo-client";
import { friendlyProblem } from "../friendly-problem";

export interface WalletReadiness {
  readiness: ReadinessBody | null;
  wallet: WalletBody | null;
  faucetClaim: FaucetClaimBody | null;
  isClaiming: boolean;
  isFaucetOpen: boolean;
  onFaucetClick: () => Promise<void>;
}

export function useWalletReadiness(
  onNotice: (message: string | null) => void,
  pausePolling: boolean
): WalletReadiness {
  const [readiness, setReadiness] = useState<ReadinessBody | null>(null);
  const [wallet, setWallet] = useState<WalletBody | null>(null);
  const [faucetClaim, setFaucetClaim] = useState<FaucetClaimBody | null>(null);
  const [isClaiming, setIsClaiming] = useState(false);
  const [isFaucetOpen, setIsFaucetOpen] = useState(false);
  const mountedRef = useRef(true);

  useEffect(() => {
    mountedRef.current = true;
    void refreshState();
    return () => {
      mountedRef.current = false;
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  useEffect(() => {
    if (pausePolling) {
      return;
    }
    const refreshTimer = window.setInterval(() => {
      refreshState();
    }, 10_000);
    return () => window.clearInterval(refreshTimer);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [pausePolling]);

  function refreshState() {
    void getReadiness()
      .then((nextReadiness) => {
        if (!mountedRef.current) {
          return;
        }
        setReadiness(nextReadiness);
        if (nextReadiness.wallet.status === "needs_funds") {
          onNotice("The demo wallet needs testnet funds");
        }
      })
      .catch((err: unknown) => {
        if (mountedRef.current) {
          onNotice(friendlyProblem(err));
        }
      })
      .finally(() => {
        void getWallet()
          .then((nextWallet) => {
            if (!mountedRef.current) {
              return;
            }
            setWallet(nextWallet);
            if (!nextWallet.is_funded) {
              onNotice("The demo wallet needs testnet funds");
            }
          })
          .catch((err: unknown) => {
            if (mountedRef.current) {
              onNotice(friendlyProblem(err));
            }
          });
      });
  }

  async function onFaucetClick() {
    setIsFaucetOpen(true);
    onNotice(null);
    setIsClaiming(true);
    try {
      const claim = await createFaucetClaim(wallet?.address);
      setFaucetClaim(claim);
    } catch (err) {
      onNotice(friendlyProblem(err));
    } finally {
      setIsClaiming(false);
    }
  }

  return { readiness, wallet, faucetClaim, isClaiming, isFaucetOpen, onFaucetClick };
}
