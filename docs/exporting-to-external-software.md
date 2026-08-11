# Custody-provider voting handoff

This flow lets a custody provider delegate a customer's voting weight, then
give the customer the material needed to vote without exposing the Zcash keys
that control the customer's funds.

## Trust model

The provider generates and retains a fresh voting hotkey for one customer and
round. The customer receives a copy of that voting authority, not an exclusive
transfer. The provider can observe the customer's vote and can cast or race a
vote using its retained copy. The voting hotkey cannot spend funds held by the
original delegating account. It can spend funds sent to its own Orchard
receiver, so that receiver MUST NOT receive value or be reused outside the
delegation for that customer and round. The same receiver is required for every
bundle within that delegation.

Do not use this flow when the customer must have voting privacy or exclusive
voting authority against the provider.

## Handoff

1. The provider calls `generate_random_voting_hotkey` once for the customer and
   round, then uses that `VotingHotkey` with the existing delegation APIs for
   every bundle.
2. After every bundle is proven and signed, the provider calls
   `export_delegation_capability` with the exact signed vote-chain transaction
   bytes in bundle order.
3. Before broadcasting, the provider durably stores the opaque bytes returned
   by `VotingHotkey::stored_secret()`, the exact capability JSON and digest, and
   the exact signed delegation transaction bytes and hashes.
4. Over an authenticated, confidential API, the provider sends the customer
   the opaque hotkey secret and exact capability JSON. Delivery may happen
   while the provider broadcasts the delegation transactions.
5. The customer reconstructs the hotkey with
   `VotingHotkey::from_stored_secret`, then passes the exact JSON bytes and its
   independently trusted chain, network, and round context to
   `import_delegation_capability`.
6. Import verifies the hotkey address, recomputes every VAN, and commits the
   complete batch atomically. The returned SHA-256 digest covers the exact
   delivered bytes and can be acknowledged to the provider as a delivery
   receipt.
7. The customer uses each package `delegation_tx_hash` to query the existing
   transaction-status/event API. After confirmation, it records the public VAN
   leaf position, syncs the public vote tree, and uses the normal ZKP2 vote
   path.

The provider should support idempotent redelivery through round close. Voting
weight becomes unusable only if both parties lose the hotkey secret or
capability package; the customer's funds are not at risk.

## Capability format

Version 1 is canonical compact JSON:

```json
{"format_version":1,"vote_chain_id":"vote-chain","network":"mainnet","vote_round_id":"<64 lowercase hex>","raw_orchard_address":"<43 bytes, padded Base64>","bundles":[{"bundle_index":0,"num_ballots":10,"van_comm_rand":"<32 bytes, padded Base64>","delegation_tx_hash":"<64 lowercase hex>"}]}
```

The bundle list is complete, contiguous from index zero, and limited to 4,096
entries. The whole JSON document is limited to 1 MiB. `num_ballots` is the
quantized voting weight committed by the VAN; raw zatoshi remainders are not
part of the handoff.

The package contains no Zcash wallet seed, mnemonic, account spending key,
full viewing key, incoming viewing key, delegation proof, or raw transaction.
The opaque hotkey secret is delivered beside it and must stay out of logs,
analytics, crash reports, clipboard history, and unencrypted backups.

## Broadcast and acknowledgement

Durable provider storage before broadcast is required. Customer
acknowledgement is useful for delivery tracking, but is not a broadcast gate.
This avoids coupling the provider's on-chain deadline to customer availability
while preserving an exact-byte receipt for support and redelivery.
