# Delivery gates

This file is a checklist, not a declaration that the complete roadmap is shipped.
Executed receipts and exact source revisions accompany release evidence separately.

| Gate | Current implementation / remaining evidence |
| --- | --- |
| Decisions, wire types, trust | EVM v1 specified; Circle test-USDC identity/code/decimals verified; mainnet budget pending |
| EVM actual funds | Real locally deployed contracts, SDK and balances exercised |
| Scoped signatures/replay | Local negative journeys include wrong chain/deployment, terms mutation, expiry, revocation and replay |
| Durable agent submission | SQLite before-broadcast journal, process-exit recovery with same hash exercised |
| Canonical indexing | Restart, duplicate logs, real Anvil snapshot/revert and RPC unavailability exercised; full pilot replay has explicit bounds |
| Browser two-wallet journey | Payer/provider and independent arbitrator browser paths exercised locally; no wallet-extension/hardware-wallet qualification |
| Recovery for unknown wallet response | Fail-closed pending intent; lost-hash sender/nonce/calldata recovery exercised in browser, bounded to 1,000 blocks |
| Adversarial token/security | Local fee-token rejection, failed-transfer rollback, reentrancy rejection, donation accounting and caps exercised; Slither 0.11.6 executed (87 reports including dependencies; triage recorded); independent security review pending |
| Public testnet | Arbitrum Sepolia funded through free faucets; capped test-USDC escrow deployed; Sourcify exact source/creation/runtime match; Historical v1: 14 public transactions finalized with canonical zero-liability projection and restart proof. Current strict-debit v2: source exact-match verified; public inclusion/finality being recorded |
| Capped mainnet pilot | Funding approval, reviewed asset, independent review, verified public deployment, operating/custody controls and reconciled pilot pending |
| Solana | Native SPL-token escrow + SDK/journal local-validator E2E passed: payout, refund, dispute, bad signer/destination/program, revocation, recovery and deadlines; public devnet SDK payout/refund/dispute and reviewed expired-delivery recovery finalized, restart preserved 18 signed records (17 applied, one expired); browser, independent review and mainnet pending |
| Bitcoin on-chain and Lightning | Bitcoin Core regtest payment/reorg/recovery E2E and two-node LND payment/expiry/recovery E2E passed; public test-network qualification pending; no escrow parity advertised |

The long-running goal must remain active/incomplete until the requested
implementation and the corresponding executed release gates are satisfied.

Dependency checks: EVM/Node and Solana client npm audits report zero findings.
Solana Cargo audit reports zero vulnerability entries and three informational
transitive warnings: bincode/libsecp256k1 unmaintained and rand 0.7.3 unsound
with a custom reentrant logger (RUSTSEC-2026-0097). These remain review items,
not silently waived launch evidence.
