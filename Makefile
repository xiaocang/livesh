PREFIX ?= $(HOME)/.local
BINDIR ?= $(PREFIX)/bin
CARGO ?= cargo

.PHONY: build test install

build:
	$(CARGO) build

test:
	$(CARGO) test

install:
	$(CARGO) build --release
	install -d "$(BINDIR)"
	install -m 0755 target/release/livesh "$(BINDIR)/livesh"
	install -m 0755 target/release/liveshd "$(BINDIR)/liveshd"
	install -m 0755 target/release/liveshctl "$(BINDIR)/liveshctl"
