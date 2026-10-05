# VPush. From the repository root the same targets are `make vpush-<target>`.

.PHONY: build test check run release build-deploy deploy ftp rollback bootstrap trust status logs ctl server-ctl test-deploy third-party clean

SHELL := /bin/bash
ENV := source scripts/env.sh &&

# Debug build.
build:
	@$(ENV) cargo build

test:
	@$(ENV) cargo test

check:
	@$(ENV) cargo clippy --all-targets -- -D warnings

# Runs the server here. The first run writes vpush.toml next to this file.
run:
	@[ -f vpush.toml ] || sed -e 's|https://push.example.org|http://localhost:8090|' \
		-e 's|/run/vpush/admin.sock|/tmp/vpush-dev.sock|' deploy/vpush.example.toml > vpush.toml
	@$(ENV) cargo run -q -p vpush -- serve --config vpush.toml

# vpush ctl against `make run`:  make ctl ARGS="log set relay=debug --for 15m"
ctl:
	@$(ENV) cargo run -q -p vpush -- ctl --config vpush.toml $(ARGS)

# Static binary in dist/.
release:
	@bash scripts/build-release.sh

# Static binary copied into the bin folder of a server kit, which installs it
# from there:  make build-deploy DEPLOY_BIN=<folder>
build-deploy: release
	@if [ -z "$(DEPLOY_BIN)" ]; then echo ">> make build-deploy DEPLOY_BIN=<folder>"; exit 1; fi
	@mkdir -p $(DEPLOY_BIN)
	@cp dist/vpush dist/vpush.sha256 dist/RELEASE $(DEPLOY_BIN)/
	@echo ">> $(DEPLOY_BIN)/vpush  $$(cat dist/RELEASE)"

# Build, send to the server, switch, check; rolls back by itself on failure.
deploy:
	@bash scripts/deploy.sh

ftp: deploy

rollback:
	@bash scripts/rollback.sh

# Once per machine that deploys: records the server's key, so that every
# other command talks to that server and no other.
trust:
	@bash scripts/trust.sh

# Once per server: user, directories, systemd unit, first config.
bootstrap:
	@bash scripts/bootstrap-server.sh

status:
	@bash scripts/server.sh status

# make logs            follow
# make logs ARGS="-n 500 --since -1h"
logs:
	@bash scripts/server.sh logs $(ARGS)

# vpush ctl on the server:  make server-ctl ARGS="log set relay=debug --for 15m"
server-ctl:
	@bash scripts/server.sh ctl $(ARGS)

# THIRD-PARTY-LICENSES.md from Cargo.lock: run it after the lock changes.
third-party:
	@$(ENV) node scripts/third-party.mjs

clean:
	@rm -rf target dist

# The deploy and rollback scripts, against a directory instead of a server.
test-deploy:
	@bash scripts/test-deploy.sh
