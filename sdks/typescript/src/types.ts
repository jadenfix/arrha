export interface ClientConfig {
  defaultGasLimit: number;
  /** Milliseconds before a request times out. Defaults to 30 000 ms. */
  requestTimeoutMs: number;
}

export const DEFAULT_CONFIG: ClientConfig = {
  defaultGasLimit: 21_000,
  requestTimeoutMs: 30_000,
};

export interface TransferRequestPayload {
  recipient: string;
  amount: bigint;
  memo?: string;
}

export interface JobRequest {
  jobId: string;
  modelHash: string;
  inputHash: string;
  maxFee: bigint;
  expiresAt: number;
  metadata?: Record<string, unknown>;
}

export interface JobSubmission {
  url: string;
  method: "POST";
  headers: Record<string, string>;
  body: JobRequest;
}

export interface SubmitResponse {
  txHash: string;
  accepted: boolean;
}

export interface AirdropResponse {
  success: true;
}

export interface RpcFeeParameters {
  a: string;
  b: string;
  c: string;
  minBaseFee: string;
  chainId: number;
}

export interface RpcBlock {
  header: {
    slot: number;
    timestamp: number;
    proposer?: unknown;
  };
  transactions: unknown[];
}

export interface RpcReceipt {
  tx_hash: unknown;
  block_hash: unknown;
  slot: number;
  status: unknown;
}

export type RpcAccountState = Record<string, unknown>;

export interface NodeHealth {
  status: "ok" | "syncing" | "error";
  version: string;
  latestSlot: number;
  finalizedSlot: number;
  peerCount: number;
  sync: {
    syncing: boolean;
    fromSlot?: number;
    targetSlot?: number;
  };
}
