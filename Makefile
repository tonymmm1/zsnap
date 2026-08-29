SHELL := /bin/sh

CARGO ?= cargo
PREFIX ?= /usr/local
BINDIR ?= $(PREFIX)/sbin
SYSCONFDIR ?= /etc
SYSTEMD_UNIT_DIR ?= /etc/systemd/system
OPENRC_INIT_DIR ?= /etc/init.d
PERIODIC_DIR ?= /etc/periodic/15min
DESTDIR ?=

BINARY := target/release/zsnap
STATIC_TARGET ?= x86_64-unknown-linux-musl
MUSL_CC ?= cc
STATIC_BINARY := target/$(STATIC_TARGET)/release/zsnap
STATIC_TARGET_ENV := $(shell printf '%s' '$(STATIC_TARGET)' | tr '[:lower:]-' '[:upper:]_')
RUST_SOURCES := $(wildcard src/*.rs tests/*.rs)
CONFIG_TARGET := $(DESTDIR)$(SYSCONFDIR)/zsnap/zsnap.toml
ENV_TARGET := $(DESTDIR)$(SYSCONFDIR)/zsnap/webhooks.env
SERVICE_TARGET := $(DESTDIR)$(SYSTEMD_UNIT_DIR)/zsnap.service
TIMER_TARGET := $(DESTDIR)$(SYSTEMD_UNIT_DIR)/zsnap.timer
OPENRC_TARGET := $(DESTDIR)$(OPENRC_INIT_DIR)/zsnap
PERIODIC_TARGET := $(DESTDIR)$(PERIODIC_DIR)/zsnap

.PHONY: all build release test check fmt lint install install-binary install-static \
	install-static-binary install-config install-systemd install-openrc install-static-openrc \
	install-openrc-service install-none install-static-none enable enable-openrc disable \
	disable-openrc uninstall uninstall-systemd uninstall-openrc uninstall-common clean package \
	static verify verify-static

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

verify: fmt lint test release

verify-static: verify static

install: install-binary install-config install-systemd

install-static: install-static-binary install-config install-systemd

install-openrc: install-binary install-config install-openrc-service

install-static-openrc: install-static-binary install-config install-openrc-service

install-none: install-binary install-config

install-static-none: install-static-binary install-config

install-binary: $(BINARY)
	install -d -m755 $(DESTDIR)$(BINDIR)
	install -m755 $(BINARY) $(DESTDIR)$(BINDIR)/zsnap

install-static-binary: $(STATIC_BINARY)
	install -d -m755 $(DESTDIR)$(BINDIR)
	install -m755 $(STATIC_BINARY) $(DESTDIR)$(BINDIR)/zsnap

install-config:
	install -d -m750 $(DESTDIR)$(SYSCONFDIR)/zsnap
	@if [ ! -e "$(CONFIG_TARGET)" ]; then \
		install -m600 config.example.toml "$(CONFIG_TARGET)"; \
		echo "installed example configuration at $(CONFIG_TARGET)"; \
	else \
		echo "preserved existing configuration at $(CONFIG_TARGET)"; \
	fi
	@if [ ! -e "$(ENV_TARGET)" ]; then \
		install -m600 contrib/webhooks.env.example "$(ENV_TARGET)"; \
		echo "installed empty webhook environment at $(ENV_TARGET)"; \
	else \
		echo "preserved existing webhook environment at $(ENV_TARGET)"; \
	fi

install-systemd:
	install -d -m755 $(DESTDIR)$(SYSTEMD_UNIT_DIR)
	install -m644 contrib/zsnap.service $(SERVICE_TARGET)
	install -m644 contrib/zsnap.timer $(TIMER_TARGET)

install-openrc-service:
	install -d -m755 $(DESTDIR)$(OPENRC_INIT_DIR)
	install -m755 contrib/zsnap.openrc $(OPENRC_TARGET)

enable:
	@test -z "$(DESTDIR)" || { echo "enable cannot be used with DESTDIR" >&2; exit 2; }
	systemctl daemon-reload
	systemctl enable --now zsnap.timer

enable-openrc:
	@test -z "$(DESTDIR)" || { echo "enable-openrc cannot be used with DESTDIR" >&2; exit 2; }
	install -d -m755 $(PERIODIC_DIR)
	install -m755 contrib/zsnap.periodic $(PERIODIC_DIR)/zsnap
	rc-update add crond default
	rc-service crond start

disable:
	@test -z "$(DESTDIR)" || { echo "disable cannot be used with DESTDIR" >&2; exit 2; }
	-systemctl disable --now zsnap.timer

disable-openrc:
	@test -z "$(DESTDIR)" || { echo "disable-openrc cannot be used with DESTDIR" >&2; exit 2; }
	rm -f $(PERIODIC_DIR)/zsnap

# The user-edited configuration is intentionally retained.
uninstall: uninstall-systemd

uninstall-systemd: disable uninstall-common
	rm -f $(SERVICE_TARGET) $(TIMER_TARGET)
	@if [ -z "$(DESTDIR)" ]; then systemctl daemon-reload; fi

uninstall-openrc: disable-openrc uninstall-common
	rm -f $(OPENRC_TARGET)

uninstall-common:
	rm -f $(DESTDIR)$(BINDIR)/zsnap
	@echo "retained $(CONFIG_TARGET)"
	@echo "retained $(ENV_TARGET)"

package:
	$(CARGO) package --locked

# Requires the musl target: rustup target add x86_64-unknown-linux-musl
static:
	CC=$(MUSL_CC) CARGO_TARGET_$(STATIC_TARGET_ENV)_LINKER=$(MUSL_CC) \
		$(CARGO) build --release --locked --target $(STATIC_TARGET)

clean:
	$(CARGO) clean

$(BINARY): Cargo.toml Cargo.lock rust-toolchain.toml $(RUST_SOURCES)
	$(CARGO) build --release --locked

$(STATIC_BINARY): Cargo.toml Cargo.lock rust-toolchain.toml $(RUST_SOURCES)
	CC=$(MUSL_CC) CARGO_TARGET_$(STATIC_TARGET_ENV)_LINKER=$(MUSL_CC) \
		$(CARGO) build --release --locked --target $(STATIC_TARGET)
