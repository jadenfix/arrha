import { JobBuilder, TransferBuilder } from "./builders.js";
import { Transaction } from "./transaction.js";
import {
  ClientConfig,
  DEFAULT_CONFIG,
  AirdropResponse,
  JobRequest,
  JobSubmission,
  NodeHealth,
  RpcAccountState,
  RpcBlock,
  RpcReceipt,
  SubmitResponse,
} from "./types.js";
import type { FeeParameters } from "./transaction.js";

function normalizeEndpoint(endpoint: string): string {
  return endpoint.replace(/\/+$/, "");
}

export class AetherClient {
  private readonly endpoint: string;
  private requestId = 1;
  constructor(
    endpoint: string,
    private readonly config: ClientConfig = DEFAULT_CONFIG,
  ) {
    if (!endpoint) {
      throw new Error("endpoint must be provided");
    }
    this.endpoint = normalizeEndpoint(endpoint);
  }

  static withConfig(endpoint: string, config: ClientConfig): AetherClient {
    return new AetherClient(endpoint, config);
  }

  getEndpoint(): string {
    return this.endpoint;
  }

  getConfig(): ClientConfig {
    return this.config;
  }

  transfer(): TransferBuilder {
    return new TransferBuilder(this.config);
  }

  job(): JobBuilder {
    return new JobBuilder(this.endpoint);
  }

  async submit(transaction: Transaction): Promise<SubmitResponse> {
    const txHash = await this.rpcCall<string>("aeth_sendRawTransaction", [transaction.toRawHex()]);
    if (txHash.toLowerCase() !== transaction.hash.toLowerCase()) {
      throw new Error(`node returned transaction hash ${txHash}, expected ${transaction.hash}`);
    }
    return {
      txHash,
      accepted: true,
    };
  }

  async getChainId(): Promise<bigint> {
    const value = await this.rpcCall<string>("aeth_chainId", []);
    if (!/^0x[0-9a-f]+$/i.test(value)) throw new Error("aeth_chainId returned invalid hex");
    return BigInt(value);
  }

  async getFeeParameters(): Promise<FeeParameters> {
    const value = await this.rpcCall<FeeParameters>("aeth_getFeeParameters", []);
    for (const key of ["a", "b", "c", "minBaseFee"] as const) {
      if (typeof value?.[key] !== "string" || !/^(0|[1-9][0-9]*)$/.test(value[key])) {
        throw new Error(`aeth_getFeeParameters returned invalid ${key}`);
      }
    }
    return value;
  }

  async getPendingAccount(address: string): Promise<RpcAccountState | null> {
    return this.rpcCall<RpcAccountState | null>("aeth_getPendingAccount", [address]);
  }

  async requestAirdrop(address: string, amount: bigint): Promise<AirdropResponse> {
    const response = await this.rpcCall<unknown>("aeth_requestAirdrop", [address, amount.toString()]);
    if (!response || typeof response !== "object" || (response as Record<string, unknown>).success !== true) {
      throw new Error("aeth_requestAirdrop returned an invalid acknowledgement");
    }
    return { success: true };
  }

  async getTransactionStatus(txHash: string): Promise<unknown> {
    return this.rpcCall<unknown>("aeth_getTransactionStatus", [txHash]);
  }

  async waitForFinality(txHash: string, timeoutMs = 60_000, pollMs = 500): Promise<RpcReceipt> {
    const deadline = Date.now() + timeoutMs;
    while (Date.now() < deadline) {
      const status = await this.getTransactionStatus(txHash);
      if (isFinalizedStatus(status)) {
        const receipt = await this.getTransactionReceipt(txHash);
        if (!receipt) throw new Error(`finalized transaction ${txHash} has no receipt`);
        if (isFailedStatus(status)) throw new Error(`transaction ${txHash} finalized with failure`);
        return receipt;
      }
      await new Promise((resolve) => setTimeout(resolve, pollMs));
    }
    throw new Error(`transaction ${txHash} did not finalize within ${timeoutMs} ms`);
  }

  async getSlotNumber(): Promise<number> {
    return this.rpcCall<number>("aeth_getSlotNumber", []);
  }

  async getFinalizedSlot(): Promise<number> {
    return this.rpcCall<number>("aeth_getFinalizedSlot", []);
  }

  async getBlockByNumber(
    blockRef: number | "latest" = "latest",
    fullTx = true,
  ): Promise<RpcBlock | null> {
    return this.rpcCall<RpcBlock | null>("aeth_getBlockByNumber", [
      blockRef.toString(),
      fullTx,
    ]);
  }

  async getTransactionReceipt(txHash: string): Promise<RpcReceipt | null> {
    return this.rpcCall<RpcReceipt | null>("aeth_getTransactionReceipt", [txHash]);
  }

  async getAccount(
    address: string,
    blockRef?: string,
  ): Promise<RpcAccountState | null> {
    const params = blockRef ? [address, blockRef] : [address];
    return this.rpcCall<RpcAccountState | null>("aeth_getAccount", params);
  }

  async getBlockByHash(blockHash: string, fullTx = true): Promise<RpcBlock | null> {
    return this.rpcCall<RpcBlock | null>("aeth_getBlockByHash", [blockHash, fullTx]);
  }

  async getStateRoot(blockRef?: string): Promise<string> {
    const params = blockRef ? [blockRef] : [];
    return this.rpcCall<string>("aeth_getStateRoot", params);
  }

  /**
   * Fetch node health from the HTTP `/health` endpoint (not a JSON-RPC call).
   * Returns sync status, peer count, and latest/finalized slot numbers.
   */
  async getHealth(): Promise<NodeHealth> {
    const response = await fetch(`${this.endpoint}/health`, {
      signal: AbortSignal.timeout(this.config.requestTimeoutMs),
    });
    if (!response.ok) {
      throw new Error(`health check failed with status ${response.status}`);
    }
    return response.json() as Promise<NodeHealth>;
  }

  prepareJobSubmission(job: JobRequest): JobSubmission {
    return {
      url: `${this.endpoint}/v1/jobs`,
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: job,
    };
  }

  private async rpcCall<T>(method: string, params: unknown[]): Promise<T> {
    const response = await fetch(this.endpoint, {
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      signal: AbortSignal.timeout(this.config.requestTimeoutMs),
      body: JSON.stringify({
        jsonrpc: "2.0",
        method,
        params,
        id: this.requestId++,
      }),
    });

    if (!response.ok) {
      throw new Error(`rpc request failed with status ${response.status}`);
    }

    const payload = (await response.json()) as {
      result?: T;
      error?: { code: number; message: string };
    };
    if (payload.error) {
      throw new Error(`rpc error ${payload.error.code}: ${payload.error.message}`);
    }
    if (!("result" in payload)) {
      throw new Error("rpc response missing result");
    }
    return payload.result as T;
  }
}

function isFinalizedStatus(status: unknown): boolean {
  if (typeof status === "string") return status.toLowerCase() === "finalized";
  if (status && typeof status === "object") {
    const value = status as Record<string, unknown>;
    return value.status === "finalized";
  }
  return false;
}

function isFailedStatus(status: unknown): boolean {
  return Boolean(status && typeof status === "object" && (status as Record<string, unknown>).success === false);
}
