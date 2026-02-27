#!/bin/bash
# join.sh — Join the Zally chain as a validator.
#
# Binary-only (no repo):
#   curl -fsSL https://vote.fra1.digitaloceanspaces.com/join.sh | bash
#
# Source developer (has repo + mise):
#   mise run build:install   # builds zallyd + create-val-tx → $HOME/go/bin
#   ./join.sh                # detects local binaries, skips download
#
# What it does:
#   1. Acquires zallyd + create-val-tx (downloads if not in PATH, else uses local)
#   2. Discovers the network via the Vercel API (voting-config endpoint)
#   3. Fetches genesis.json + node identity from a live validator
#   4. Initializes a node, generates cryptographic keys
#   5. Configures the node to connect to the existing network
#   6. Generates a start.sh script that handles sync + validator registration
#
# Requirements: curl, jq
# Dependency (binary path): release.yml must have run to upload binaries to DO Spaces.

set -euo pipefail

CHAIN_ID="zvote-1"
INSTALL_DIR="${ZALLY_INSTALL_DIR:-$HOME/.local/bin}"
HOME_DIR="${ZALLY_HOME:-$HOME/.zallyd}"
DO_BASE="https://vote.fra1.digitaloceanspaces.com"
VOTING_CONFIG_URL="${VOTING_CONFIG_URL:-https://zally-phi.vercel.app}"

# Parse --domain flag for TLS hostname override.
ZALLY_DOMAIN="${ZALLY_DOMAIN:-}"
while [ $# -gt 0 ]; do
  case "$1" in
    --domain) ZALLY_DOMAIN="$2"; shift 2 ;;
    --domain=*) ZALLY_DOMAIN="${1#--domain=}"; shift ;;
    *) shift ;;
  esac
done

# ─── Preflight ────────────────────────────────────────────────────────────────

echo "=== Zally validator join ==="
echo ""

for cmd in curl jq; do
  if ! command -v "$cmd" > /dev/null 2>&1; then
    echo "ERROR: $cmd is required. Install it and re-run."
    exit 1
  fi
done

# ─── Prompt for moniker ──────────────────────────────────────────────────────

if [ -n "${ZALLY_MONIKER:-}" ]; then
  MONIKER="$ZALLY_MONIKER"
else
  printf "Enter a name for your validator: "
  read -r MONIKER < /dev/tty
  if [ -z "$MONIKER" ]; then
    echo "ERROR: Moniker cannot be empty."
    exit 1
  fi
fi

# ─── Acquire binaries ────────────────────────────────────────────────────────
# If zallyd and create-val-tx are already in PATH (e.g. from `mise run build:install`),
# skip the download. Otherwise fetch pre-built binaries from DO Spaces.

if command -v zallyd > /dev/null 2>&1 && command -v create-val-tx > /dev/null 2>&1; then
  echo "Using local binaries:"
  echo "  zallyd:         $(command -v zallyd)"
  echo "  create-val-tx:  $(command -v create-val-tx)"
