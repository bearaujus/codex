SHELL := sh

CODEX_RS_DIR := codex-rs
BIN_DIR := bin
EXE :=

ifeq ($(OS),Windows_NT)
EXE := .exe
endif

CODEX_BIN := $(BIN_DIR)/codex$(EXE)

.PHONY: prod
prod: $(CODEX_BIN)

.PHONY: release
release: prod

$(BIN_DIR):
	mkdir -p "$(BIN_DIR)"

$(CODEX_BIN): | $(BIN_DIR)
	cd "$(CODEX_RS_DIR)" && cargo build -p codex-cli --release
	cp "$(CODEX_RS_DIR)/target/release/codex$(EXE)" "$(CODEX_BIN)"

.PHONY: clean
clean:
	rm -f "$(CODEX_BIN)"
