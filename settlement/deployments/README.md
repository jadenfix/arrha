# Deployment registry

Current Arbitrum Sepolia test-USDC escrow (deployment v2):
`0xF18e16406B871d67CFAFd30f9630c294474Eb566`.
`arbitrum-sepolia-usdc.json` selects the current deployment and pins its source
revision, source hash, creation transaction/block, runtime hash, asset identity,
immutable caps and finality policy. Caps are 1 test USDC/run and 10 outstanding.
Sourcify confirmed exact creation/runtime matching on 2026-10-19. All 12 public v2
payout/refund/dispute actions finalized; canonical reconstruction shows three
terminal runs and zero liabilities. The v2 journey moved 1.4 test USDC net to
the provider, leaving cumulative payer/provider balances 17.2/2.8 test USDC.

V2 adds an exact payer-debit check alongside escrow-credit and payout checks,
rejecting tokens that charge a fee on top of the authorized amount. The old-bytecode
negative control reproduced this unsupported behavior; current local E2E rejects
it atomically. EIP-712 domain version 1 is unchanged; the new verifying-contract
address provides explicit deployment binding. There is no contract upgrade or
silent movement of deposits.

Historical v1 is `0xc4a9e0594FaAbE346BcfB963B551a7aE5c38d0cf`. Its `.v1.json`,
`.v1.verification.json` and `.v1.journeys.json` retain the exact old source/deployment
and finalized evidence: 14 actions, Settled/Refunded/Resolved, zero liabilities,
18.6 payer / 1.4 provider / 0 escrow test USDC. Use the current profile for new
integrations; v1 evidence does not qualify the changed v2 bytecode.

Solana devnet: `solana-devnet.json` records program, mint, image hash, genesis,
upgrade authority, limits and finalized deployment signature.
`solana-devnet.journeys.json` records payout/refund/dispute, an explicitly reviewed
expired-delivery replacement, and restart evidence. All 17 applied actions
finalized; one expired authorization is retained. The image matches the local
build; independent reproducible-build/security review and mainnet remain open.

Public EVM and native fixture runners are explicit opt-ins outside the default
suite. They pin test-network identities, retain authorizations before effects and
verify balances. Keep signer journals and keys outside Git, and do not run another
writer against these signers concurrently. EVM operation IDs include deployment
identity, allowing historical and current deployments to share one signer journal.
Compiler input is reproducible using pinned locks and `npm run build`. Source
matching is not independent security review. No real-funds deployment is enabled
or qualified. Test assets have no monetary value.