else
  echo "=== Downloading binaries ==="

  OS_RAW=$(uname -s)
  ARCH_RAW=$(uname -m)

  case "$OS_RAW" in
    Linux)  OS="linux" ;;
    Darwin) OS="darwin" ;;
    *)
      echo "ERROR: Unsupported OS: ${OS_RAW}. Supported: Linux, Darwin (macOS)."
      exit 1
      ;;
  esac

  case "$ARCH_RAW" in
    x86_64)          ARCH="amd64" ;;
    aarch64|arm64)   ARCH="arm64" ;;
    *)
      echo "ERROR: Unsupported architecture: ${ARCH_RAW}. Supported: x86_64, arm64/aarch64."
      exit 1
      ;;
  esac

  PLATFORM="${OS}-${ARCH}"

  mkdir -p "${INSTALL_DIR}"

  VERSION=$(curl -fsSL "${DO_BASE}/version.txt" | tr -d '[:space:]')
  if [ -z "$VERSION" ]; then
    echo "ERROR: Could not fetch version from ${DO_BASE}/version.txt"
    exit 1
  fi

  echo "Version: ${VERSION}"
  echo "Platform: ${PLATFORM}"
  curl -fsSL -o /tmp/zally-release.tar.gz "${DO_BASE}/zally-${VERSION}-${PLATFORM}.tar.gz"

  # Verify tarball integrity via SHA-256 checksum.
  CHECKSUM_URL="${DO_BASE}/zally-${VERSION}-${PLATFORM}.tar.gz.sha256"
  if curl -fsSL -o /tmp/zally-release.tar.gz.sha256 "${CHECKSUM_URL}" 2>/dev/null; then
    EXPECTED=$(awk '{print $1}' /tmp/zally-release.tar.gz.sha256)
    if command -v sha256sum > /dev/null 2>&1; then
      ACTUAL=$(sha256sum /tmp/zally-release.tar.gz | awk '{print $1}')
    elif command -v shasum > /dev/null 2>&1; then
      ACTUAL=$(shasum -a 256 /tmp/zally-release.tar.gz | awk '{print $1}')
    else
      echo "WARNING: Neither sha256sum nor shasum found — skipping checksum verification."
      ACTUAL="$EXPECTED"
    fi
    if [ "$ACTUAL" != "$EXPECTED" ]; then
      echo "ERROR: Checksum mismatch!"
      echo "  Expected: ${EXPECTED}"
      echo "  Actual:   ${ACTUAL}"
      echo "  The downloaded tarball may be corrupted or tampered with."
      rm -f /tmp/zally-release.tar.gz /tmp/zally-release.tar.gz.sha256
      exit 1
    fi
    echo "Checksum verified."
    rm -f /tmp/zally-release.tar.gz.sha256
  else
    echo "WARNING: Checksum file not available — skipping verification."
  fi

  TARBALL_DIR="zally-${VERSION}-${PLATFORM}"
  tar xzf /tmp/zally-release.tar.gz -C /tmp "${TARBALL_DIR}/bin/zallyd" "${TARBALL_DIR}/bin/create-val-tx"

  cp "/tmp/${TARBALL_DIR}/bin/zallyd" "${INSTALL_DIR}/zallyd"
  cp "/tmp/${TARBALL_DIR}/bin/create-val-tx" "${INSTALL_DIR}/create-val-tx"
  chmod +x "${INSTALL_DIR}/zallyd" "${INSTALL_DIR}/create-val-tx"
  rm -rf /tmp/zally-release.tar.gz "/tmp/${TARBALL_DIR}"

  hash -r
  echo "Installed: ${INSTALL_DIR}/zallyd, ${INSTALL_DIR}/create-val-tx"
fi

# Ensure install dir is on PATH for this session and the generated start.sh.
case ":${PATH}:" in
  *":${INSTALL_DIR}:"*) ;;
  *) export PATH="${INSTALL_DIR}:${PATH}" ;;
esac

# ─── Discover network via Vercel API ─────────────────────────────────────────
# The voting-config endpoint returns the same data iOS clients use for service
# discovery. We pick a vote_server, fetch its node identity, and use it as our
# initial peer. CometBFT's PEX handles further peer discovery.

echo ""
echo "=== Discovering network ==="

VOTING_CONFIG=$(curl -fsSL "${VOTING_CONFIG_URL}/api/voting-config")
SEED_URL=$(echo "$VOTING_CONFIG" | jq -r '.vote_servers[0].url')

if [ -z "$SEED_URL" ] || [ "$SEED_URL" = "null" ]; then
  echo "ERROR: No vote_servers found in voting-config at ${VOTING_CONFIG_URL}/api/voting-config"
  echo "  The bootstrap operator needs to register at least one validator URL in the admin UI."
  exit 1
fi

echo "Seed node: ${SEED_URL}"

# Fetch the node's P2P identity.
NODE_INFO=$(curl -fsSL "${SEED_URL}/cosmos/base/tendermint/v1beta1/node_info")
NODE_ID=$(echo "$NODE_INFO" | jq -r '.default_node_info.default_node_id // .default_node_info.id // empty')
LISTEN_ADDR=$(echo "$NODE_INFO" | jq -r '.default_node_info.listen_addr // empty')

if [ -z "$NODE_ID" ]; then
  echo "ERROR: Could not fetch node_id from ${SEED_URL}"
  exit 1
fi

