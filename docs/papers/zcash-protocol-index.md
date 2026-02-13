# Zcash Protocol Spec — Section Index
Source: `docs/papers/zcash-protocol.tex` (from zcash/zips repo)

## How to use
Read this index to find relevant line ranges, then read those lines from zcash-protocol.tex.
The first ~2700 lines are LaTeX macro definitions (useful for understanding notation).

## Key Topics

### Notes & Commitments
- Notes (concept): lines 3303–3418
- Note plaintexts & memo fields: lines 3418–3507
- Note commitments: lines 3507–3598
- Sending notes (Sprout): lines 5863–5935
- Sending notes (Sapling): lines 5935–6046
- Sending notes (Orchard): lines 6046–6139
- Dummy notes: lines 6139–6326

### Nullifiers
- Nullifiers (concept): lines 3598–3640
- Nullifier sets: lines 3976–3993
- Computing nullifiers: lines 7022–7121

### Keys & Addresses
- Payment addresses and keys: lines 3192–3303
- Sprout key components: lines 5157–5186
- Sapling key components: lines 5186–5355
- Orchard key components: lines 5355–5519
- Address encodings: lines 11997–12642

### Transactions
- Transactions and treestates: lines 3687–3766
- JoinSplit descriptions: lines 5519–5603
- Spend descriptions: lines 5603–5677
- Output descriptions: lines 5677–5751
- Action descriptions: lines 5751–5860
- Transaction encoding & consensus: lines 12807–13287

### Merkle Trees
- Note commitment trees: lines 3929–3976
- Merkle path validity: lines 6326–6416

### zk-SNARKs & Proofs
- Zero-knowledge proving system: lines 5018–5154
- JoinSplit statement (Sprout): lines 7214–7303
- Spend statement (Sapling): lines 7303–7427
- Output statement (Sapling): lines 7427–7503
- Action statement (Orchard): lines 7503–7692

### Encryption & In-band Secret Distribution
- Symmetric encryption: lines 4332–4353
- Key agreement: lines 4353–4388
- In-band secret distribution (Sprout): lines 7692–7840
- In-band secret distribution (Sapling/Orchard): lines 7840–8240

### Signatures & Balance
- Signature schemes: lines 4450–4712
- Balance & binding signature (Sapling): lines 6566–6772
- Balance & binding signature (Orchard): lines 6772–6944
- Spend authorization signature: lines 6944–7022

### Commitment Schemes
- Abstract commitments: lines 4712–4854
- Concrete commitments: lines 10475–10820

### Concrete Cryptographic Schemes
- Hash functions: lines 8538–9561
- PRFs: lines 9561–9802
- RedDSA/RedJubjub/RedPallas: lines 10189–10475

### Circuits
- Circuit design: lines 17672–17800
- Circuit components: lines 17800–18992
- Sapling Spend circuit: lines 18992–19173
- Sapling Output circuit: lines 19173–19277

### Other
- Notation: lines 2972–3190
- Block chain: lines 3640–3687
- Network upgrades: lines 12711–12804
- Proof of work: lines 13733–14022
- Differences from Zerocash paper: lines 14535–15182
