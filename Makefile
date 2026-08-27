SHELL := /bin/sh

CARGO ?= cargo
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/sbin
SYSCONFDIR ?= /etc
SYSTEMD_UNIT_DIR ?= /etc/systemd/system
DESTDIR ?=

BINARY := target/release/zsnap
STATIC_TARGET ?= x86_64-unknown-linux-musl
STATIC_BINARY := target/$(STATIC_TARGET)/release/zsnap
RUST_SOURCES := $(wildcard src/*.rs tests/*.rs)
CONFIG_TARGET := $(DESTDIR)$(SYSCONFDIR)/zsnap/zsnap.toml
SERVICE_TARGET := $(DESTDIR)$(SYSTEMD_UNIT_DIR)/zsnap.service
TIMER_TARGET := $(DESTDIR)$(SYSTEMD_UNIT_DIR)/zsnap.timer

.PHONY: all build release test check fmt lint install install-binary install-static \
	install-static-binary install-config install-systemd enable disable uninstall clean \
	package static

all: build

build:
	$(CARGO) build

release:
	$(CARGO) build --release --locked

test:
	$(CARGO) test --all-targets --locked

check:
	$(CARGO) check --all-targets --locked

fmt:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --all-targets --all-features --locked -- -D warnings

install: install-binary install-config install-systemd

install-static: install-static-binary install-config install-systemd

install-binary: $(BINARY)
	install -Dm755 $(BINARY) $(DESTDIR)$(BINDIR)/zsnap

install-static-binary: $(STATIC_BINARY)
	install -Dm755 $(STATIC_BINARY) $(DESTDIR)$(BINDIR)/zsnap

install-config:
	install -d -m755 $(DESTDIR)$(SYSCONFDIR)/zsnap
	@if [ ! -e "$(CONFIG_TARGET)" ]; then \
		install -m600 config.example.toml "$(CONFIG_TARGET)"; \
		echo "installed example configuration at $(CONFIG_TARGET)"; \
	else \
		echo "preserved existing configuration at $(CONFIG_TARGET)"; \
	fi

install-systemd:
	install -Dm644 contrib/zsnap.service $(SERVICE_TARGET)
	install -Dm644 contrib/zsnap.timer $(TIMER_TARGET)

enable:
	@test -z "$(DESTDIR)" || { echo "enable cannot be used with DESTDIR" >&2; exit 2; }
	systemctl daemon-reload
	systemctl enable --now zsnap.timer

disable:
	@test -z "$(DESTDIR)" || { echo "disable cannot be used with DESTDIR" >&2; exit 2; }
	-systemctl disable --now zsnap.timer

# The user-edited configuration is intentionally retained.
uninstall: disable
	rm -f $(DESTDIR)$(BINDIR)/zsnap $(SERVICE_TARGET) $(TIMER_TARGET)
	@if [ -z "$(DESTDIR)" ]; then systemctl daemon-reload; fi
	@echo "retained $(CONFIG_TARGET)"

package:
	$(CARGO) package --locked

# Requires the musl target: rustup target add x86_64-unknown-linux-musl
static:
	$(CARGO) build --release --locked --target $(STATIC_TARGET)

clean:
	$(CARGO) clean

$(BINARY): Cargo.toml Cargo.lock rust-toolchain.toml $(RUST_SOURCES)
	$(CARGO) build --release --locked

$(STATIC_BINARY): Cargo.toml Cargo.lock rust-toolchain.toml $(RUST_SOURCES)
	$(CARGO) build --release --locked --target $(STATIC_TARGET)
