import React, { useEffect, useMemo, useState } from "react";

import { AetherClient } from "@aether/sdk";
import { Card, Metric, Section } from "@aether/ui";

const RPC_ENDPOINT =
  (globalThis as { __AETHER_RPC_ENDPOINT__?: string }).__AETHER_RPC_ENDPOINT__ ??
  "http://127.0.0.1:8545";
const client = new AetherClient(RPC_ENDPOINT);

interface TransferPreview {
  txHash: string;
  sender: string;
  recipient: string;
  amount: string;
  fee: string;
  slot: number | null;
  mode: "live" | "offline";
}

interface JobPreview {
  url: string;
  method: string;
  jobId: string;
  modelHash: string;
  preparedMatches: boolean;
}

const DEMO_SENDER = "0x1111111111111111111111111111111111111111";
const DEMO_PUBLIC_KEY = "0x" + "aa".repeat(32);
const DEMO_SIGNATURE = "0x" + "bb".repeat(64);
const DEMO_RECIPIENT = "0x8b0b54d2248a3a5617b6bd8a2fd4cc8ebc0f2e90";

export function App() {
  const [transferPreview, setTransferPreview] = useState<TransferPreview | null>(null);
  const [networkStatus, setNetworkStatus] = useState("Connecting to local RPC...");

  useEffect(() => {
    let cancelled = false;

    async function loadTransferPreview() {
      const tx = client
        .transfer()
        .to(DEMO_RECIPIENT)
        .amount(750_000n)
        .memo("wallet demo")
        .fee(2_750_000n)
        .gasLimit(650_000)
        .build({
          sender: DEMO_SENDER,
          senderPublicKey: DEMO_PUBLIC_KEY,
          signature: DEMO_SIGNATURE,
          nonce: 7,
        });

      try {
        const slot = await client.getSlotNumber();
        if (cancelled) {
          return;
        }
        setNetworkStatus("Live JSON-RPC (preview mode)");
        setTransferPreview({
          txHash: tx.hash(),
          sender: DEMO_SENDER,
          recipient: DEMO_RECIPIENT,
          amount: "750,000 AIC",
          fee: "2,750,000 lamports",
          slot,
          mode: "live",
        });
      } catch {
        if (cancelled) {
          return;
        }
        setNetworkStatus("RPC unavailable (showing offline preview)");
        setTransferPreview({
          txHash: tx.hash(),
          sender: DEMO_SENDER,
          recipient: DEMO_RECIPIENT,
          amount: "750,000 AIC",
          fee: "2,750,000 lamports",
          slot: null,
          mode: "offline",
        });
      }
    }

    loadTransferPreview();
    return () => {
      cancelled = true;
    };
  }, []);

  const jobPreview = useMemo<JobPreview>(() => {
    const submission = client
      .job()
      .id("wallet-hello-job")
      .model("0x" + "12".repeat(32))
      .input("0x" + "ab".repeat(32))
      .maxFee(600_000_000)
      .expiresAt(new Date(Date.now() + 3_600_000))
      .toSubmission();

    const prepared = client.prepareJobSubmission(submission.body);
    return {
      url: prepared.url,
      method: prepared.method,
      jobId: prepared.body.jobId,
      modelHash: prepared.body.modelHash,
      preparedMatches: prepared.body.jobId === "wallet-hello-job"
    };
  }, []);

  return (
    <div style={{
      display: "grid",
      gap: "24px",
      padding: "24px",
      background: "#050b12",
      minHeight: "100vh",
      fontFamily: "Inter, system-ui, -apple-system, BlinkMacSystemFont"
    }}>
      <Card title="Wallet Overview" subtitle="Transaction payload preview">
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(160px, 1fr))", gap: "12px" }}>
          <Metric label="Default Endpoint" value={client.getEndpoint()} />
          <Metric label="Network Mode" value={networkStatus} />
          <Metric
            label="Demo Sender"
            value={`${(transferPreview?.sender ?? DEMO_SENDER).slice(0, 10)}…`}
          />
          <Metric
            label="Transfer Hash"
            value={transferPreview ? transferPreview.txHash.slice(0, 12) + "…" : "pending"}
          />
          <Metric
            label="Current Slot"
            value={transferPreview?.slot != null ? String(transferPreview.slot) : "n/a"}
          />
        </div>
      </Card>

      <Card title="Transfer Preview" subtitle="Offline payload">
        <Section title="Details">
          <dl style={{ display: "grid", gap: "4px", gridTemplateColumns: "auto 1fr", color: "#e5ecff" }}>
            <dt>Recipient</dt>
            <dd style={{ margin: 0 }}>{transferPreview?.recipient ?? DEMO_RECIPIENT}</dd>
            <dt>Amount</dt>
            <dd style={{ margin: 0 }}>{transferPreview?.amount ?? "750,000 AIC"}</dd>
            <dt>Fee</dt>
            <dd style={{ margin: 0 }}>{transferPreview?.fee ?? "2,750,000 lamports"}</dd>
            <dt>Submit Path</dt>
            <dd style={{ margin: 0 }}>
              {transferPreview?.mode === "live" ? "manual submit only" : "local fallback"}
            </dd>
          </dl>
        </Section>
      </Card>

      <Card title="Job Submission" subtitle="Hello AIC tutorial">
        <Section title="Coordinator Request">
          <dl style={{ display: "grid", gap: "4px", gridTemplateColumns: "auto 1fr", color: "#e5ecff" }}>
            <dt>Endpoint</dt>
            <dd style={{ margin: 0 }}>{jobPreview.url}</dd>
            <dt>Method</dt>
            <dd style={{ margin: 0 }}>{jobPreview.method}</dd>
            <dt>Job ID</dt>
            <dd style={{ margin: 0 }}>{jobPreview.jobId}</dd>
            <dt>Model</dt>
            <dd style={{ margin: 0 }}>{jobPreview.modelHash.slice(0, 18) + "…"}</dd>
            <dt>Prepared Matches</dt>
            <dd style={{ margin: 0 }}>{jobPreview.preparedMatches ? "yes" : "no"}</dd>
          </dl>
        </Section>
      </Card>
    </div>
  );
}
