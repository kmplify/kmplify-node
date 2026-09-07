# Local development entry points for kmplify-node. `make` (or `make help`)
# lists them. Every target wraps the exact cargo, script or packaging
# command the docs and CI use, so nothing here is a second way of doing
# things; it is the same way with the environment filled in.
#
# Needs GNU make and a POSIX shell (macOS, Linux; on Windows use WSL or
# Git Bash). Rust 1.95+ via rustup.
#
# Knobs, all overridable on the command line (`make run GATEWAY=...`):
#
#   NODE_DIR   where the dev node keeps identity, settings and state.
#              Defaults to ./.kmplify-node (gitignored), never your real
#              ~/.config/kmplify-node, so experiments cannot touch it.
#   GATEWAY    the fabric to join. Defaults to a compute-fabric gateway
#              running locally (kmplify-compute-fabric, `FABRIC_PORT`
#              18100); the public fabric is https://fabric.kmplify.io.
#   OLLAMA     the local engine (OLLAMA_BASE).
#   FEATURES   cargo features for the run targets; router/gui targets
#              add what they need themselves.

NODE_DIR ?= $(CURDIR)/.kmplify-node
GATEWAY  ?= http://127.0.0.1:18100
OLLAMA   ?= http://127.0.0.1:11434
FEATURES ?=

VERSION  := $(shell grep -m1 '^version = ' Cargo.toml | cut -d'"' -f2)
HOST     := $(shell rustc -vV 2>/dev/null | sed -n 's/^host: //p')
UNAME_S  := $(shell uname -s)

# What every dev run gets: its own node directory and engine, and the
# gateway. Router runs point at a dead gateway when asked (see node-a/b).
ENV      := KMPLIFY_NODE_DIR="$(NODE_DIR)" PROVIDER_GATEWAY_URL="$(GATEWAY)" OLLAMA_BASE="$(OLLAMA)"
CARGO_RUN := cargo run --quiet $(if $(FEATURES),--features $(FEATURES),) --

.DEFAULT_GOAL := help
.PHONY: help version build build-gui release release-gui functions \
        fmt fmt-check clippy clippy-gui test test-gui licenses ci \
        init check run router tui tui-router gui status node-a node-b \
        deb dmg static package clean clean-node

help: ## List the targets (this text)
	@echo "kmplify-node $(VERSION)  ·  host $(HOST)"
	@echo
	@echo "NODE_DIR=$(NODE_DIR)"
	@echo "GATEWAY=$(GATEWAY)   OLLAMA=$(OLLAMA)"
	@echo
	@awk 'BEGIN {FS = ":.*## "} \
	      /^##@/ {printf "\n%s\n", substr($$0, 5); next} \
	      /^[a-zA-Z0-9_-]+:.*## / {printf "  %-14s %s\n", $$1, $$2}' $(MAKEFILE_LIST)
	@echo

version: ## Print the crate version and the host target triple
	@echo "$(VERSION) $(HOST)"

##@ Build

build: ## Debug build, default features (tui + wasm)
	cargo build

build-gui: ## Debug build with the window and the LAN router
	cargo build --features gui

release: ## Release build, default features, for this host
	cargo build --release --target $(HOST)

release-gui: ## Release build with gui, for this host (what the packages ship)
	cargo build --release --features gui --target $(HOST)

functions: ## Build the catalog's Wasm functions (needs the wasm32-wasip1 target)
	functions/build.sh

##@ Quality (what CI runs)

fmt: ## Format the tree
	cargo fmt

fmt-check: ## Fail if the tree is not formatted (CI lint job)
	cargo fmt --check

clippy: ## Clippy, default features, warnings are errors (CI lint job)
	RUSTFLAGS="-D warnings" cargo clippy --all-targets

clippy-gui: ## Clippy with gui, warnings are errors (CI gui job)
	RUSTFLAGS="-D warnings" cargo clippy --all-targets --features gui

test: ## Unit tests, default features; no network or hardware needed
	cargo test --all-targets

test-gui: ## Unit tests with gui and the router
	cargo test --all-targets --features gui

licenses: ## cargo-deny: no copyleft, no banned crates (CI licenses job)
	cargo deny check licenses bans

ci: fmt-check clippy clippy-gui test test-gui licenses ## Everything CI checks, locally

##@ Spin up (all use NODE_DIR, GATEWAY, OLLAMA)

init: ## First-run wizard for the dev node
	$(ENV) $(CARGO_RUN) init

check: ## Preflight this host as the dev node would see it
	$(ENV) $(CARGO_RUN) check

run: ## Join the gateway and serve, logging to stdout (Ctrl-C stops it)
	$(ENV) $(CARGO_RUN) run

router: ## Serve AND run the LAN router headless (14418, 11440, 11441)
	$(ENV) cargo run --quiet --features router -- run --router

tui: ## Terminal dashboard; attaches to a running dev node or starts one
	$(ENV) $(CARGO_RUN) tui

tui-router: ## Dashboard with the network and cluster screens (8, 9)
	$(ENV) cargo run --quiet --features router -- tui --router

gui: ## Desktop window; attaches to the running dev node and router
	$(ENV) cargo run --quiet --features gui -- gui

status: ## One-shot JSON report of the running dev node
	$(ENV) $(CARGO_RUN) status --json

# Two routers on one machine, for pairing and routing without a second box
# (docs/ROUTER.md): separate node dirs, names and ports, a dead gateway so
# neither joins a fabric. mDNS cannot see across one host, so pair node-b by
# typing node-a's address, 127.0.0.1:14418, on its cluster screen.
node-a: ## Router test node A: dir .kmplify-node/a, ports 14418/11440/11441
	KMPLIFY_NODE_DIR="$(NODE_DIR)/a" KMPLIFY_NODE_NAME=node-a \
	KMPLIFY_ROUTER_PORTS=14418,11440,11441 PROVIDER_GATEWAY_URL=http://127.0.0.1:9 \
	OLLAMA_BASE="$(OLLAMA)" cargo run --quiet --features router -- tui --router

node-b: ## Router test node B: dir .kmplify-node/b, ports 24418/21440/21441
	KMPLIFY_NODE_DIR="$(NODE_DIR)/b" KMPLIFY_NODE_NAME=node-b \
	KMPLIFY_ROUTER_PORTS=24418,21440,21441 PROVIDER_GATEWAY_URL=http://127.0.0.1:9 \
	OLLAMA_BASE="$(OLLAMA)" cargo run --quiet --features router -- tui --router

##@ Packaging (the same commands release.yml runs)

deb: release-gui ## Debian package into out/ (Linux; needs cargo-deb)
	@command -v cargo-deb >/dev/null || { echo "cargo install cargo-deb --locked"; exit 1; }
	cargo deb --target $(HOST) --no-build --output out/

dmg: release-gui ## KMPLIFY Node.app in a .dmg into out/ (macOS)
	mkdir -p out
	scripts/bundle-macos.sh target/$(HOST)/release/kmplify-node v$(VERSION) out/kmplify-node-v$(VERSION)-$(HOST).dmg

static: ## Fully static Linux binary via Docker into dist-node/
	docker build -f packaging/Dockerfile.node-build -o dist-node .

package: ## The package for this OS: dmg on macOS, deb on Linux
ifeq ($(UNAME_S),Darwin)
	$(MAKE) dmg
else
	$(MAKE) deb
endif

##@ Housekeeping

clean: ## Remove build output and packages (keeps the dev node)
	cargo clean
	rm -rf out dist-node

clean-node: ## Remove the dev node directory (identity, settings, cluster)
	rm -rf "$(NODE_DIR)"
