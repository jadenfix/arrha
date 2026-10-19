import { ClientConfig, DEFAULT_CONFIG, JobRequest, JobSubmission } from "./types.js";
import {
  calculateTransferFee,
  Keypair,
  Transaction,
  type FeeParameters,
} from "./transaction.js";

export class TransferBuilder {
  private _recipient?: string;
  private _amount?: bigint;
  private _memo?: string;
  private _fee?: bigint;
  private _gasLimit: number;

  constructor(private readonly config: ClientConfig = DEFAULT_CONFIG) {
    this._gasLimit = config.defaultGasLimit;
  }

  to(recipient: string): TransferBuilder {
    if (!recipient.startsWith("0x")) {
      throw new Error("recipient must be a hex string");
    }
    this._recipient = recipient;
    return this;
  }

  amount(amount: bigint | number | string): TransferBuilder {
    const value =
      typeof amount === "bigint" ? amount : BigInt(amount.toString());
    if (value <= 0n) {
      throw new Error("amount must be positive");
    }
    this._amount = value;
    return this;
  }

  memo(memo: string): TransferBuilder {
    this._memo = memo;
    return this;
  }

  fee(fee: bigint | number | string): TransferBuilder {
    const value =
      typeof fee === "bigint" ? fee : BigInt(fee.toString());
    if (value <= 0n) {
      throw new Error("fee must be positive");
    }
    this._fee = value;
    return this;
  }

  gasLimit(gasLimit: number): TransferBuilder {
    if (gasLimit <= 0) {
      throw new Error("gasLimit must be positive");
    }
    this._gasLimit = gasLimit;
    return this;
  }

  async build(options: {
    keypair: Keypair;
    chainId: bigint;
    nonce: bigint;
    feeParameters?: FeeParameters;
    fee?: bigint;
  }): Promise<Transaction> {
    if (!this._recipient) {
      throw new Error("recipient not set");
    }
    if (!this._amount) {
      throw new Error("amount not set");
    }
    const recipient = fromHexAddress(this._recipient);
    const transfer = {
      chainId: options.chainId,
      nonce: options.nonce,
      sender: options.keypair.address,
      publicKey: options.keypair.publicKey,
      recipient,
      amount: this._amount,
      gasLimit: BigInt(this._gasLimit),
      memo: this._memo,
    };
    const fee = options.fee ?? this._fee ?? (options.feeParameters
      ? calculateTransferFee(transfer, options.feeParameters)
      : undefined);
    if (fee === undefined) {
      throw new Error("fee or live RPC fee parameters are required");
    }
    return Transaction.signTransfer({ ...transfer, fee }, options.keypair.secretKey);
  }
}

function fromHexAddress(value: string): Uint8Array {
  const normalized = value.startsWith("0x") ? value.slice(2) : value;
  if (!/^[0-9a-fA-F]{40}$/.test(normalized)) {
    throw new Error("recipient must be a 20-byte 0x-prefixed hex address");
  }
  return Uint8Array.from(normalized.match(/.{2}/g)!, (byte) => Number.parseInt(byte, 16));
}

export class JobBuilder {
  private _jobId?: string;
  private _modelHash?: string;
  private _inputHash?: string;
  private _maxFee: bigint;
  private _expiresAt?: number;
  private _metadata?: Record<string, unknown>;

  constructor(private readonly endpoint: string) {
    this._maxFee = 1_000_000n;
  }

  id(jobId: string): JobBuilder {
    if (!jobId.trim()) {
      throw new Error("jobId must not be empty");
    }
    this._jobId = jobId;
    return this;
  }

  model(hash: string): JobBuilder {
    if (!hash.startsWith("0x")) {
      throw new Error("model hash must be hex");
    }
    this._modelHash = hash;
    return this;
  }

  input(hash: string): JobBuilder {
    if (!hash.startsWith("0x")) {
      throw new Error("input hash must be hex");
    }
    this._inputHash = hash;
    return this;
  }

  maxFee(fee: bigint | number | string): JobBuilder {
    const value =
      typeof fee === "bigint" ? fee : BigInt(fee.toString());
    if (value <= 0n) {
      throw new Error("max fee must be positive");
    }
    this._maxFee = value;
    return this;
  }

  expiresAt(timestamp: number | Date): JobBuilder {
    const value =
      timestamp instanceof Date ? Math.floor(timestamp.getTime() / 1000) : timestamp;
    if (value <= 0) {
      throw new Error("expiry must be in the future");
    }
    this._expiresAt = value;
    return this;
  }

  withMetadata(metadata: Record<string, unknown>): JobBuilder {
    this._metadata = metadata;
    return this;
  }

  build(): JobRequest {
    if (!this._jobId) {
      throw new Error("jobId not set");
    }
    if (!this._modelHash) {
      throw new Error("model hash not set");
    }
    if (!this._inputHash) {
      throw new Error("input hash not set");
    }
    if (!this._expiresAt) {
      throw new Error("expiry not set");
    }

    return {
      jobId: this._jobId,
      modelHash: this._modelHash,
      inputHash: this._inputHash,
      maxFee: this._maxFee,
      expiresAt: this._expiresAt,
      metadata: this._metadata,
    };
  }

  toSubmission(): JobSubmission {
    const job = this.build();
    return {
      url: `${this.endpoint.replace(/\/+$/, "")}/v1/jobs`,
      method: "POST",
      headers: {
        "content-type": "application/json",
      },
      body: job,
    };
  }
}
