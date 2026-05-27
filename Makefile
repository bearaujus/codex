.PHONY: build prod release install run clean

# Build the release codex binary into ./bin (prod/release are back-compat aliases).
build:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/build.ps1

prod: build

release: build

# Build + install codex onto your user PATH (%LOCALAPPDATA%\codex\bin).
install:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/install-windows.ps1

# Build + run the local codex binary.
run:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/run-windows.ps1

# Remove the repo-local built binary.
clean:
	powershell.exe -NoProfile -ExecutionPolicy Bypass -File scripts/clean-repo-artifacts.ps1
