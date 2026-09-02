# Local PIR + voting-config smoke harness for exercising this checkout against
# a sibling vote-nullifier-pir tree and a dummy hash-pinned config.
#
# Usage:
#   make pir-smoke
#
# Override any constant on the command line, e.g.:
#   make pir-smoke PIR_PORT=3010 PRESENT_NF_HEX=...

# ── Hardcoded constants (override via env / make args) ───────────────────────
PIR_REPO          ?= $(abspath $(ROOT)/../vote-nullifier-pir)
CONFIG_HOST       ?= 127.0.0.1
CONFIG_PORT       ?= 18080
PIR_HOST          ?= 127.0.0.1
PIR_PORT          ?= 13000
ZCASH_NETWORK     ?= main
PIR_POLY_LEN      ?= 4096

# Logical HTTPS identities embedded in the dummy config documents. The smoke
# driver fetches the corresponding paths from the local HTTP config server.
STATIC_IDENTITY_URL  ?= https://config.smoke.test/static-voting-config.json
DYNAMIC_IDENTITY_URL ?= https://config.smoke.test/dynamic-voting-config.json
PIR_IDENTITY_URL     ?= https://pir.smoke.test

# Little-endian 32-byte field elements (hex).
# PRESENT is written into the synthetic nullifiers.bin and must fail exclusion.
# ABSENT is a gap value and must return a verifying non-membership proof.
PRESENT_NF_HEX ?= 0700000000000000000000000000000000000000000000000000000000000000
ABSENT_NF_HEX  ?= 6400000000000000000000000000000000000000000000000000000000000000

# `help` is defined by the root Makefile, which greps every included file, so
# the pir-smoke target below is listed there too.
.PHONY: pir-smoke

pir-smoke: ## Resolve dummy config, serve local PIR, fetch present+absent nullifiers
	@PIR_REPO="$(PIR_REPO)" \
		CONFIG_HOST="$(CONFIG_HOST)" \
		CONFIG_PORT="$(CONFIG_PORT)" \
		PIR_HOST="$(PIR_HOST)" \
		PIR_PORT="$(PIR_PORT)" \
		ZCASH_NETWORK="$(ZCASH_NETWORK)" \
		PIR_POLY_LEN="$(PIR_POLY_LEN)" \
		STATIC_IDENTITY_URL="$(STATIC_IDENTITY_URL)" \
		DYNAMIC_IDENTITY_URL="$(DYNAMIC_IDENTITY_URL)" \
		PIR_IDENTITY_URL="$(PIR_IDENTITY_URL)" \
		PRESENT_NF_HEX="$(PRESENT_NF_HEX)" \
		ABSENT_NF_HEX="$(ABSENT_NF_HEX)" \
		ROOT="$(ROOT)" \
		bash "$(ROOT)/zcash_voting/tests/pir-smoke.sh"