# Extract the host from the seed URL for the P2P address.
# The REST API URL has a host; P2P uses port 26656.
SEED_HOST=$(echo "$SEED_URL" | sed -E 's|^https?://||; s|:[0-9]+$||; s|/.*||')
PERSISTENT_PEERS="${NODE_ID}@${SEED_HOST}:26656"
echo "Peers: ${PERSISTENT_PEERS}"

# ─── Initialize node ─────────────────────────────────────────────────────────

echo ""
echo "=== Initializing node ==="

# Clean previous state if present.
if [ -d "${HOME_DIR}" ]; then
  echo "Removing existing ${HOME_DIR}..."
  rm -rf "${HOME_DIR}"
fi

zallyd init "${MONIKER}" --chain-id "${CHAIN_ID}" --home "${HOME_DIR}" > /dev/null 2>&1

# ─── Fetch genesis from the seed node ────────────────────────────────────────

echo "Fetching genesis.json from ${SEED_URL}..."
curl -fsSL -o "${HOME_DIR}/config/genesis.json" "${SEED_URL}/zally/v1/genesis"
zallyd genesis validate-genesis --home "${HOME_DIR}" > /dev/null 2>&1
echo "Genesis validated."

# ─── Generate keys ────────────────────────────────────────────────────────────

echo ""
echo "=== Generating cryptographic keys ==="

zallyd init-validator-keys --home "${HOME_DIR}"

VALIDATOR_ADDR=$(zallyd keys show validator -a --keyring-backend test --home "${HOME_DIR}")

# ─── Configure config.toml ───────────────────────────────────────────────────

echo ""
echo "=== Configuring node ==="

CONFIG_TOML="${HOME_DIR}/config/config.toml"

# Set persistent peers.
sed -i.bak "s|persistent_peers = \"\"|persistent_peers = \"${PERSISTENT_PEERS}\"|" "${CONFIG_TOML}"

# Increase broadcast timeout for ZKP verification (~30-60s).
sed -i.bak 's/^timeout_broadcast_tx_commit = .*/timeout_broadcast_tx_commit = "120s"/' "${CONFIG_TOML}"

rm -f "${CONFIG_TOML}.bak"

# ─── Configure app.toml ──────────────────────────────────────────────────────

APP_TOML="${HOME_DIR}/config/app.toml"

# Enable REST API with CORS.
sed -i.bak '/\[api\]/,/\[.*\]/ s/enable = false/enable = true/' "${APP_TOML}"
sed -i.bak '/\[api\]/,/\[.*\]/ s/enabled-unsafe-cors = false/enabled-unsafe-cors = true/' "${APP_TOML}"

# Fix [vote] paths (template uses literal $HOME, replace with actual).
sed -i.bak "s|\\\$HOME/.zallyd|${HOME_DIR}|g" "${APP_TOML}"

rm -f "${APP_TOML}.bak"

# Append [helper] section (not in the default template).
cat >> "${APP_TOML}" <<HELPERCFG

###############################################################################
###                         Helper Server                                   ###
###############################################################################

[helper]

# Set to true to disable the helper server.
disable = false

# Optional auth token for POST /api/v1/shares (sent via X-Helper-Token header).
# Empty disables token auth.
api_token = ""

# Path to the SQLite database file. Empty = default (\$HOME/.zallyd/helper.db).
db_path = ""

# Mean of the exponential delay distribution (seconds).
# Shares are delayed by Exp(1/mean) for temporal unlinkability, capped at vote end time.
# Use a short value for testing; production default is 43200 (12 hours).
mean_delay = 60

# How often to check for shares ready to submit (seconds).
process_interval = 5

# Port of the chain's REST API (used for MsgRevealShare submission).
chain_api_port = 1317

# Maximum concurrent proof generation goroutines.
max_concurrent_proofs = 2
HELPERCFG

echo "Node configured."

# ─── TLS reverse proxy (Caddy) ──────────────────────────────────────────────
# Sets up Caddy as a TLS reverse proxy in front of the chain REST API (port 1418).
# Caddy auto-provisions Let's Encrypt certificates.

echo ""
echo "=== Setting up TLS reverse proxy ==="

