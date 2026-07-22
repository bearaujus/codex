.PHONY: build prod release fmt check test lint install run clean

# Build a cheap local codex binary into ./bin for the edit/run loop.
build:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build.ps1 -CargoProfile dev-small

prod: build

release: build

# Format Rust code with the repo-standard formatter. Narrow with:
# make fmt p=codex-login args="--check"
fmt:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/fmt.ps1 $(p) $(args)

# Inner dev loop: fast compile check (no optimized binary). Narrow with:
# make check p=codex-login args="--features foo"
check:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/check.ps1 $(p) $(args)

# Run the local test loop with nextest. Narrow with:
# make test p=codex-login args="--no-run"
test:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test.ps1 $(p) $(args)

# Clippy with the repo's deny lints; run before committing. Narrow with:
# make lint p=codex-login args="--features foo"
lint:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/lint.ps1 $(p) $(args)

# Force a fresh build and install codex onto your user PATH.
install:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/install-windows.ps1 -CargoProfile dev-small -ForceBuild

# Build + run the local codex binary.
run:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/run-windows.ps1

# Remove all local build artifacts: the cargo target/ tree (every profile) and
# the repo-local binary. Frees the bulk of disk; next build is a cold rebuild.
clean:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/clean-repo-artifacts.ps1
