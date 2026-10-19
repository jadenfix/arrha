# Solana, Bitcoin and Lightning adapters

These are native implementations with separate trust/encoding boundaries, not
bridges and not a universal signature format. Local executed journeys do not
qualify public networks or mainnet deployments. The browser currently exposes
EVM only. Private evidence distribution, external verification, native-wallet
browser UX and production operations remain deployment integrations.

## Solana escrow

`solana/src/lib.rs` is an independently built Anchor/SPL Token program. No Arrha
node or Rust consensus import. `solana/client/sdk.mjs` builds native instructions
and journals signed transactions before broadcast using `SolanaJournal`.

```sh
cd settlement
npm ci --workspaces=false --prefix solana/client
cargo build-sbf --manifest-path solana/Cargo.toml --jobs 2
ARRHA_SOLANA_BIN=/path/to/verified/agave/bin npm run test:solana
```

Qualification used Agave 4.3.2, cargo-build-sbf 4.4.0/platform-tools 1.57, pinned
Anchor 0.32.1, web3.js 3.1.0, a real local validator, deployed program, SPL mint,
actual native signatures and token accounts. macOS Agave requires libusb. The
test accepts `ARRHA_SOLANA_DYLD_PATH=/path/to/libusb/lib` on macOS when npm strips
`DYLD_LIBRARY_PATH` from subprocesses. The local test shortens tick count for bounded finality waits and uses deterministic
**local-only** signer seeds. Never reuse its deploy keys with funds.

The `devnet` Cargo feature selects a separate random public program ID,
`3nf8gyJnj2aSLyeERjAysJTHCSjuBcxUA2giH8ZXS4hu`. Its operator key is outside
Git. Build it into a separate directory so it cannot replace the local test ELF:

```sh
cargo build-sbf --manifest-path solana/Cargo.toml --features devnet \
  --sbf-out-dir /absolute/path/to/devnet-build --jobs 2
```

`deployments/solana-devnet.preflight.json` records intended identity, image hash,
mint, authority and measured rent. It is **not** a deployed manifest. Deploy with
the ordinary Solana CLI using the protected public program key, payer key and a
persistent buffer key; use `--max-len` equal to the measured ELF byte length to
bound allocation. Retain buffer and program state after uncertain responses;
inspect canonical deployment before retrying. Never publish CLI recovery seeds.
After deployment verify the on-chain ProgramData image hash and authority, then
publish a manifest with `programDataHash` and deployment signature. The upgrade
authority remains explicitly held by the dedicated test operator.

`client/public-devnet.mjs` is a prepared, separately invoked public fixture runner.
It requires `ARRHA_PUBLIC_SOLANA=devnet`, `ARRHA_SOLANA_MANIFEST`,
`ARRHA_TEST_KEY_DIR` and `ARRHA_PUBLIC_WORK`; it refuses a different genesis,
program, payer or a wallet exceeding its 5 test-SOL budget. The payer covers test
transaction fees; provider/arbitrator still provide separate native signatures.
It journals mint/ATA/config setup and each payment before broadcasting and checks
finalized payout, refund, split balances and zero vault liabilities. This public
runner is unqualified until executed against the actual devnet deployment.

Terms use 32-byte run/work/evidence commitments, provider/arbitrator/receipt-signer
Pubkeys, u64 amount/nonce, i64 Unix deadlines. Native payer transaction signatures
bind exact terms, program, accounts and recent blockhash on the intended network;
there is no claim of EIP-712 signature parity or delegated session authorization.
The receipt signer co-signs delivery with its evidence hash. This proves the
committed claim's authorship, not correctness. Each payer/run PDA is unique and
retained permanently; each payer/nonce PDA is allocated on funding or revocation
and cannot be reused. Associated vaults belong to the run PDA. Account ownership,
legacy token program identity, approved mint and decimals, fixed recipient owners,
checked transfers, deadlines and terminal states are enforced on chain.

