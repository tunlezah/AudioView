CARGO ?= cargo
FIXTURES := fixtures/sessions

.PHONY: help build test lint fmt fmt-check fixtures fixtures-check check clean

help:
	@echo "build         debug build of the workspace"
	@echo "test          run all tests"
	@echo "lint          clippy, warnings denied"
	@echo "fmt           format"
	@echo "check         fmt-check + lint + test + fixtures-check  (what CI runs)"
	@echo "fixtures      regenerate synthetic fixtures and golden files"
	@echo "fixtures-check verify golden files are current"

build:
	$(CARGO) build --workspace --all-targets

test:
	$(CARGO) test --workspace

lint:
	$(CARGO) clippy --workspace --all-targets -- -D warnings

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

fixtures:
	$(CARGO) run -q -p lpcapture -- synth --out-dir $(FIXTURES)

fixtures-check:
	$(CARGO) run -q -p lpcapture -- golden --check

check: fmt-check lint test fixtures-check

clean:
	$(CARGO) clean
