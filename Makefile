# Stronghold Makefile
# Common development tasks

.PHONY: all build test fmt clippy clean release run-gateway run-cli help

all: build

## Build the workspace in debug mode
build:
	cargo build --workspace

## Build in release mode with SEV-SNP support (default)
release:
	cargo build --workspace --release --features sev-snp

## Build without SEV-SNP (for dev environments without SEV hardware)
release-no-sev:
	cargo build --workspace --release --features no-sev-snp

## Run all tests
test:
	cargo test --workspace

## Format code
fmt:
	cargo fmt --all

## Run clippy lints
clippy:
	cargo clippy --workspace --all-targets -- -D warnings

## Run the gateway (dev mode)
run-gateway:
	cargo run --bin stronghold-gateway -- --dev

## Run the CLI
run-cli:
	cargo run --bin stronghold -- --help

## Clean build artifacts
clean:
	cargo clean

## Generate documentation
docs:
	cargo doc --workspace --no-deps --open

## Build all catalog images
build-images:
	@for dir in images/*/; do \
		if [ -f "$$dir/image.toml" ]; then \
			echo "Building $$dir..."; \
			cargo run --bin stronghold -- image build "$$dir/image.toml" || true; \
		fi; \
	done

## Verify audit log (example)
verify-audit:
	cargo run --bin stronghold -- audit verify --tenant default

## Help
help:
	@echo "Stronghold Development Tasks"
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@echo "Targets:"
	@grep -E '^## ' $(MAKEFILE_LIST) | sed 's/## //; s/:/  →  /'
