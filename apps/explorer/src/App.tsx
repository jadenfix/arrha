import React from "react";

import { Card, Metric, Section } from "@aether/ui";
import { useExplorerData } from "./hooks/useChainStats.js";
import { JobsList } from "./components/JobsList.js";
import { ValidatorsTable } from "./components/ValidatorsTable.js";

export function App() {
  const { stats, validators, jobs, source } = useExplorerData();

  return (
    <div style={{
      display: "grid",
      gap: "24px",
      padding: "24px",
      background: "#050b12",
      minHeight: "100vh",
      fontFamily: "Inter, system-ui, -apple-system, BlinkMacSystemFont"
    }}>
      <Card title="Network Overview" subtitle="Local chain overview">
        <div style={{ display: "grid", gridTemplateColumns: "repeat(auto-fit, minmax(150px, 1fr))", gap: "12px" }}>
          <Metric label="Data Source" value={source === "live" ? "Live RPC" : "Mock fallback"} />
          <Metric label="Slot Time" value={`${stats.slotTimeMs} ms`} hint="p95" />
          <Metric label="Finality Lag" value={`${stats.finalityLagSlots} slots`} />
          <Metric label="Throughput" value={`${stats.tps.toLocaleString()} TPS`} />
          <Metric label="Uptime" value={`${stats.uptimePercentage}%`} />
        </div>
      </Card>

      <Card title="Validator Performance" subtitle="Top performers last epoch">
        <ValidatorsTable validators={validators} />
      </Card>

      <Card
        title="Active AI Jobs"
        subtitle="Coordinator feed"
        action={<a href="#jobs" style={{ color: "#7ab7ff" }}>Open API docs</a>}
      >
        <Section title="Jobs">
          <JobsList jobs={jobs} />
        </Section>
      </Card>
    </div>
  );
}
