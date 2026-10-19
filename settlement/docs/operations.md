# Recovery and deployment runbook

## Wallets and authority

Use independent payer, provider and arbitrator keys. A dedicated test deployer
holds only an approved gas budget. Keygen writes a fresh file with 0600 mode;
backup and custody remain operator responsibilities. HTTP serves only read state;
no server signing or public transaction-broadcast endpoint exists.

Pause/upgrade/admin: none in escrow v1. Stop proposing new agreements to pause the
product; users retain contract access to resolve existing escrows. An issuer can
freeze its token; do not promise recovery that the token cannot provide. Deploy
new code at a new address, verify source/runtime, publish its manifest and require
new authorizations. There is no silent migration of deposits.

## Crash after broadcast

Restart with the same journal. Reconcile the operation's already stored hash and
check the canonical receipt and finality. If not found, broadcasting the identical
stored raw bytes is safe against duplicate nonce execution. Do not generate a new
payment transaction. A known consumed nonce with unknown hash is
`ReplacedOrUnknown`: inspect wallet/provider history. Never mark it paid.
SQLite WAL/FULL commits the signed transaction first. Preserve database, WAL and
backups together. Do not delete the DB to resolve a nonce conflict.

## RPC outage, indexing and reorganization

HTTP returns 503 and `availability: unavailable` on any failed canonical read.
Do not substitute a stale snapshot as confirmed. Retry against the configured
trusted provider after recovery. A provider change must preserve exact chain and
deployment validation. Rebuild from deployment block plus separate retained
evidence; events are deduplicated and projection commit is atomic. Recheck the
tip hash before publishing. Beyond the configured backfill budget the service
fails closed and needs an explicitly reviewed indexing upgrade.

## Deadlines and arbitration unavailable

Anyone can refund after missed acceptance/delivery, or after a dispute's
resolution deadline. A valid delivery becomes payable to the provider after the
review window unless disputed. Arbitrator allocation is bounded by deposit and
cannot choose a third recipient. Publish this trust model with every quote.
Monitor approaching review/resolution deadlines; a stopped service does not
extend a contract deadline.

## Before a public pilot

Record exact Git revision, compiler/settings and source input, dependency lock,
network, deployment transaction/block, token address/decimals/issuer powers,
contract runtime hash, independently verified source, configured finalized tag,
per-run/outstanding caps, approved gas budget, operator and arbitrator custody.
Reconcile balances and outstanding liabilities; monitor receipts/failures,
unknown/replaced operations, RPC health and index lag. Test restart/outage,
blacklisting/token transfer failure and end-user recovery. Close all critical/high
findings from independent contract/service review. Complete testnet first.
No mainnet gate follows merely from a green local suite.
