import React, { useEffect, useState } from "react";

import { AetherClient, bytesToHex0x, hexToBytesStrict, keypairFromSeed, randomSeed } from "@aether/sdk";
import type { Keypair } from "@aether/sdk";
import { Card, Metric, Section } from "@aether/ui";

const RPC_ENDPOINT =
  (globalThis as { __AETHER_RPC_ENDPOINT__?: string }).__AETHER_RPC_ENDPOINT__ ??
  "http://127.0.0.1:8545";
const client = new AetherClient(RPC_ENDPOINT);

export function App() {
  const [keypair, setKeypair] = useState<Keypair | null>(null);
  const [seedInput, setSeedInput] = useState("");
  const [recipient, setRecipient] = useState("");
  const [amount, setAmount] = useState("1000");
  const [memo, setMemo] = useState("");
  const [networkStatus, setNetworkStatus] = useState("Checking RPC…");
  const [currentSlot, setCurrentSlot] = useState<number | null>(null);
  const [balance, setBalance] = useState<string | null>(null);
  const [txHash, setTxHash] = useState<string | null>(null);
  const [txStatus, setTxStatus] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);

  const sender = keypair ? bytesToHex0x(keypair.address) : null;

  useEffect(() => {
    let cancelled = false;
    void client.getSlotNumber()
      .then((slot) => {
        if (cancelled) return;
        setCurrentSlot(slot);
        setNetworkStatus("Connected to local/devnet RPC");
      })
      .catch((reason: unknown) => {
        if (cancelled) return;
        setNetworkStatus("RPC unavailable");
        setError(reason instanceof Error ? reason.message : String(reason));
      });
    return () => { cancelled = true; };
  }, []);

  async function installSeed(seed: Uint8Array) {
    const generated = await keypairFromSeed(seed);
    setKeypair(generated);
    setSeedInput(bytesToHex0x(generated.secretKey));
    setTxHash(null);
    setTxStatus(null);
    setError(null);
    await refreshBalance(generated.address);
  }

  async function refreshBalance(addressBytes: Uint8Array) {
    const account = await client.getAccount(bytesToHex0x(addressBytes));
    setBalance(readBalance(account));
  }

  async function createSessionKey() {
    setBusy(true);
    setError(null);
    try {
      await installSeed(randomSeed());
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  }

  async function importSessionKey() {
    setBusy(true);
    setError(null);
    try {
      await installSeed(hexToBytesStrict(seedInput, 32));
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  }

  async function requestDevnetFunds() {
    if (!sender || !keypair) return;
    setBusy(true);
    setError(null);
    setTxHash(null);
    setTxStatus("Requesting local faucet credit…");
    try {
      const acknowledgement = await client.requestAirdrop(sender, 1_000_000_000n);
      if (!acknowledgement.success) throw new Error("local faucet did not acknowledge the credit");
      await refreshBalance(keypair.address);
      setTxStatus("Local faucet credit applied; this was not a transaction");
    } catch (reason) {
      const message = reason instanceof Error ? reason.message : String(reason);
      if (message.toLowerCase().includes("airdrop is disabled")) {
        setTxStatus(null);
        setError(`${message}. Fund this address on a multi-validator devnet with: ./target/release/aetherctl chain --rpc ${RPC_ENDPOINT} transfer --dev 0 --to ${sender} --amount 1000000000 --wait`);
      } else {
        setTxStatus(null);
        setError(message);
      }
    } finally {
      setBusy(false);
    }
  }

  async function sendTransfer(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!keypair || !recipient.trim()) return;
    setBusy(true);
    setError(null);
    setTxHash(null);
    setTxStatus("Preparing and signing locally…");
    try {
      const [chainId, account, feeParameters] = await Promise.all([
        client.getChainId(),
        client.getPendingAccount(sender!),
        client.getFeeParameters(),
      ]);
      const nonce = readNonce(account);
      const transaction = await client
        .transfer()
        .to(recipient.trim())
        .amount(parseAmount(amount))
        .memo(memo)
        .build({ keypair, chainId, nonce, feeParameters });
      setTxHash(transaction.hash);
      setTxStatus("Signed locally; submitting");
      const submitted = await client.submit(transaction);
      setTxStatus("Submitted; waiting for finality");
      await client.waitForFinality(submitted.txHash);
      setTxStatus("Finalized");
      const slot = await client.getSlotNumber();
      setCurrentSlot(slot);
      await refreshBalance(keypair.address);
    } catch (reason) {
      setTxStatus(null);
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  }

  const shortSender = sender ? `${sender.slice(0, 10)}…${sender.slice(-6)}` : "No key loaded";

  return (
    <main style={pageStyle}>
      <Card title="Aether Wallet" subtitle="Local/devnet transfer signing">
        <p style={warningStyle}>
          Local/devnet use only. This demo keeps the seed in this tab’s memory and does not encrypt or persist it.
          Never enter a mainnet key or use this wallet for real funds.
        </p>
        <div style={metricGridStyle}>
          <Metric label="RPC Endpoint" value={client.getEndpoint()} />
          <Metric label="Network" value={networkStatus} />
          <Metric label="Current Slot" value={currentSlot == null ? "—" : String(currentSlot)} />
          <Metric label="Session Account" value={shortSender} />
          <Metric label="Balance" value={balance ?? "—"} />
        </div>
      </Card>

      <Card title="Session Key" subtitle="Generated locally in this browser tab">
        <Section title="Key material">
          <label style={labelStyle} htmlFor="seed">Ed25519 seed (32-byte hex)</label>
          <input
            id="seed"
            type="password"
            autoComplete="off"
            value={seedInput}
            onChange={(event) => setSeedInput(event.target.value)}
            style={inputStyle}
          />
          <div style={buttonRowStyle}>
            <button type="button" disabled={busy} onClick={() => void createSessionKey()} style={buttonStyle}>
              Generate random session key
            </button>
            <button type="button" disabled={busy || !seedInput.trim()} onClick={() => void importSessionKey()} style={buttonStyle}>
              Load seed
            </button>
            <button type="button" disabled={busy || !sender} onClick={() => void requestDevnetFunds()} style={buttonStyle}>
              Request local faucet credit
            </button>
          </div>
          {sender && <p style={addressStyle}>Address: <code>{sender}</code></p>}
        </Section>
      </Card>

      <Card title="Send transfer" subtitle="Builds the Rust-compatible transaction, signs its canonical hash, then submits raw bytes">
        <form onSubmit={(event) => void sendTransfer(event)}>
          <div style={formGridStyle}>
            <label style={fieldStyle}>
              Recipient address
              <input required value={recipient} onChange={(event) => setRecipient(event.target.value)} style={inputStyle} placeholder="0x…" />
            </label>
            <label style={fieldStyle}>
              Amount (base units)
              <input required inputMode="numeric" value={amount} onChange={(event) => setAmount(event.target.value)} style={inputStyle} />
            </label>
            <label style={fieldStyle}>
              Memo (optional)
              <input value={memo} onChange={(event) => setMemo(event.target.value)} style={inputStyle} />
            </label>
          </div>
          <div style={buttonRowStyle}>
            <button type="submit" disabled={busy || !keypair || !recipient.trim()} style={buttonStyle}>
              {busy ? "Working…" : "Sign and send"}
            </button>
            {txHash && <code>Transaction: {txHash}</code>}
            {txStatus && <span role="status">{txStatus}</span>}
          </div>
        </form>
        {error && <p role="alert" style={{ color: "#ffb4ab" }}>{error}</p>}
      </Card>
    </main>
  );
}

function parseAmount(value: string): bigint {
  if (!/^[1-9][0-9]*$/.test(value)) throw new Error("amount must be a positive base-10 integer");
  return BigInt(value);
}

function readBalance(account: Record<string, unknown> | null): string | null {
  const value = account?.balance;
  if (typeof value === "string" && /^(0|[1-9][0-9]*)$/.test(value)) return `${value} base units`;
  if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return `${value} base units`;
  if (typeof value === "number") return "Unavailable (RPC balance exceeds JavaScript safe integer range)";
  return null;
}

function readNonce(account: Record<string, unknown> | null): bigint {
  const value = account?.nonce;
  if (typeof value === "string" && /^(0|[1-9][0-9]*)$/.test(value)) return BigInt(value);
  if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return BigInt(value);
  if (value === undefined || value === null) return 0n;
  throw new Error("pending account returned an invalid nonce");
}

const pageStyle: React.CSSProperties = {
  display: "grid",
  gap: "24px",
  padding: "24px",
  background: "#050b12",
  color: "#e5ecff",
  minHeight: "100vh",
  fontFamily: "Inter, system-ui, -apple-system, BlinkMacSystemFont",
};
const metricGridStyle: React.CSSProperties = { display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))", gap: "12px" };
const formGridStyle: React.CSSProperties = { display: "grid", gap: "14px", maxWidth: "720px" };
const fieldStyle: React.CSSProperties = { display: "grid", gap: "6px", color: "#bfcce8" };
const labelStyle: React.CSSProperties = { display: "block", marginBottom: "6px", color: "#bfcce8" };
const inputStyle: React.CSSProperties = { width: "100%", boxSizing: "border-box", padding: "10px 12px", color: "#e5ecff", background: "#0c1724", border: "1px solid #26384d", borderRadius: "6px" };
const buttonRowStyle: React.CSSProperties = { display: "flex", flexWrap: "wrap", alignItems: "center", gap: "10px", marginTop: "14px" };
const buttonStyle: React.CSSProperties = { padding: "10px 14px", color: "#e5ecff", background: "#173354", border: "1px solid #426a93", borderRadius: "6px", cursor: "pointer" };
const warningStyle: React.CSSProperties = { color: "#f4d58d", marginTop: 0, lineHeight: 1.5 };
const addressStyle: React.CSSProperties = { overflowWrap: "anywhere", color: "#bfcce8" };
