import { getPublicKey, sign } from "@noble/ed25519";
import { sha256 } from "@noble/hashes/sha256";
import { bytesToHex, hexToBytes } from "@noble/hashes/utils";

const TRANSFER_PROGRAM_ID = new Uint8Array(32).fill(1);
const TRANSFER_GAS_LIMIT = 21_000;
const U64_MAX = (1n << 64n) - 1n;
const U128_MAX = (1n << 128n) - 1n;

export interface Keypair {
  readonly secretKey: Uint8Array;
  readonly publicKey: Uint8Array;
  readonly address: Uint8Array;
}

export async function keypairFromSeed(seed: Uint8Array): Promise<Keypair> {
  if (seed.length !== 32) throw new Error("Ed25519 seed must be 32 bytes");
  const secretKey = new Uint8Array(seed);
  const publicKey = await getPublicKey(secretKey);
  const address = sha256(publicKey).slice(0, 20);
  return { secretKey, publicKey, address };
}

export function randomSeed(): Uint8Array {
  const seed = new Uint8Array(32);
  globalThis.crypto.getRandomValues(seed);
  return seed;
}

export function devnetSeed(index: number): Uint8Array {
  if (!Number.isInteger(index) || index < 0 || index > 0xffff_ffff) {
    throw new Error("devnet account index must fit in u32");
  }
  const indexBytes = new Uint8Array(4);
  new DataView(indexBytes.buffer).setUint32(0, index, true);
  const label = new TextEncoder().encode("aether-devnet-account");
  return sha256(concat(label, indexBytes));
}

export interface TransferOptions {
  chainId: bigint;
  nonce: bigint;
  sender: Uint8Array;
  publicKey: Uint8Array;
  recipient: Uint8Array;
  amount: bigint;
  fee: bigint;
  gasLimit?: bigint;
  memo?: string;
}

export interface FeeParameters {
  a: string;
  b: string;
  c: string;
  minBaseFee: string;
  chainId: number;
}

export function calculateTransferFee(options: Omit<TransferOptions, "fee">, parameters: FeeParameters): bigint {
  const gasLimit = options.gasLimit ?? BigInt(TRANSFER_GAS_LIMIT);
  validateTransfer({ ...options, fee: 0n }, gasLimit, new Uint8Array(32));
  const encoded = encodeTransaction({
    nonce: options.nonce,
    chainId: options.chainId,
    sender: options.sender,
    publicKey: options.publicKey,
    reads: [options.sender],
    writes: equalBytes(options.sender, options.recipient)
      ? [options.sender]
      : [options.sender, options.recipient],
    programId: TRANSFER_PROGRAM_ID,
    data: encodeTransferPayload(options.recipient, options.amount, options.memo),
    gasLimit,
    fee: 0n,
    // Rust's fee checker serializes the complete transaction, including the
    // fixed 64-byte Ed25519 signature that will be attached after fee choice.
    signature: new Uint8Array(64),
  });
  const a = parseU128(parameters.a, "fee parameter a");
  const b = parseU128(parameters.b, "fee parameter b");
  const c = parseU128(parameters.c, "fee parameter c");
  // `minBaseFee` is exposed for monitoring. Rust transfer finalization currently
  // prices transactions from a + b * serialized_size + c * gas_limit.
  parseU128(parameters.minBaseFee, "minimum base fee");
  if (!Number.isSafeInteger(parameters.chainId) || parameters.chainId <= 0 || BigInt(parameters.chainId) !== options.chainId) {
    throw new Error("fee parameters belong to a different or invalid chain");
  }
  const byteFee = checkedMultiply(b, BigInt(encoded.length), "byte fee");
  const gasFee = checkedMultiply(c, gasLimit, "gas fee");
  const calculated = checkedAdd(checkedAdd(a, byteFee, "base plus byte fee"), gasFee, "minimum fee");
  const recommended = checkedMultiply(calculated, 2n, "recommended fee");
  if (recommended > U128_MAX) throw new Error("recommended fee exceeds u128");
  return recommended;
}

