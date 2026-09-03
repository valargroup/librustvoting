ROOT := $(abspath $(dir $(lastword $(MAKEFILE_LIST))))

# Canonical build and test targets. Run `make help` for the list.
include $(ROOT)/make/build.mk

# Local PIR + voting-config smoke harness.
include $(ROOT)/zcash_voting/tests/Makefile
