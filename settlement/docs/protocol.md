# Settlement protocol and trust decisions

## Deployment decision

First intended public network: Arbitrum Sepolia (421614), then a separately
reviewed Arbitrum One (42161) deployment. Off-chain agent execution. First local
asset is a six-decimal **aTEST** fixture, not a stablecoin. Proposed public asset
is native Circle USDC, pending an approved deployment profile, official address
verification and issuer/proxy-risk review. No unapproved real asset is enabled.
Solana escrow and Bitcoin/Lightning payments have separately exercised local
adapters; see native-adapters.md for the exact capability and qualification limits. Arrha's original L1 remains optional and untouched.

No bridge, new Arrha token, contract upgrades, fee-taking admin, arbitrary
execution, relayer custody or gas sponsorship. Pilot deployment caps are 10
whole fixture units/run and 100 outstanding; real-funds limits and gas budget
must be explicitly recorded before that gate. The constructor validates the
asset's code/decimals. Exact incoming balance change rejects transfer-fee tokens.
Rebasing, malicious, fee-on-transfer, transfer-hook and variable-balance tokens
are unsupported; an allowlisted address must first be independently reviewed.
USDC's issuer can pause/blacklist/upgrade its token; Arrha cannot bypass that.

Arbitrum depends on its sequencer, Ethereum/Sepolia parent, rollup protocol,
data availability and RPC providers. L2 inclusion is not parent-chain finality.
The indexer uses `finalized`, rather than an arbitrary number of confirmations.
Withdrawals to Ethereum have their own rollup challenge/delay behavior; Arrha
escrow itself neither bridges nor accelerates withdrawals. Pilot costs include
contract creation and approval/funding/accept/delivery/settlement transactions;
public cost measurement is pending, not inferred from local gas receipts.

References: [Arbitrum chain profiles](https://docs.arbitrum.io/chain-info),
[EIP-712](https://eips.ethereum.org/EIPS/eip-712),
[OpenZeppelin signatures](https://docs.openzeppelin.com/contracts/5.x/api/utils/cryptography),
[safe ERC20 transfers](https://docs.openzeppelin.com/contracts/5.x/api/token/erc20).

## Encoding and enforcement

Version 1 has exactly one mode: payer acceptance, or permissionless settlement
after an undisputed review deadline; named arbitration after a timely dispute.
No receipt-only automatic correctness claim. Integer amounts are exact native
asset units. JSON represents large integers as canonical decimal strings;
EIP-712 uses uint256 words. Times are Unix seconds against chain block timestamps.

The provider produces a proposed agreement and the payer verifies every term
before signing. `EvmSettlement.signAgreement` encodes its exact types. The
contract verifies that signature at funding; modification requires a fresh
signature/nonce, and an already funded agreement cannot be modified.

| Field | Producer / verifier | Encoding and enforcement |
| --- | --- | --- |
| version, network, escrow | deployment manifest / SDK, signer and contract | EIP-712 domain name ArrhaEscrow, version 1, chainId, verifyingContract |
| runId | provider quote / payer, contract | bytes32 unique per deployment, nonzero; persists after terminal state |
| payer, provider | quote / payer and contract | addresses; nonzero and different; fixed token destinations |
| arbitrator | both parties / contract caller | address; nonzero, independent of parties; only bounded split during dispute |
| receiptSigner | both parties / contract | address; nonzero, exact receipt signature verifier |
| asset | deployment and quote / SDK, contract | exact immutable ERC20 address; decimals pinned in manifest and constructor |
| amount | quote / payer, contract | uint256; nonzero, per-run/outstanding caps, actual incoming balance checked |
| workHash | provider / payer, arbitrator | bytes32 nonzero commitment; contract binds, humans/verifier evaluate content |
| evidencePolicy | both parties / payer, arbitrator | bytes32 nonzero policy commitment; contract binds, off-chain service enforces retention/access |
| fundBy | quote / contract | uint256; authorization expiry, funding at or before deadline |
| acceptBy | quote / contract | uint256; provider acceptance deadline, subsequent permissionless refund |
| deliverBy | quote / contract | uint256; delivery deadline, subsequent permissionless refund if undelivered |
| reviewUntil | quote / contract | uint256; payer dispute window, then permissionless provider payout |
| resolveBy | quote / contract | uint256; arbitrator deadline, then permissionless payer refund |
| nonce | payer / contract | uint256 one-use per payer; payer may revoke before funding |

Strict order: fundBy < acceptBy < deliverBy < reviewUntil < resolveBy.
The contract struct hash uses ABI encoding of TYPEHASH followed by the entirely
static Agreement tuple; SDK/EVM E2E asserts equality. The signed message is the
EIP-712 domain-separated digest. OpenZeppelin SignatureChecker supports EOA and
ERC-1271 verification, but ERC-1271 wallet integration is not separately qualified.

Receipt is EIP-712 `Receipt(bytes32 runId,bytes32 agreementHash,bytes32 evidenceHash)`
under the same domain. The receipt signer claims authorship of the evidence
commitment. The provider separately submits delivery. Evidence policy names
what an independent verifier/arbitrator should evaluate; neither a valid receipt
nor a stored hash is proof that the work is correct.

## Transitions

Signed/off-chain quote → Funded → Accepted → Delivered → Settled.
Delivered → Disputed → Resolved; timeout/cancellation → Refunded.
There is no storage-allocating unfunded Created transaction.

| Transition | Actor and deadline | Economic effect |
| --- | --- | --- |
| fund | anyone presenting payer signature, by fundBy | exact amount debited from payer's exact allowance to escrow |
| accept | provider by acceptBy | no transfer |
| deliver | provider by deliverBy with receiptSigner signature | anchor nonzero evidence commitment |
| settle | payer after delivery, or anyone after reviewUntil | full amount to fixed provider |
| dispute | payer after delivery, through reviewUntil | freezes ordinary settlement |
| resolve | fixed arbitrator while disputed, through resolveBy | amount splits only between fixed payer and provider |
| refund | payer can cancel Funded; anyone after missed accept/deliver or disputed resolve deadline | full amount to fixed payer |

Accounting changes precede safe ERC20 calls and revert atomically if transfers
fail. Financial operations are nonreentrant. Terminal runs cannot pay again;
used nonces cannot reopen any run. No owner can change terms, assets, recipients,
caps, or withdraw deposits. Direct unsolicited token donations do not create
escrow liabilities and are not sweepable. The supported invariant is token
balance >= outstanding liabilities (exact equality absent donations).

## Evidence storage

Store evidence outside the chain; hash exact bytes before signing. Retain terms,
authorization, receipt, policy and access-controlled evidence through all review,
arbitration, and relevant retention periods. This package stores local references
only; encrypted object storage, access/retention service, independent verification
and private evidence delivery are unqualified deployment integrations. Do not put
private transcripts into public transactions or expose signed raw transactions
through HTTP. SQLite journals contain executable authorizations and need local
file/backup protection.
