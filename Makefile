BACKENDS := brother,hplip,mobile
CARGO_ENV ?= debug
PREFIX ?= /usr
BINDIR ?= $(PREFIX)/bin
SYSTEMD_USER_DIR ?= $(PREFIX)/lib/systemd/user
DBUS_SERVICE_DIR ?= $(PREFIX)/share/dbus-1/services
LIBEXECDIR ?= $(PREFIX)/libexec/scanbus
DESTDIR ?=
PACKAGE_NAME := scanbus
VERSION := $(shell sed -n '/^\[workspace\.package\]/,/^\[/{s/^version = "\(.*\)"/\1/p}' Cargo.toml | head -n 1)
DEB_ARCH := $(shell dpkg-architecture -qDEB_HOST_ARCH)
DEB_STAGE_DIR := target/debian/$(PACKAGE_NAME)/$(PACKAGE_NAME)_$(VERSION)_$(DEB_ARCH)
DEB_ROOT := $(DEB_STAGE_DIR)/pkg
DEB_OUTPUT := $(DEB_STAGE_DIR)/$(PACKAGE_NAME)_$(VERSION)_$(DEB_ARCH).deb

ifeq ($(CARGO_ENV),release)
profile_opt := --release
else
profile_opt :=
endif

all: build

build:
	cargo build --workspace $(profile_opt) --features $(BACKENDS)

release:
	cargo build -p scanbus-cli --release --locked
	cargo build -p scanbus-daemon --release --features $(BACKENDS) --locked

install:
	test -n "$(DESTDIR)" || { echo "DESTDIR is required"; exit 1; }
	install -D -m 0755 target/release/scanbus "$(DESTDIR)$(BINDIR)/scanbus"
	install -D -m 0755 target/release/scanbus-daemon "$(DESTDIR)$(BINDIR)/scanbus-daemon"
	install -D -m 0644 packaging/systemd/user/scanbus.service \
		"$(DESTDIR)$(SYSTEMD_USER_DIR)/scanbus.service"
	install -D -m 0644 packaging/dbus-1/services/org.scanbus.service \
		"$(DESTDIR)$(DBUS_SERVICE_DIR)/org.scanbus.service"
	install -D -m 0755 packaging/libexec/scanbus/scanbus-scanimage \
		"$(DESTDIR)$(LIBEXECDIR)/scanbus-scanimage"
	install -D -m 0755 debian/postinst "$(DESTDIR)/DEBIAN/postinst"

deb:
	PACKAGE_NAME="$(PACKAGE_NAME)" VERSION="$(VERSION)" DEB_ARCH="$(DEB_ARCH)" \
		DEB_STAGE_DIR="$(DEB_STAGE_DIR)" DEB_ROOT="$(DEB_ROOT)" DEB_OUTPUT="$(DEB_OUTPUT)" \
		$(MAKE) -f debian/rules binary

test:
	cargo test --workspace --features $(BACKENDS)

clean:
	cargo clean

.PHONY: all build release install deb test clean
