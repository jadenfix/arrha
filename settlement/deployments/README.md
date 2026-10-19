# Deployment registry

Arbitrum Sepolia Circle test-USDC escrow:
`0xc4a9e0594FaAbE346BcfB963B551a7aE5c38d0cf`.
See `arbitrum-sepolia-usdc.json` for the creation transaction, block, runtime hash,
asset identity and finality policy. Immutable caps are 1 test USDC/run and 10
outstanding. Test tokens have no monetary value.

Sourcify v2 confirmed exact creation and runtime matches on 2026-10-19;
`arbitrum-sepolia-usdc.verification.json` records the result. Compiler input is
reproducible with the pinned package lock and `npm run build`. Source matching
is not an independent security review. Arbiscan/Blockscout mirroring was not
confirmed. No mainnet deployment is enabled or qualified.

Public journeys are run explicitly with `test/public-evm.mjs`, outside the default
suite. It permits only chain 421614, the fixed test-USDC address and the dedicated
public test payer; saves terms/evidence and signed transactions before effects;
and verifies payout/refund/dispute token balances. Preserve its journal and keys
outside Git. Its report distinguishes included transactions from finalized ones.
Never run it concurrently with another user of the same signers.

On 2026-10-19 all 14 public SDK operations finalized. Canonical reconstruction
shows Settled, Refunded and Resolved, with zero outstanding liabilities. The payer
retained 18.6 test USDC; the provider received 1.4; the escrow holds zero. Restart
reused the same operations and signer nonces without another financial effect.
The finalized report and receipt-fee measurements accompany delivery evidence.