/** A signed Aether transfer encoded using the Rust Transaction bincode layout. */
export class Transaction {
  readonly hash: string;
  readonly signature: Uint8Array;
  readonly rawBytes: Uint8Array;

  private constructor(hash: Uint8Array, signature: Uint8Array, rawBytes: Uint8Array) {
    this.hash = `0x${bytesToHex(hash)}`;
    this.signature = signature;
    this.rawBytes = rawBytes;
  }

  static async signTransfer(options: TransferOptions, secretKey: Uint8Array): Promise<Transaction> {
    const gasLimit = options.gasLimit ?? BigInt(TRANSFER_GAS_LIMIT);
    validateTransfer(options, gasLimit, secretKey);
    if (!equalBytes(options.publicKey, await getPublicKey(secretKey))) {
      throw new Error("public key does not match secret key");
    }

    const payload = encodeTransferPayload(options.recipient, options.amount, options.memo);
    const reads = [options.sender];
    const writes = equalBytes(options.sender, options.recipient)
      ? [options.sender]
      : [options.sender, options.recipient];
    const canonical = encodeTransaction({
      nonce: options.nonce,
      chainId: options.chainId,
      sender: options.sender,
      publicKey: options.publicKey,
      reads,
      writes,
      programId: TRANSFER_PROGRAM_ID,
      data: payload,
      gasLimit,
      fee: options.fee,
      signature: new Uint8Array(),
    });
    const hash = sha256(canonical);
    const signature = await sign(hash, secretKey);
    const rawBytes = encodeTransaction({
      nonce: options.nonce,
      chainId: options.chainId,
      sender: options.sender,
      publicKey: options.publicKey,
      reads,
      writes,
      programId: TRANSFER_PROGRAM_ID,
      data: payload,
      gasLimit,
      fee: options.fee,
      signature,
    });
    return new Transaction(hash, signature, rawBytes);
  }

  toRawHex(): string {
    return `0x${bytesToHex(this.rawBytes)}`;
  }
}

/** Rust `bincode::serialize(&TransferPayload { ... })` encoding. */
export function encodeTransferPayload(
  recipient: Uint8Array,
  amount: bigint,
  memo?: string,
): Uint8Array {
  assertLength(recipient, 20, "recipient");
  const encodedMemo = memo === undefined
    ? new Uint8Array([0])
    : concat(new Uint8Array([1]), encodeString(memo));
  return concat(recipient, u128(amount), encodedMemo);
}

interface TransactionEncoding {
  nonce: bigint;
  chainId: bigint;
  sender: Uint8Array;
  publicKey: Uint8Array;
  reads: Uint8Array[];
  writes: Uint8Array[];
  programId: Uint8Array;
  data: Uint8Array;
  gasLimit: bigint;
  fee: bigint;
  signature: Uint8Array;
}

/** Rust `Transaction::hash` canonical struct, including its empty signature. */
function encodeTransaction(tx: TransactionEncoding): Uint8Array {
  assertLength(tx.sender, 20, "sender");
  assertLength(tx.publicKey, 32, "public key");
  assertLength(tx.programId, 32, "program id");
  const reads = sortAddresses(tx.reads);
  const writes = sortAddresses(tx.writes);
  return concat(
    u64(tx.nonce),
    u64(tx.chainId),
    tx.sender,
    bytes(tx.publicKey),
    u64(0n), // inputs: Vec<UtxoId>
    u64(0n), // outputs: Vec<UtxoOutput>
    addressSet(reads),
    addressSet(writes),
    new Uint8Array([1]), // Some(program_id): Option uses a u8 tag in bincode 1
    tx.programId,
    bytes(tx.data),
    u64(tx.gasLimit),
    u128(tx.fee),
    bytes(tx.signature),
  );
}

function addressSet(addresses: Uint8Array[]): Uint8Array {
  return concat(u64(BigInt(addresses.length)), ...addresses);
}

