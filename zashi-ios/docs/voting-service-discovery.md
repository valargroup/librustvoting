# Voting Service Discovery

How Zashi discovers vote servers and nullifier providers at runtime.

## Resolution order

1. **Local override** — `voting-config-local.json` bundled in the app
2. **CDN** — `https://zally-phi.vercel.app/api/voting-config` (served from Vercel Edge Config)
3. **Hardcoded fallback** — deployed dev server (`46.101.255.48`)

The first source that succeeds wins. This means a TestFlight build works out of the box (CDN or fallback), while a developer can drop a local file into the bundle to point at localhost.

## Config format

```json
{
  "version": 1,
  "vote_servers": [
    { "url": "https://46-101-255-48.sslip.io", "label": "Primary" }
  ],
  "nullifier_providers": [
    { "url": "https://46-101-255-48.sslip.io/nullifier", "label": "Primary" }
  ]
}
```

`vote_servers` entries each serve the full set of endpoints — both chain API (`/zally/v1/*`) and helper API (`/api/v1/shares`). This is because the SDK and helper server are a single merged binary.

`nullifier_providers` serve `/nullifier/proof/{nullifier_hex}` for IMT non-membership proofs.

## Local testing

Create `voting-config-local.json` in the Xcode project (add to the app target so it's copied into the bundle):

```json
{
  "version": 1,
  "vote_servers": [
    { "url": "http://localhost:1318", "label": "Localhost" }
  ],
  "nullifier_providers": [
    { "url": "http://localhost:3000", "label": "Localhost" }
  ]
}
```

This file is checked before the CDN fetch, so it always takes priority. Don't commit it — it's for local dev only.

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

1. **Vercel Dashboard**: Go to the project's Edge Config store → edit the `voting-config` key
2. **Vercel CLI**: `vercel edge-config items update voting-config --value '{"version":1,...}'`
3. **REST API**: `PATCH https://api.vercel.com/v1/edge-config/{id}/items` with your Vercel token

Changes take effect immediately — no git push or redeploy needed. This is useful for demos where you spin up new servers and want TestFlight builds to pick them up right away.

### Setup (one-time)

1. In the Vercel dashboard, go to **Storage** → **Create** → **Edge Config**
2. Connect it to the `zally` project
3. Add a key `voting-config` with the JSON value:
   ```json
   {
     "version": 1,
     "vote_servers": [
       { "url": "https://46-101-255-48.sslip.io", "label": "Primary" }
     ],
     "nullifier_providers": [
       { "url": "https://46-101-255-48.sslip.io/nullifier", "label": "Primary" }
     ]
   }
   ```
4. Vercel auto-sets the `EDGE_CONFIG` env var on the project
