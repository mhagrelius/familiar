UUID = webcam-mic@hagreli.us
SRC = src/$(UUID)
INSTALL_DIR = $(HOME)/.local/share/gnome-shell/extensions/$(UUID)
BUILD_DIR = build

.PHONY: all install uninstall enable disable pack lint logs clean

all: lint

install: lint
	rm -rf $(INSTALL_DIR)
	mkdir -p $(INSTALL_DIR)
	cp -r $(SRC)/. $(INSTALL_DIR)
	@echo "Installed. On Wayland the shell only discovers a new extension at"
	@echo "startup, so log out and back in the first time, then:"
	@echo "  gnome-extensions enable $(UUID)"

uninstall:
	rm -rf $(INSTALL_DIR)

enable:
	gnome-extensions enable $(UUID)

disable:
	gnome-extensions disable $(UUID)

# The gate. A syntax error in a shell extension takes the whole panel down and
# reports itself only in the journal, so every file has to parse before it is
# worth installing. Syntax only, and node rather than gjs on purpose:
# `extension.js` imports `resource:///org/gnome/shell/...`, which resolves only
# inside a running shell.
lint:
	@mkdir -p $(BUILD_DIR)
	@for file in $(SRC)/*.js; do \
		cp "$$file" $(BUILD_DIR)/check.mjs; \
		node --check $(BUILD_DIR)/check.mjs || { echo "FAILED: $$file"; exit 1; }; \
		echo "ok: $$file"; \
	done
	@rm -f $(BUILD_DIR)/check.mjs
	@python3 -c "import json; json.load(open('$(SRC)/metadata.json'))" && echo "ok: metadata.json"
	@echo "All checks passed."

pack:
	rm -rf $(BUILD_DIR)
	mkdir -p $(BUILD_DIR)
	cd $(SRC) && zip -r ../../$(BUILD_DIR)/$(UUID).zip . -x '*.compiled'

logs:
	journalctl -f -o cat /usr/bin/gnome-shell | grep -i webcam-mic

clean:
	rm -rf $(BUILD_DIR)