if [ -z "$ZALLY_DOMAIN" ]; then
  # Auto-detect public IP and use sslip.io for a valid TLS hostname.
  PUBLIC_IP=$(curl -fsSL --connect-timeout 5 https://ifconfig.me 2>/dev/null || echo "")
  if [ -z "$PUBLIC_IP" ]; then
    echo "WARNING: Could not detect public IP. Skipping Caddy setup."
    echo "  Re-run with --domain <hostname> or set ZALLY_DOMAIN to configure TLS."
    VALIDATOR_URL=""
  else
    ZALLY_DOMAIN="$(echo "$PUBLIC_IP" | tr '.' '-').sslip.io"
    echo "Detected public IP: ${PUBLIC_IP}"
    echo "Using sslip.io domain: ${ZALLY_DOMAIN}"
  fi
fi

if [ -n "$ZALLY_DOMAIN" ]; then
  VALIDATOR_URL="https://${ZALLY_DOMAIN}"

  # Install Caddy if not present.
  if ! command -v caddy > /dev/null 2>&1; then
    OS_NAME=$(uname -s)
    if [ "$OS_NAME" = "Linux" ]; then
      echo "Installing Caddy..."
      if command -v apt-get > /dev/null 2>&1; then
        export DEBIAN_FRONTEND=noninteractive NEEDRESTART_MODE=a
        sudo -E apt-get install -y debian-keyring debian-archive-keyring apt-transport-https curl > /dev/null 2>&1
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/gpg.key' | sudo gpg --dearmor -o /usr/share/keyrings/caddy-stable-archive-keyring.gpg 2>/dev/null
        curl -1sLf 'https://dl.cloudsmith.io/public/caddy/stable/debian.deb.txt' | sudo tee /etc/apt/sources.list.d/caddy-stable.list > /dev/null
        sudo -E apt-get update > /dev/null 2>&1
        sudo -E apt-get install -y caddy > /dev/null 2>&1
      else
        echo "WARNING: apt not found. Install Caddy manually: https://caddyserver.com/docs/install"
        VALIDATOR_URL=""
      fi
    else
      echo "WARNING: Automatic Caddy installation is only supported on Linux (apt)."
      echo "  Install Caddy manually: https://caddyserver.com/docs/install"
      echo "  Then re-run join.sh."
      VALIDATOR_URL=""
    fi
  fi
fi

if [ -n "$VALIDATOR_URL" ] && command -v caddy > /dev/null 2>&1; then
  # Write Caddyfile.
  echo "Configuring Caddy for ${ZALLY_DOMAIN} → localhost:1418..."
  sudo tee /etc/caddy/Caddyfile > /dev/null <<CADDYEOF
${ZALLY_DOMAIN} {
    reverse_proxy localhost:1418
}
CADDYEOF

  sudo systemctl restart caddy 2>/dev/null || sudo caddy reload --config /etc/caddy/Caddyfile 2>/dev/null || true
  echo "Caddy configured: ${VALIDATOR_URL} → localhost:1418"
else
  VALIDATOR_URL=""
fi

# ─── Phase 1: Register as pending validator ─────────────────────────────────
# The validator exists (keys generated) but isn't bonded yet. Register with
# the Vercel API so the vote-manager can see and approve it.

if [ -n "$VALIDATOR_URL" ]; then
  echo ""
  echo "=== Registering with vote network ==="

  TIMESTAMP=$(date +%s)
  REG_PAYLOAD="{\"operator_address\":\"${VALIDATOR_ADDR}\",\"url\":\"${VALIDATOR_URL}\",\"moniker\":\"${MONIKER}\",\"timestamp\":${TIMESTAMP}}"

  if SIG_JSON=$(zallyd sign-arbitrary "$REG_PAYLOAD" --from validator --keyring-backend test --home "${HOME_DIR}" 2>/dev/null); then
    SIG=$(echo "$SIG_JSON" | jq -r '.signature')
    PUB_KEY=$(echo "$SIG_JSON" | jq -r '.pub_key')

    REG_BODY="{\"operator_address\":\"${VALIDATOR_ADDR}\",\"url\":\"${VALIDATOR_URL}\",\"moniker\":\"${MONIKER}\",\"timestamp\":${TIMESTAMP},\"signature\":\"${SIG}\",\"pub_key\":\"${PUB_KEY}\"}"

    REG_RESULT=$(curl -fsSL -X POST "${VOTING_CONFIG_URL}/api/register-validator" \
      -H "Content-Type: application/json" \
      -d "$REG_BODY" 2>/dev/null || echo "")

    if [ -n "$REG_RESULT" ]; then
      REG_STATUS=$(echo "$REG_RESULT" | jq -r '.status // empty' 2>/dev/null || echo "")
      if [ "$REG_STATUS" = "pending" ] || [ "$REG_STATUS" = "registered" ]; then
        echo "Registered (${REG_STATUS}). The vote-manager will see your request."
      else
        echo "WARNING: Registration response: ${REG_RESULT}"
      fi
    else
      echo "WARNING: Could not reach registration API. You can register manually later:"
      echo "  zallyd sign-arbitrary '<payload>' --from validator --keyring-backend test --home ${HOME_DIR}"
    fi
  else
    echo "WARNING: Could not sign registration payload. You can register manually later."
  fi
fi

# ─── Generate start.sh ───────────────────────────────────────────────────────

START_SCRIPT="${HOME_DIR}/start.sh"

cat > "${START_SCRIPT}" <<STARTEOF
#!/bin/bash
# start.sh — Start the node, wait for sync, and register as a validator.
# Generated by join.sh for moniker "${MONIKER}".
set -euo pipefail

HOME_DIR="${HOME_DIR}"
INSTALL_DIR="${INSTALL_DIR}"
MONIKER="${MONIKER}"
VALIDATOR_ADDR="${VALIDATOR_ADDR}"
VALIDATOR_URL="${VALIDATOR_URL}"
VOTING_CONFIG_URL="${VOTING_CONFIG_URL}"
LOG_FILE="\${HOME_DIR}/node.log"

# Ensure binaries are on PATH.
case ":\${PATH}:" in
  *":\${INSTALL_DIR}:"*) ;;
  *) export PATH="\${INSTALL_DIR}:\${PATH}" ;;
