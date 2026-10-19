import { useEffect, useState } from "react";

import { AetherClient } from "@aether/sdk";
import type { RpcBlock } from "@aether/sdk";

const DEFAULT_RPC_ENDPOINT = "http://127.0.0.1:8545";

export interface ExplorerSnapshot {
  endpoint: string;
  currentSlot: number | null;
  finalizedSlot: number | null;
  latestBlock: RpcBlock | null;
  error: string | null;
}

export function useExplorerData() {
  const endpoint =
    (globalThis as { __AETHER_RPC_ENDPOINT__?: string }).__AETHER_RPC_ENDPOINT__ ??
    DEFAULT_RPC_ENDPOINT;
  const [snapshot, setSnapshot] = useState<ExplorerSnapshot>({
    endpoint,
    currentSlot: null,
    finalizedSlot: null,
    latestBlock: null,
    error: null,
  });

  useEffect(() => {
    const client = new AetherClient(endpoint);
    let cancelled = false;
    const refresh = async () => {
      try {
        const [currentSlot, finalizedSlot, latestBlock] = await Promise.all([
          client.getSlotNumber(),
          client.getFinalizedSlot(),
          client.getBlockByNumber("latest", true),
        ]);
        if (!cancelled) {
          setSnapshot({ endpoint, currentSlot, finalizedSlot, latestBlock, error: null });
        }
      } catch (error) {
        if (!cancelled) {
          setSnapshot((current) => ({
            ...current,
            error: error instanceof Error ? error.message : String(error),
          }));
        }
      }
    };

    void refresh();
    const timer = setInterval(() => void refresh(), 5_000);
    return () => {
      cancelled = true;
      clearInterval(timer);
    };
  }, [endpoint]);

  return snapshot;
}
