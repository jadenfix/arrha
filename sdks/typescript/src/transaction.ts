import { sha256 } from "@noble/hashes/sha256";
import { bytesToHex } from "@noble/hashes/utils";

import type { TransactionFields } from "./types.js";

function normalizeSignatureHex(signature: string): string {
  const normalized = signature.startsWith("0x") ? signature.slice(2) : signature;
  if (normalized.length !== 128) {
    throw new Error("signature must be exactly 64 bytes (128 hex characters)");
  }
  if (!/^[0-9a-fA-F]+$/.test(normalized)) {
    throw new Error("signature must be valid hex");
  }
  return `0x${normalized.toLowerCase()}`;
}

export class Transaction {
  readonly nonce: number;
  readonly sender: string;
  readonly senderPublicKey: string;
  readonly recipient: string;
  readonly amount: bigint;
  readonly fee: bigint;
  readonly gasLimit: number;
  readonly memo?: string;
  readonly signature: string;
  readonly reads: string[];
  readonly writes: string[];

  constructor(fields: TransactionFields) {
    this.nonce = fields.nonce;
    this.sender = fields.sender;
    this.senderPublicKey = fields.senderPublicKey;
    this.recipient = fields.recipient;
    this.amount = fields.amount;
    this.fee = fields.fee;
    this.gasLimit = fields.gasLimit;
    this.memo = fields.memo;
    this.signature = normalizeSignatureHex(fields.signature);
    this.reads = [fields.sender];
    this.writes =
      fields.sender === fields.recipient
        ? [fields.sender]
        : [fields.sender, fields.recipient];
  }

  hash(): string {
    const forHash = {
      nonce: this.nonce,
      sender: this.sender,
      senderPublicKey: this.senderPublicKey,
      recipient: this.recipient,
      amount: this.amount.toString(),
      fee: this.fee.toString(),
      gasLimit: this.gasLimit,
      memo: this.memo,
      reads: this.reads,
      writes: this.writes,
    };
    // Sort keys for deterministic serialization (matches Python SDK's sort_keys=True)
    const sortedKeys = Object.keys(forHash).sort();
    const sorted: Record<string, unknown> = {};
    for (const key of sortedKeys) {
      sorted[key] = (forHash as Record<string, unknown>)[key];
    }
    const hash = bytesToHex(sha256(new TextEncoder().encode(JSON.stringify(sorted))));
    return `0x${hash}`;
  }

  toJSON() {
    return {
      nonce: this.nonce,
      sender: this.sender,
      senderPublicKey: this.senderPublicKey,
      recipient: this.recipient,
      amount: this.amount.toString(),
      fee: this.fee.toString(),
      gasLimit: this.gasLimit,
      memo: this.memo,
      signature: this.signature,
      reads: this.reads,
      writes: this.writes,
    };
  }

  toRpcTransaction() {
    return {
      nonce: this.nonce,
      sender: this.sender,
      sender_public_key: this.senderPublicKey,
      recipient: this.recipient,
      amount: this.amount.toString(),
      fee: this.fee.toString(),
      gas_limit: this.gasLimit,
      memo: this.memo,
      reads: this.reads,
      writes: this.writes,
      signature: this.signature,
    };
  }
}