esac

# Re-register URL with the vote network (idempotent).
# Once bonded, this promotes the pending entry to vote_servers directly.
register_url() {
  if [ -z "\${VALIDATOR_URL}" ] || [ -z "\${VOTING_CONFIG_URL}" ]; then
    return 0
  fi
  local ts=\$(date +%s)
  local payload="{\"operator_address\":\"\${VALIDATOR_ADDR}\",\"url\":\"\${VALIDATOR_URL}\",\"moniker\":\"\${MONIKER}\",\"timestamp\":\${ts}}"
  local sig_json
  sig_json=\$(zallyd sign-arbitrary "\$payload" --from validator --keyring-backend test --home "\${HOME_DIR}" 2>/dev/null) || return 0
  local sig=\$(echo "\$sig_json" | jq -r '.signature')
  local pub_key=\$(echo "\$sig_json" | jq -r '.pub_key')
  local body="{\"operator_address\":\"\${VALIDATOR_ADDR}\",\"url\":\"\${VALIDATOR_URL}\",\"moniker\":\"\${MONIKER}\",\"timestamp\":\${ts},\"signature\":\"\${sig}\",\"pub_key\":\"\${pub_key}\"}"
  curl -fsSL -X POST "\${VOTING_CONFIG_URL}/api/register-validator" \
    -H "Content-Type: application/json" \
    -d "\$body" > /dev/null 2>&1 || true
}

echo "Starting zallyd..."
echo "Logs: \${LOG_FILE}"
zallyd start --home "\${HOME_DIR}" >> "\${LOG_FILE}" 2>&1 &
ZALLYD_PID=\$!

trap "echo ''; echo 'zallyd is still running in the background (PID: \${ZALLYD_PID}).'; echo \"Stop it with: kill \${ZALLYD_PID}\"; echo \"Logs: \${LOG_FILE}\"; exit 0" INT TERM

# Give the node a moment to start up.
sleep 5

echo "Waiting for node to sync..."
echo "  (follow logs with: tail -f \${LOG_FILE})"
while true; do
  STATUS=\$(zallyd status --home "\${HOME_DIR}" 2>/dev/null || echo "")
  if [ -z "\$STATUS" ]; then
    sleep 2
    continue
  fi

  CATCHING_UP=\$(echo "\$STATUS" | jq -r '.sync_info.catching_up' 2>/dev/null || echo "true")
  HEIGHT=\$(echo "\$STATUS" | jq -r '.sync_info.latest_block_height' 2>/dev/null || echo "0")
  echo "  height: \${HEIGHT}, catching_up: \${CATCHING_UP}"

  if [ "\$CATCHING_UP" = "false" ]; then
    echo "Node is synced."
    break
  fi
  sleep 5
