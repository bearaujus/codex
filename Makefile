.PHONY: build prod release install run clean check lint

# Build the release codex binary into ./bin (prod/release are back-compat aliases).
build:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build.ps1

prod: build

release: build

# Inner dev loop: fast compile check (no optimized binary). Narrow with: make check p=codex-login
check:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/check.ps1 $(p)

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
