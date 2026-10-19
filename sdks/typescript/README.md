# Aether TypeScript SDK

The SDK provides JSON-RPC reads and Rust-compatible Ed25519-signed transfer
transactions for local Aether nodes. It requires Node.js 20+ and uses the node's
fee parameters, chain ID, and pending account nonce when building a transfer.

From the repository root, build it with:

```bash
npm ci
npm run build:sdk
```

## Signed transfer

```ts
import { AetherClient, bytesToHex0x, keypairFromSeed } from "@aether/sdk";

const client = new AetherClient("http://127.0.0.1:8545");
const keypair = await keypairFromSeed(seed); // exactly 32 seed bytes; fund on local/devnet first
const address = bytesToHex0x(keypair.address);
const [chainId, account, feeParameters] = await Promise.all([
  client.getChainId(),
  client.getPendingAccount(address),
  client.getFeeParameters(),
]);
const tx = await client.transfer()
  .to("0x" + "8b".repeat(20))
  .amount(1_000_000n)
  .memo("example")
  .build({
    keypair,
    chainId,
    nonce: BigInt(account?.nonce ?? 0),
    feeParameters,
  });
const submitted = await client.submit(tx);
const receipt = await client.waitForFinality(submitted.txHash);
console.log(tx.hash, receipt.slot);
```

`build` signs the canonical Rust transaction hash. `submit` sends the complete
bincode transaction using `aeth_sendRawTransaction` and rejects a returned hash
that differs from the locally computed hash. The signed transfer path requires
`aeth_getFeeParameters`, `aeth_chainId`, `aeth_getPendingAccount`, and
`aeth_sendRawTransaction`; there is no guessed fee fallback.

`requestAirdrop` returns a typed `{ success: true }` acknowledgement. It is a
single-validator local state credit, not a signed transaction or finalized
receipt, and the node disables it in multi-validator mode.

The retained live interoperability journey is run from the repository root with
a four-validator devnet and the release CLI available:

```bash
npm run e2e:client
```

It funds a temporary key using the Rust CLI, submits through this SDK, requires a
successful finalized receipt, and checks the exact recipient balance change
through a second validator. See [Getting Started](../../GETTING_STARTED.md) for
devnet setup and environment overrides. Matching output from another SDK is only
an encoding consistency check; the live Rust-accepted transaction is the
cross-language proof.

The separate job request builder only creates request data; the node does not
expose an integrated job execution or settlement service.

RPC account balances arrive as JSON values. JavaScript numbers above
`Number.MAX_SAFE_INTEGER` cannot represent every integer; callers must treat an
unsafe numeric balance as unavailable rather than converting it to a string and
presenting it as exact. The wallet and live E2E journey apply that guard.