Initialization requires the actual program upgrade authority from ProgramData.
Only that signer may set the immutable asset/caps once. The SDK pins genesis,
program-data bytecode hash and initialized mint/decimals. The upgrade authority
can still change program code until explicitly removed. A public profile must
publish that authority and its custody, then independently review and finalize
(or explicitly govern) upgrades before real funds. EVM's no-upgrade promise must
not be copied onto this Solana deployment. State/nonce accounts and vault rent
remain allocated to preserve replay evidence; costs must be measured publicly.

`SolanaJournal` retains exact signed bytes/signature and last-valid block height.
A lost response resolves through signature history. No automatic blockhash refresh
or re-signing of an uncertain payment. Expiry or unknown history requires review;
run and nonce PDAs independently prevent a second funding effect. Public RPC
history retention and recovery availability need their own qualification.

## Bitcoin native payments

`src/bitcoin.mjs` uses a caller-owned Bitcoin Core wallet. No custody server,
EVM escrow emulation, Lightning fallback, refund/dispute promise or cross-chain
transfer. The payer must authenticate the provider's quote through a trusted
channel; an unkeyed request checksum is a mutation detector, not provider identity.

```sh
ARRHA_BITCOIND=/path/to/verified/bitcoind npm run test:bitcoin
```

Qualification used Bitcoin Core 31.1 on isolated, offline regtest. It creates
unique receive addresses bound to the retained agreement hash, exact integer
satoshis, expiry, network/genesis and explicit confirmation policy. JSON monetary
lexemes are retained as strings; financial calculations use bigint. Wallet PSBT
funding/signing is local and bounded by the total authorized amount plus fee.
Persist finalized raw bytes/txid before broadcast. One request has one journaled
payment; uncertain RPC responses cannot create a new transaction. Keep the same
wallet and journal; payments made manually outside the adapter are outside its
local duplicate-prevention boundary.

Recipient observation reconstructs wallet transactions and verifies active-chain
blocks. Unconfirmed/conflicted transactions are not qualified payments. Real
block invalidation/reconsideration changes the projected payment back to pending
and then paid. Late confirmations are distinguished from timely qualified
payment; late/partial funds require manual reconciliation, not an automatic
refund claim. Provider wallets must retain history and run a synchronized trusted
node. Regtest execution does not prove signet/testnet or mainnet operation.

## Lightning payments

`src/lightning.mjs` uses LND REST with verified TLS and scoped operator-supplied
macaroons in real deployments. No insecure certificate fallback. Node identity,
Bitcoin network and synchronization are verified before effects.

```sh
ARRHA_BITCOIND=/path/to/verified/bitcoind \
ARRHA_LND_BIN=/path/to/verified/lnd-and-lncli npm run test:lightning
```

Qualification used two LND 0.21.4-beta nodes, an actual funded local channel and
Bitcoin Core regtest. Test processes use `--noseedbackup --no-macaroons` only on
isolated loopback; these flags are never production configuration.

Provider invoice creation journals a random preimage/payment hash before calling
LND, then recovers any existing invoice by that hash before attempting creation.
BOLT11's description hash binds the exact agreement commitment. The payer decodes
and checks destination, payment hash, network, exact millisatoshis, expiry and fee
budget before journaling an intent. Invoice hash is unique across operation IDs.
Payment-history recovery is paginated and bounded at 10,000 records; beyond this,
fail for operator review rather than misreporting absence. Interrupted responses
are looked up, not retried blindly. A failed payment remains failed; reattempts
need a new explicit policy decision. Successful recovery verifies that SHA-256 of
the returned preimage matches the payment hash. This is payment evidence, not
proof of AI work quality. LND may automatically cancel expired invoices; both
expired and canceled states remain unpaid and are rejected before submission.

Payment lifecycle/operational references:
[Solana programs](https://solana.com/docs/core/programs),
[checked SPL transfers](https://solana.com/docs/tokens/basics/transfer-tokens),
[Bitcoin Core releases](https://bitcoincore.org/en/download/),
[LND payment lifecycle](https://docs.lightning.engineering/the-lightning-network/payment-lifecycle/understanding-lightning-invoices).
