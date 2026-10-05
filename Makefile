PREFIX ?= $(HOME)/.local
BINDIR := $(PREFIX)/bin
DATADIR := $(PREFIX)/share
UNITDIR := $(HOME)/.config/systemd/user
APP_ID := dev.radar.Radar

.PHONY: build test install uninstall

build:
	cargo build --release

test:
	cargo test --workspace

install: build
	install -Dm755 target/release/radar-collect $(BINDIR)/radar-collect
	install -Dm755 target/release/radar $(BINDIR)/radar
	install -Dm755 data/radar-claude-statusline $(BINDIR)/radar-claude-statusline
	install -Dm644 data/radar-collect.service $(UNITDIR)/radar-collect.service
	install -Dm644 data/$(APP_ID).desktop $(DATADIR)/applications/$(APP_ID).desktop
	install -Dm644 data/$(APP_ID).svg $(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	systemctl --user daemon-reload
	systemctl --user enable --now radar-collect
	systemctl --user restart radar-collect

uninstall:
	-systemctl --user disable --now radar-collect
	rm -f $(BINDIR)/radar-collect $(BINDIR)/radar $(BINDIR)/radar-claude-statusline
	rm -f $(UNITDIR)/radar-collect.service
	rm -f $(DATADIR)/applications/$(APP_ID).desktop
	rm -f $(DATADIR)/icons/hicolor/scalable/apps/$(APP_ID).svg
	systemctl --user daemon-reload
