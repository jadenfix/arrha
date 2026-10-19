# Static analysis and review handoff

Slither 0.11.6 analyzed the exact ArrhaEscrow source and pinned OpenZeppelin
sources with solc 0.8.37, optimizer 200, Cancun. Standard JSON output was expanded
to include AST/source maps for analysis; contract source bytes were unchanged.
The test-only TestToken and AttackToken fixtures were excluded. All 102 default
detectors ran. The raw report contains 87 entries, including dependency code:
2 high, 11 medium, 7 low, and 67 informational. This is not a clean-scan claim
or an independent security audit. Raw JSON and execution log accompany outputs.

Author triage, requiring independent confirmation before real funds:

- `arbitrary-send-erc20` (high), ArrhaEscrow.fund: intentional transfer from the
  payer named in terms. The preceding SignatureChecker verification binds every
  field to that payer, network and contract; nonce/run consumption prevents
  replay. Local E2E covers changed parties/amount/asset, missing/invalid/revoked
  signatures and replay. An untrusted relayer cannot change destinations. Review
  must independently confirm the signed tuple encoding and ERC-1271 assumptions.
- `incorrect-exp` (high), OpenZeppelin Math.mulDiv: XOR in the modular inverse
  seed `(3 * denominator) ^ 2` is intentional library arithmetic, not a request
  to exponentiate. Do not patch a reviewed dependency based on this heuristic.
- Medium `divide-before-multiply` and `unused-return` reports are in pinned
  dependency math/signature code. Review their exact call reachability and
  invariants; they have not been silently removed from the raw findings.
- Seven timestamp reports cover explicit authorization, acceptance, delivery,
  review and arbitration deadlines. The protocol deliberately uses chain time;
  callers need windows tolerant of sequencer/parent clock behavior and outages.
- Informational assembly, pragma/compiler versions, naming, constants, dead code
  and low-level-call reports remain visible. Compiler warnings include future
  reservation of the identifier `error` in dependency source.

Solana Cargo audit separately reports zero vulnerability entries and three
transitive informational warnings. See status.md. Static tools and author triage
cannot close the independent contract, SDK, browser, service and key-custody
review gate. Mainnet remains disabled by the deployment helper.

The current source also rejects tokens that credit the exact escrow amount while
charging the payer an additional fee. An executed old-bytecode negative control
authorized 1,000,000 units but debited 1,000,001. The current deployed-contract
E2E rejects that funding atomically, preserving payer balance, nonce and zero
liabilities. Existing v1 test deployments are historical; use a newly qualified
deployment for this strict-debit revision. No proxy upgrade is possible.