done

# Check if already a validator.
IS_VALIDATOR=\$(zallyd query staking validators --home "\${HOME_DIR}" --output json 2>/dev/null \
  | jq -r ".validators[] | select(.description.moniker == \"\${MONIKER}\") | .operator_address" 2>/dev/null || echo "")

if [ -n "\$IS_VALIDATOR" ]; then
  echo "Already registered as validator: \${IS_VALIDATOR}"
  # Re-register URL on restart (idempotent — ensures vote_servers is up to date).
  register_url
else
  # Wait for the account to be funded before attempting registration.
  echo "Waiting for account \${VALIDATOR_ADDR} to be funded..."
  echo "  (ask the bootstrap operator to fund your address in the admin UI)"
  while true; do
    BALANCE=\$(zallyd query bank balances "\${VALIDATOR_ADDR}" --home "\${HOME_DIR}" --output json 2>/dev/null \
      | jq -r '.balances[] | select(.denom == "uzvote") | .amount' 2>/dev/null || echo "")
    if [ -n "\$BALANCE" ] && [ "\$BALANCE" != "0" ]; then
      echo "  Account funded (\${BALANCE} uzvote)."
      break
    fi
    sleep 5
  done

  echo "Registering as validator..."
  if ! create-val-tx --moniker "\${MONIKER}" --amount 10000000uzvote --home "\${HOME_DIR}" --rpc-url tcp://localhost:26657; then
    echo ""
    echo "ERROR: create-val-tx exited with a non-zero status." >&2
    echo "  Check node logs for details: \${LOG_FILE}" >&2
    exit 1
  fi

  # Verify the validator actually appeared on-chain rather than assuming success.
  echo "Verifying registration on-chain (waiting ~6s for block commit)..."
  sleep 6
  IS_NOW_VALIDATOR=\$(zallyd query staking validators --home "\${HOME_DIR}" --output json 2>/dev/null \
    | jq -r ".validators[] | select(.description.moniker == \"\${MONIKER}\") | .operator_address" 2>/dev/null || echo "")
  if [ -z "\${IS_NOW_VALIDATOR}" ]; then
    echo ""
    echo "ERROR: Validator registration failed — '\${MONIKER}' not found in the validator set." >&2
    echo "  Check node logs for details: \${LOG_FILE}" >&2
    exit 1
  fi
  echo "Validator registered: \${IS_NOW_VALIDATOR}"
  # Re-register now that we're bonded — promotes to vote_servers.
  register_url
fi

echo ""
echo "Node is running (PID: \${ZALLYD_PID}). Logs: \${LOG_FILE}"
echo "Press Ctrl+C to detach (node keeps running). To stop: kill \${ZALLYD_PID}"
wait \$ZALLYD_PID
STARTEOF

chmod +x "${START_SCRIPT}"

# ─── Summary ──────────────────────────────────────────────────────────────────

echo ""
echo "============================================="
echo "       Node initialized successfully"
echo "============================================="
echo ""
echo "  Moniker:  ${MONIKER}"
echo "  Home:     ${HOME_DIR}"
echo "  Address:  ${VALIDATOR_ADDR}"
echo ""

echo "=== Next steps ==="
echo ""
if [ -n "$VALIDATOR_URL" ]; then
  echo "1. Your registration request has been sent to the vote-manager."
  echo "   They will approve and fund your account in the admin UI."
  echo ""
  echo "2. Once approved, start your node (syncs and registers automatically):"
else
  echo "1. Fund your account. Ask the vote-manager to approve your"
  echo "   registration in the admin UI, or send uzvote to:"
  echo "   ${VALIDATOR_ADDR}"
  echo ""
  echo "2. Once funded, start your node (syncs and registers automatically):"
fi
echo "   ${START_SCRIPT}"
echo ""
echo "   Then follow node logs with:"
echo "   tail -f ${HOME_DIR}/node.log"
echo ""
