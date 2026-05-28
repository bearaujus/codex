.PHONY: build build-fast prod release fmt check test test-full bench-smoke lint install run clean web-check web-build

# Build a cheap local codex binary into ./bin for the edit/run loop.
build:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build.ps1 -CargoProfile dev-small

# Build the faster release-like local profile into ./bin.
build-fast:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build.ps1 -CargoProfile fast

prod: build-fast

release: build-fast

# Format the Rust workspace with the repo-standard formatter.
fmt:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/fmt.ps1

# Inner dev loop: fast compile check (no optimized binary). Narrow with:
# make check p=codex-login args="--features foo"
check:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/check.ps1 $(p) $(args)

# Run the fast local test loop. This skips benchmark smoke. Narrow with:
# make test p=codex-login args="--no-run"
test:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test.ps1 $(p) $(args)

# Run the repo-standard local verification path: nextest + benchmark smoke.
test-full:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test.ps1 -Full $(p) $(args)

# Run the benchmark smoke pass on demand.
bench-smoke:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/bench-smoke.ps1

# Clippy with the repo's deny lints; run before committing. Narrow with:
# make lint p=codex-login args="--features foo"
lint:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/lint.ps1 $(p) $(args)

# Compile-check for the browser-oriented wasm target. Narrow with:
# make web-check p=codex-login args="--features web"
web-check:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/web-check.ps1 $(p) $(args)

# Build a wasm artifact for the browser-oriented wasm target. Narrow with:
# make web-build p=codex-login args="--release --features web"
web-build:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/web-build.ps1 $(p) $(args)

# Build + install codex onto your user PATH (%LOCALAPPDATA%\codex\bin).
install:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/install-windows.ps1

# Build + run the local codex binary.
run:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/run-windows.ps1

# Remove the repo-local built binary.
clean:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/clean-repo-artifacts.ps1
