CARGO ?= cargo
FIXTURES := fixtures/sessions

# Everything that runs as root on a device, or builds what does.
SHELL_SCRIPTS := provisioning/install.sh \
                 provisioning/build-deb.sh \
                 provisioning/make-writable-partition.sh \
                 provisioning/lpframe-rw \
                 provisioning/lpframe-ro \
                 provisioning/pi-gen/build-image.sh \
                 provisioning/pi-gen/add-data-partition.sh \
                 provisioning/pi-gen/test-add-data-partition.sh \
                 provisioning/pi-gen/stage-lpframe/prerun.sh \
                 provisioning/pi-gen/stage-lpframe/00-lpframe/00-run.sh \
                 provisioning/pi-gen/stage-lpframe/00-lpframe/01-run-chroot.sh

.PHONY: help build test lint fmt fmt-check fixtures fixtures-check \
        provisioning-check deb image placeholder check clean

help:
	@echo "build         debug build of the workspace"
	@echo "test          run all tests"
	@echo "lint          clippy, warnings denied"
	@echo "fmt           format"
	@echo "check         fmt-check + lint + test + fixtures-check  (what CI runs)"
	@echo "fixtures      regenerate synthetic fixtures and golden files"
	@echo "fixtures-check verify golden files are current"
	@echo "provisioning-check  shell syntax, shellcheck if present, dry-run install"
	@echo "deb           build a .deb into dist/"
	@echo "image         build a flashable .img (needs root, takes an hour)"
	@echo "placeholder   regenerate provisioning/placeholder.png"

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

# Everything under provisioning/ that can be checked without a Pi.
#
# The dry run is the substantive part: it walks every step of the installer
# with run() printing instead of executing, so a stray unquoted path or a
# missing file is caught here rather than on a device.
provisioning-check:
	@for script in $(SHELL_SCRIPTS); do bash -n "$$script" || exit 1; done
	@if command -v shellcheck >/dev/null; then \
		shellcheck -S warning $(SHELL_SCRIPTS); \
	else \
		echo "shellcheck not installed; skipped (bash -n passed)"; \
	fi
	@./provisioning/install.sh --dry-run --yes --no-build --skip-shairport >/dev/null
	@./provisioning/make-writable-partition.sh --dry-run >/dev/null 2>&1 || true
	@./provisioning/pi-gen/test-add-data-partition.sh
	@echo "provisioning ok"

deb:
	./provisioning/build-deb.sh

image:
	sudo ./provisioning/pi-gen/build-image.sh

placeholder:
	python3 provisioning/make-placeholder.py

check: fmt-check lint test fixtures-check provisioning-check

clean:
	$(CARGO) clean
