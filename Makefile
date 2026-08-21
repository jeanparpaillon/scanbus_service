BACKENDS := brother,hplip,mobile
CARGO_ENV ?= debug

ifeq ($(CARGO_ENV),release)
TARGET_DIR ?= target/release
else ifeq ($(CARGO_ENV),debug)
TARGET_DIR ?= target/debug
else
$(error "CARGO_ENV must be either 'release' or 'debug'")
endif


DESTDIR ?=
PREFIX ?= /usr
BINDIR ?= $(PREFIX)/bin
SCANBUS_MANPAGES ?= target/man
SYSTEMD_USER_DIR ?= $(PREFIX)/lib/systemd/user
DBUS_SERVICE_DIR ?= $(PREFIX)/share/dbus-1/services
LIBEXECDIR ?= $(PREFIX)/libexec/scanbus
APPLICATIONS_DIR ?= $(PREFIX)/share/applications
AUTOSTART_DIR ?= /etc/xdg/autostart
DOCDIR ?= $(PREFIX)/share/doc
MANDIR ?= $(PREFIX)/share/man
# The doc directory is per binary package, so debian/rules overrides this once per
# package root; a plain `make install` ships a single copy under the project name.
DOC_PACKAGE ?= scanbus

ifeq ($(CARGO_ENV),release)
profile_opt := --release
else
profile_opt :=
endif

binaries := scanbus scanbus-daemon scanbus-gui
targets := $(patsubst %, $(TARGET_DIR)/%, $(binaries))

ifneq (,$(findstring hplip,$(BACKENDS)))
hplip_services := install-hplip-services
endif

all: $(targets)

release:
	$(MAKE) CARGO_ENV=release all

debug:
	$(MAKE) CARGO_ENV=debug all

$(TARGET_DIR)/scanbus: FORCE
	cargo build --package scanbus-cli --bin $(notdir $@) $(profile_opt)

$(TARGET_DIR)/scanbus-daemon: FORCE
	cargo build --package scanbus-daemon --bin $(notdir $@) $(profile_opt) --features $(BACKENDS)

$(TARGET_DIR)/scanbus-gui: FORCE
	cargo build --package scanbus-gui --bin $(notdir $@) $(profile_opt)

install: install-backend install-frontend

install-backend: install-services
	install -D -m 0755 "$(TARGET_DIR)/scanbus" "$(DESTDIR)$(BINDIR)/scanbus"
	install -D -m 0755 "$(TARGET_DIR)/scanbus-daemon" "$(DESTDIR)$(BINDIR)/scanbus-daemon"
	install -D -m 0755 packaging/libexec/scanbus/scanbus-scanimage \
		"$(DESTDIR)$(LIBEXECDIR)/scanbus-scanimage"

install-services: $(hplip_services)
	install -D -m 0644 packaging/systemd/user/scanbus.service \
		"$(DESTDIR)$(SYSTEMD_USER_DIR)/scanbus.service"
	install -D -m 0644 packaging/dbus-1/services/org.scanbus.service \
		"$(DESTDIR)$(DBUS_SERVICE_DIR)/org.scanbus.service"

# Only meaningful when the HP backend is linked in: it is the sole user of
# com.hplip.StatusService, and the file starts hp-systray on any bus that reads it.
install-hplip-services:
	install -D -m 0644 packaging/dbus-1/services/scanbus-hplip-status.service \
		"$(DESTDIR)$(DBUS_SERVICE_DIR)/scanbus-hplip-status.service"

install-frontend:
	install -D -m 0755 "$(TARGET_DIR)/scanbus-gui" "$(DESTDIR)$(BINDIR)/scanbus-gui"
	install -D -m 0644 packaging/applications/org.scanbus.Gui.desktop \
		"$(DESTDIR)$(APPLICATIONS_DIR)/org.scanbus.Gui.desktop"
	install -D -m 0644 packaging/autostart/org.scanbus.Gui.desktop \
		"$(DESTDIR)$(AUTOSTART_DIR)/org.scanbus.Gui.desktop"

manpages:
	"$(TARGET_DIR)/scanbus" manpage --output-dir target/man

install-manpages:
	install -d "$(DESTDIR)$(MANDIR)/man1"
	cp -a target/man/* $(DESTDIR)$(MANDIR)/man1
	gzip -9n "$(DESTDIR)$(MANDIR)/man1"/*.1

deb:
	$(MAKE) -f debian/rules binary

# VERSION is the release version; Cargo.toml carries a generated copy of it.
version:
	@./scripts/version.sh

version-check:
	./scripts/version.sh check

version-sync:
	./scripts/version.sh sync

reload:
	systemctl --user daemon-reload
	dbus-send --session --type=method_call --dest=org.freedesktop.DBus \
		/ org.freedesktop.DBus.ReloadConfig
	systemctl --user restart scanbus.service || true

test:
	cargo test --workspace --features $(BACKENDS)

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

clean:
	cargo clean ${profile_opt}

.PHONY: all release debug 
.PHONY: install install-backend install-frontend install-services manpages
.PHONY: install-hplip-services
.PHONY: deb version version-check version-sync
.PHONY: reload test clean lint FORCE
