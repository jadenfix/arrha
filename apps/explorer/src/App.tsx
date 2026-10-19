import React from "react";

import { Card, Metric, Section } from "@aether/ui";
import { useExplorerData } from "./hooks/useChainStats.js";

export function App() {
  const { endpoint, currentSlot, finalizedSlot, latestBlock, error } = useExplorerData();
  const header = latestBlock?.header as Record<string, unknown> | undefined;
  const blockSlot = header?.slot;
  const proposer = formatValue(header?.proposer);

  return (
    <main style={pageStyle}>
      <Card title="Aether Explorer" subtitle="Live facts reported by the connected RPC node">
        <div style={metricGridStyle}>
          <Metric label="RPC Endpoint" value={endpoint} />
          <Metric label="Connection" value={error ? "Unavailable" : "Connected"} />
          <Metric label="Current Slot" value={currentSlot == null ? "—" : String(currentSlot)} />
          <Metric label="Finalized Slot" value={finalizedSlot == null ? "—" : String(finalizedSlot)} />
          <Metric
            label="Finality Lag"
            value={currentSlot == null || finalizedSlot == null
              ? "—"
              : `${Math.max(0, currentSlot - finalizedSlot)} slots`}
          />
          <Metric label="Latest Block" value={blockSlot == null ? "—" : String(blockSlot)} />
          <Metric label="Transactions in Latest Block" value={latestBlock?.transactions.length.toLocaleString() ?? "—"} />
          <Metric label="Latest Proposer" value={proposer ?? "—"} />
        </div>
        {error && (
          <p role="status" style={{ color: "#ffb4ab", margin: "16px 0 0" }}>
            RPC request failed: {error}
          </p>
        )}
      </Card>

      <Card title="Latest Block" subtitle="Data returned by aeth_getBlockByNumber">
        <Section title="Block Details">
          {latestBlock ? (
            <dl style={detailsStyle}>
              <dt>Slot</dt><dd>{formatValue(header?.slot) ?? "—"}</dd>
              <dt>Timestamp</dt><dd>{formatValue(header?.timestamp) ?? "—"}</dd>
              <dt>Proposer</dt><dd>{proposer ?? "—"}</dd>
              <dt>Transactions</dt><dd>{latestBlock.transactions.length}</dd>
            </dl>
          ) : (
            <p style={{ color: "#bfcce8", margin: 0 }}>No block data is available from this RPC yet.</p>
          )}
        </Section>
      </Card>

      <p style={{ color: "#8aa0c8", margin: 0 }}>
        Validator performance, uptime, and AI job status are not reported by the current RPC API.
      </p>
    </main>
  );
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

const metricGridStyle: React.CSSProperties = {
  display: "grid",
  gridTemplateColumns: "repeat(auto-fit, minmax(180px, 1fr))",
  gap: "12px",
};

const detailsStyle: React.CSSProperties = {
  display: "grid",
  gap: "6px",
  gridTemplateColumns: "max-content 1fr",
  margin: 0,
};

function formatValue(value: unknown): string | null {
  if (value == null) return null;
  if (typeof value === "string" || typeof value === "number") return String(value);
  if (Array.isArray(value) && value.every((item) => Number.isInteger(item) && item >= 0 && item <= 255)) {
    return `0x${value.map((item) => Number(item).toString(16).padStart(2, "0")).join("")}`;
  }
  return null;
}
