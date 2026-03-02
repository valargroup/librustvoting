# Voting Service Discovery

How Zashi discovers vote servers and PIR servers at runtime.

## Resolution order

1. **Local override** — `voting-config-local.json` bundled in the app (DEBUG builds only)
2. **CDN** — `https://zally-phi.vercel.app/api/voting-config` (served from Vercel Edge Config)
3. **Hardcoded fallback** — deployed dev server (`46.101.255.48`)

The first source that succeeds wins. This means a TestFlight build works out of the box (CDN or fallback), while a developer can drop a local file into the bundle to point at localhost.

## Config format

```json
{
  "version": 1,
  "vote_servers": [
    { "url": "https://46-101-255-48.sslip.io", "label": "Primary", "operator_address": "zvote1abc..." }
  ],
  "pir_servers": [
    { "url": "https://46-101-255-48.sslip.io/nullifier", "label": "PIR Server" }
  ]
}
```

**Important:** The JSON keys must be `vote_servers` and `pir_servers` — these map to `VotingServiceConfig.CodingKeys` in Swift. Using other key names (e.g. `nullifier_providers`) will cause silent decode failure, falling through to CDN/fallback.

The `operator_address` field is optional and used by the self-registration system to track which validator owns each entry. Swift `Codable` ignores unknown keys, so adding this field is backward-compatible with existing iOS builds.

`vote_servers` entries each serve the full set of endpoints — both chain API (`/zally/v1/*`) and helper API (`/api/v1/shares`). This is because the SDK and helper server are a single merged binary.

`pir_servers` serve the PIR nullifier exclusion proof protocol (port 3000 by default).

## Self-registration

Validators can register their URL with a single command via `join.sh`. The registration flow has two phases:

**Phase 1 (not yet bonded):** `join.sh` signs a registration payload with the validator's operator key and POSTs it to `/api/register-validator`. Since the validator isn't bonded yet, the entry goes into a `pending-registrations` queue (7-day expiry). The vote-manager sees pending registrations in the admin UI and clicks "Approve & Fund" to move the URL to `vote_servers` and send stake in one action.

**Phase 2 (bonded):** After `join.sh` registers the validator on-chain (via `create-val-tx`), it re-registers with the same endpoint. This time the edge function detects the validator is bonded and promotes the URL directly to `vote_servers` — no admin approval needed.

Both phases use the same endpoint (`POST /api/register-validator`) and the same ADR-036 amino signature format. The edge function decides the path based on on-chain bonding status.

The `zallyd sign-arbitrary` command provides the signature:
```bash
zallyd sign-arbitrary '{"operator_address":"...","url":"...","moniker":"...","timestamp":...}' \
  --from validator --keyring-backend test --home ~/.zallyd
```

## Local testing

`mise start` and `mise run multi:start` automatically write `secant/Resources/voting-config-local.json` with the correct ports for the mode being started. The file is gitignored and only bundled in DEBUG builds (via an Xcode build phase), taking priority over CDN.

| Mode             | Chain REST port | Command                | Auto-written? |
| ---------------- | --------------- | ---------------------- | ------------- |
| Single validator | 1318            | `mise start`           | Yes           |
| Multi validator  | 1418            | `mise run multi:start` | Yes           |

Whichever mode you start last wins, which is correct since you can only test against one chain at a time.

To manually override, edit the file directly — it won't be overwritten until the next `mise start` or `multi:start`.

## Where the URLs flow

```
VotingStore.initialize
  → votingAPI.fetchServiceConfig()        // resolves config per order above
  → votingAPI.configureURLs(config)       // sets ZallyAPIConfigStore actor
  → all subsequent API calls use resolved URLs
```

The resolved config is also stored in `VotingStore.State.serviceConfig` so the store can read URLs for the IMT server and chain node directly (used by `votingCrypto.syncVoteTree` and delegation proof).

## Share distribution

When multiple vote servers are configured, encrypted shares are distributed across them instead of all going to one server. With N servers and 5 shares:

- N >= 5: one share per server (shuffled)
- 1 < N < 5: round-robin
- N == 1: all shares to that server

## Config deployment

The config is served from **Vercel Edge Config**, a key-value store that can be updated instantly without redeployment. The edge function at `shielded_vote_generator_ui/api/voting-config.ts` reads the `voting-config` key and returns it as JSON.

### Updating the config

1. **Admin UI** (primary): Register/remove validator URLs via the Validators panel, or approve pending self-registrations
2. **Self-registration**: `join.sh` auto-registers via `POST /api/register-validator` (admin approves in UI, or auto-promoted after bonding)
3. **Vercel Dashboard**: Go to the project's Edge Config store → edit the `voting-config` key directly
4. **Vercel CLI**: `vercel edge-config items update voting-config --value '{"version":1,...}'`

Changes take effect immediately — no git push or redeploy needed. This is useful for demos where you spin up new servers and want TestFlight builds to pick them up right away.

### Setup (one-time)

1. In the Vercel dashboard, go to **Storage** → **Create** → **Edge Config**
2. Connect it to the `zally` project
3. Add a key `voting-config` with the JSON value:
   ```json
   {
     "version": 1,
     "vote_servers": [
       { "url": "https://46-101-255-48.sslip.io", "label": "Primary", "operator_address": "zvote1..." }
     ],
     "pir_servers": [
       { "url": "https://46-101-255-48.sslip.io/nullifier", "label": "Primary" }
     ]
   }
   ```
4. Vercel auto-sets the `EDGE_CONFIG` env var on the project