function sortAddresses(addresses: Uint8Array[]): Uint8Array[] {
  return addresses.map((a) => {
    assertLength(a, 20, "access-list address");
    return a;
  }).sort(compareBytes);
}

function compareBytes(a: Uint8Array, b: Uint8Array): number {
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return a[i] - b[i];
  }
  return 0;
}

function validateTransfer(options: TransferOptions, gasLimit: bigint, secretKey: Uint8Array): void {
  assertLength(secretKey, 32, "secret key");
  assertLength(options.sender, 20, "sender");
  assertLength(options.publicKey, 32, "public key");
  assertLength(options.recipient, 20, "recipient");
  if (!equalBytes(sha256(options.publicKey).slice(0, 20), options.sender)) {
    throw new Error("sender address does not match public key");
  }
  assertUnsigned(options.nonce, U64_MAX, "nonce");
  assertUnsigned(options.chainId, U64_MAX, "chain id");
  assertUnsigned(options.amount, U128_MAX, "amount");
  if (options.amount === 0n) throw new Error("amount must be positive");
  assertUnsigned(options.fee, U128_MAX, "fee");
  assertUnsigned(gasLimit, U64_MAX, "gas limit");
  if (gasLimit === 0n) throw new Error("gas limit must be positive");
}

function assertUnsigned(value: bigint, max: bigint, name: string): void {
  if (value < 0n || value > max) throw new Error(`${name} is outside its unsigned range`);
}

function parseU128(value: string, name: string): bigint {
  if (!/^(0|[1-9][0-9]*)$/.test(value)) throw new Error(`${name} must be a decimal integer string`);
  const parsed = BigInt(value);
  assertUnsigned(parsed, U128_MAX, name);
  return parsed;
}

function checkedAdd(a: bigint, b: bigint, name: string): bigint {
  const result = a + b;
  if (result > U128_MAX) throw new Error(`${name} overflow`);
  return result;
}

function checkedMultiply(a: bigint, b: bigint, name: string): bigint {
  const result = a * b;
  if (result > U128_MAX) throw new Error(`${name} overflow`);
  return result;
}

function assertLength(bytesValue: Uint8Array, length: number, name: string): void {
  if (bytesValue.length !== length) throw new Error(`${name} must be exactly ${length} bytes`);
}

function u64(value: bigint): Uint8Array {
  assertUnsigned(value, U64_MAX, "u64");
  return writeLittleEndian(value, 8);
}

function u128(value: bigint): Uint8Array {
  assertUnsigned(value, U128_MAX, "u128");
  return writeLittleEndian(value, 16);
}

function writeLittleEndian(value: bigint, size: number): Uint8Array {
  const output = new Uint8Array(size);
  let remaining = value;
  for (let index = 0; index < size; index++) {
    output[index] = Number(remaining & 0xffn);
    remaining >>= 8n;
  }
  return output;
}

function bytes(value: Uint8Array): Uint8Array {
  return concat(u64(BigInt(value.length)), value);
}

function encodeString(value: string): Uint8Array {
  return bytes(new TextEncoder().encode(value));
}

function concat(...chunks: Uint8Array[]): Uint8Array {
  const length = chunks.reduce((total, chunk) => total + chunk.length, 0);
  const result = new Uint8Array(length);
  let offset = 0;
  for (const chunk of chunks) {
    result.set(chunk, offset);
    offset += chunk.length;
  }
  return result;
}

function equalBytes(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  return a.every((byte, index) => byte === b[index]);
}

export function hexToBytesStrict(value: string, length?: number): Uint8Array {
  const normalized = value.startsWith("0x") ? value.slice(2) : value;
  if (!/^(?:[0-9a-fA-F]{2})*$/.test(normalized)) throw new Error("value must be even-length hex");
  const result = hexToBytes(normalized);
  if (length !== undefined) assertLength(result, length, "hex value");
  return result;
}

export function bytesToHex0x(value: Uint8Array): string {
  return `0x${bytesToHex(value)}`;
}
