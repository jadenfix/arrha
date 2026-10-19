#!/usr/bin/env node
import { execFileSync } from "node:child_process";
import { bytesToHex0x, keypairFromSeed, randomSeed, AetherClient } from "../sdks/typescript/dist/src/index.js";

const rpcFrom = process.env.AETHER_RPC_FROM ?? "http://127.0.0.1:8545";
const rpcRead = process.env.AETHER_RPC_READ ?? "http://127.0.0.1:8547";
const ctl = process.env.AETHERCTL_BIN ?? "target/release/aetherctl";
const fundingAmount = BigInt(process.env.AETHER_E2E_FUNDING ?? "1000000000");
const transferAmount = BigInt(process.env.AETHER_E2E_AMOUNT ?? "12345");
const from = new AetherClient(rpcFrom);
const read = new AetherClient(rpcRead);

if (transferAmount <= 0n || fundingAmount <= transferAmount) {
  throw new Error("funding amount must be greater than the positive transfer amount");
}

const sender = await keypairFromSeed(randomSeed());
const recipient = await keypairFromSeed(randomSeed());
const senderAddress = bytesToHex0x(sender.address);
const recipientAddress = bytesToHex0x(recipient.address);

console.log(`Funding temporary TypeScript account ${senderAddress} through the Rust CLI`);
execFileSync(ctl, [
  "chain", "--rpc", rpcFrom, "transfer", "--dev", "0",
  "--to", senderAddress, "--amount", String(fundingAmount), "--wait",
], { stdio: "inherit" });

const before = await read.getAccount(recipientAddress);
const beforeBalance = readBalance(before);
const [chainId, pending, fees] = await Promise.all([
  from.getChainId(),
  from.getPendingAccount(senderAddress),
  from.getFeeParameters(),
]);
if (fees.chainId !== Number(chainId)) throw new Error("fee parameters chain id does not match RPC chain id");
const nonce = readNonce(pending);
const transaction = await from.transfer()
  .to(recipientAddress)
  .amount(transferAmount)
  .memo("typescript rust interoperability e2e")
  .build({ keypair: sender, chainId, nonce, feeParameters: fees });

const submitted = await from.submit(transaction);
if (submitted.txHash.toLowerCase() !== transaction.hash.toLowerCase()) {
  throw new Error(`submitted hash mismatch: expected ${transaction.hash}, got ${submitted.txHash}`);
}
const receipt = await from.waitForFinality(submitted.txHash, Number(process.env.AETHER_E2E_TIMEOUT_MS ?? 90_000));
const status = await from.getTransactionStatus(submitted.txHash);
if (!status || typeof status !== "object" || (status).status !== "finalized" || (status).success !== true) {
  throw new Error(`transaction did not finalize successfully: ${JSON.stringify(status)}`);
}
if (!receipt) throw new Error("finalized transaction has no receipt");

const after = await read.getAccount(recipientAddress);
const afterBalance = readBalance(after);
if (afterBalance - beforeBalance !== transferAmount) {
  throw new Error(`cross-validator balance mismatch: expected delta ${transferAmount}, got ${afterBalance - beforeBalance}`);
}

console.log(JSON.stringify({
  result: "PASS",
  sender: senderAddress,
  recipient: recipientAddress,
  chainId: String(chainId),
  nonce: String(nonce),
  transactionHash: submitted.txHash,
  receiptSlot: receipt.slot,
  senderRpc: rpcFrom,
  balanceReadRpc: rpcRead,
  recipientBalanceBefore: String(beforeBalance),
  recipientBalanceAfter: String(afterBalance),
  transferAmount: String(transferAmount),
}, null, 2));

function readBalance(account) {
  if (!account || account.balance == null) return 0n;
  const value = account.balance;
  if (typeof value === "string" && /^(0|[1-9][0-9]*)$/.test(value)) return BigInt(value);
  if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return BigInt(value);
  throw new Error(`account returned an unsupported balance representation: ${JSON.stringify(value)}`);
}

function readNonce(account) {
  if (!account || account.nonce == null) return 0n;
  const value = account.nonce;
  if (typeof value === "string" && /^(0|[1-9][0-9]*)$/.test(value)) return BigInt(value);
  if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return BigInt(value);
  throw new Error(`pending account returned an unsupported nonce: ${JSON.stringify(value)}`);
}
