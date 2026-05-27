.PHONY: build prod release fmt check test lint install run clean

# Build the release codex binary into ./bin (prod/release are back-compat aliases).
build:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build.ps1

prod: build

release: build

# Format the Rust workspace with the repo-standard formatter.
fmt:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/fmt.ps1

# Inner dev loop: fast compile check (no optimized binary). Narrow with: make check p=codex-login
check:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/check.ps1 $(p)

# Run repo-standard tests. Narrow with: make test p=codex-login
test:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/test.ps1 $(p)

# Clippy with the repo's deny lints; run before committing. Narrow with: make lint p=codex-login
lint:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/lint.ps1 $(p)

# Build + install codex onto your user PATH (%LOCALAPPDATA%\codex\bin).
install:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/install-windows.ps1

# Build + run the local codex binary.
run:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/run-windows.ps1

# Remove the repo-local built binary.
clean:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/clean-repo-artifacts.ps1
