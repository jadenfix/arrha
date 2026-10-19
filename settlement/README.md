# Arrha settlement v1

Independent EVM payment/escrow package. No Arrha validator, token, bridge,
consensus or agent execution dependency. **Local E2E and capped Arbitrum Sepolia SDK journeys qualified.**
Published source matches exactly; payout/refund/dispute transactions finalized.
Independent security review and real-funds pilot remain release gates. Solana escrow and Bitcoin/Lightning payment adapters have separate local
E2E journeys; their browser/public integrations remain unqualified.

## Run

Node 22.12+ and npm; an ordinary wallet for public networks. Install dependencies
within this package (separate lockfile deliberately avoids the existing L1 apps):

```sh
cd settlement
npm ci --workspaces=false
npm run build
npm test
npm audit
```

The suite starts isolated Anvil processes on dynamically allocated loopback ports,
deploys real contracts and drives SDK/service/browser wallets through transactions.
Browser E2E uses the `agent-browser` CLI, invoked through npx if absent. It needs
Chrome/Chromium. Known disposable Anvil accounts are used only on chain 31337;
never send real funds to them. The local tests require no purchased crypto.

## Public test deployment

```sh
mkdir -p .local
npm run cli -- keygen .local/testnet-key.json
export ARRHA_KEY_FILE=.local/testnet-key.json
export ARRHA_RPC_URL=https://sepolia-rollup.arbitrum.io/rpc
export ARRHA_TESTNET_DEPLOY_BUDGET_WEI=1000000000000000
# Fund the printed address with at most this budget in Arbitrum Sepolia test ETH.
# Test deployment creates aTEST, which is NOT USDC and has no monetary value.
npm run cli -- deploy-test .local/arbitrum-sepolia.json
export ARRHA_MANIFEST=.local/arbitrum-sepolia.json
export ARRHA_DB=.local/settlement.sqlite
npm run serve
```

Open http://127.0.0.1:3300 with an injected Ethereum wallet on the manifest's chain.
The read-only HTTP service never receives wallet keys or signs transactions. It
serves locally bundled ethers, the shared protocol and SDK without a CDN.
Use independent payer/provider/arbitrator wallets. Fund each transaction sender
with gas; only the payer needs the chosen asset. Paste the quote's agreement
JSON, review it, sign it, approve the exact amount, then fund. Switch to the
provider wallet to accept and deliver with a signed evidence commitment. Switch
to the payer to settle or dispute. Only the agreed arbitrator can allocate a
dispute before its deadline; after that deadline anyone can refund the payer.
The UI shows canonical state separately from transaction inclusion.

For existing Circle testnet USDC, the read-only preflight profile is in
`deployments/arbitrum-sepolia-usdc.preflight.json`. Refresh it with
`ARRHA_RPC_URL=... node src/profile.mjs NEW_PROFILE.json`, then use
`npm run cli -- deploy-escrow NEW_PROFILE.json NEW_MANIFEST.json` with the same
dedicated testnet budget wallet. The helper verifies network, asset bytecode and
decimals and caps this profile at 1 test USDC/run and 10 outstanding. It refuses
mainnet. A token proxy bytecode hash does not freeze its issuer-controlled
implementation; that trust remains an explicit review gate.

The CLI intentionally accepts keys through a protected file path rather than
arguments. `keygen` refuses overwrite and writes mode 0600. Never commit or
paste these files. Back up the wallet before funding it. No mainnet deploy
command is enabled by this test deploy helper.

## SDK and durable agent operations

Import `EvmSettlement` from `src/sdk.mjs`, pass a deployment manifest, built ABI
and your ethers wallet. `signAgreement`, `signReceipt`, `request`,
`approveRequest` and `inspect` are the public adapter. Chain and bytecode checks
run before preparing actions. `src/protocol.mjs` exports the exact EIP-712 types.
See the executed `test/evm.e2e.test.mjs` and `test/browser.e2e.test.mjs` journeys.
For Solana and Bitcoin/Lightning, see [native adapters](docs/native-adapters.md).

For an agent using a local signer, `Store.prepare(operationId, wallet, request)`
populates/signs once and commits the raw signed transaction with FULL SQLite
synchronization **before** broadcasting. `broadcast` resubmits only that exact
raw transaction. `reconcile` checks receipt block canonicality and the configured
finality block. A conflicting operation ID or duplicate signer nonce fails
closed. Use one journal per signer; resolve a prepared nonce before preparing
another operation. Wallets are caller-owned; do not share the signer concurrently
outside this journal. `ReplacedOrUnknown` requires operator review. It is never
a new authorization to pay. No automatic fee replacement is implemented.

CLI commands after configuring RPC/manifest/db:

```sh
npm run cli -- sign terms.json
npm run cli -- receipt terms.json 0xEVIDENCE_HASH
npm run cli -- prepare operation-id transaction-request.json
npm run cli -- broadcast operation-id
npm run cli -- operation operation-id
npm run cli -- state
```

A wallet-extension browser cannot give us signed bytes before sending. The UI
persists an intent before asking the wallet, then the returned hash. If the
wallet response is lost, it blocks another send. Refresh recovers a lost hash by
matching sender, nonce, destination, exact calldata and value in canonical blocks
(up to a 1,000-block recovery window). Unknown, replaced or older operations
require operator inspection; there is no blind payment retry.

## Deliberately small design

One non-upgradeable escrow contract, one immutable asset, immutable per-run and
outstanding caps, no owner or sweep function, and no deployment registry contract.
Versioned manifests pin the network, contract runtime and asset identity. New
versions require an explicit opt-in migration; existing escrows finish under
existing rules. No queues, distributed workflow engine, custom token, gas
sponsorship or cross-chain transfers.

The indexer rebuilds the capped pilot's entire event projection in bounded RPC
chunks, then atomically replaces one snapshot after rechecking the canonical tip.
Duplicate events are deduplicated. Restarts/reorgs cannot preserve orphaned run
states. Off-chain evidence and transaction journals are separate tables. A
100,000-block backfill guard fails unavailable rather than silently dropping
history. This is a prototype strategy; an incremental checkpointed indexer is
required before growing beyond the declared pilot bound.

## Trust and release gates

Read [protocol](docs/protocol.md), [operations](docs/operations.md) and
[delivery gates](docs/status.md). A signature proves authorization or authorship,
not AI correctness. Dispute timeout favors refund to the payer; the provider
accepts this risk in the signed terms. The service assumes an honest configured
RPC and the manifest's finality policy. No independent security review has been
performed and no mainnet deployment is qualified.
