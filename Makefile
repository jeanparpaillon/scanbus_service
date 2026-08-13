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

$(TARGET_DIR)/scanbus:
	cargo build --package scanbus-cli --bin $(notdir $@) $(profile_opt)

$(TARGET_DIR)/scanbus-daemon:
	cargo build --package scanbus-daemon --bin $(notdir $@) $(profile_opt) --features $(BACKENDS)

$(TARGET_DIR)/scanbus-gui:
	cargo build --package scanbus-gui --bin $(notdir $@) $(profile_opt)

install: install-backend install-frontend install-services install-doc install-license

# Debian wants the license under the policy-mandated name `copyright`, with the
# copyright holder stated — the bare Apache text does not carry one — so the deb
# ships debian/copyright and everyone else gets LICENSE verbatim.
install-license:
	install -D -m 0644 LICENSE "$(DESTDIR)$(DOCDIR)/$(DOC_PACKAGE)/LICENSE"

install-copyright:
	install -D -m 0644 debian/copyright "$(DESTDIR)$(DOCDIR)/$(DOC_PACKAGE)/copyright"

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

install-frontend: $(TARGET_DIR)/scanbus-gui
	install -D -m 0755 "$(TARGET_DIR)/scanbus-gui" "$(DESTDIR)$(BINDIR)/scanbus-gui"
	install -D -m 0644 packaging/applications/org.scanbus.Gui.desktop \
		"$(DESTDIR)$(APPLICATIONS_DIR)/org.scanbus.Gui.desktop"
	install -D -m 0644 packaging/autostart/org.scanbus.Gui.desktop \
		"$(DESTDIR)$(AUTOSTART_DIR)/org.scanbus.Gui.desktop"

install-backend: $(TARGET_DIR)/scanbus $(TARGET_DIR)/scanbus-daemon
	install -D -m 0755 "$(TARGET_DIR)/scanbus" "$(DESTDIR)$(BINDIR)/scanbus"
	install -D -m 0755 "$(TARGET_DIR)/scanbus-daemon" "$(DESTDIR)$(BINDIR)/scanbus-daemon"
	install -D -m 0755 packaging/libexec/scanbus/scanbus-scanimage \
		"$(DESTDIR)$(LIBEXECDIR)/scanbus-scanimage"

# One page per command, not just scanbus.1: the SUBCOMMANDS section of scanbus(1) is a
# list of scanbus-scan(1)-style references that `man` resolves against installed files,
# so shipping the top page alone leaves every one of them a dead link.
install-doc: $(TARGET_DIR)/scanbus
	install -d "$(DESTDIR)$(MANDIR)/man1"
	"$(TARGET_DIR)/scanbus" manpage --output-dir "$(DESTDIR)$(MANDIR)/man1"

# The packaged build, and the entry point the release workflow uses. debian/rules
# recurses back into `make release` here and stages under target/debian.
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

test:
	cargo test --workspace --features $(BACKENDS)

fmt:
	cargo fmt --all

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

clean:
	cargo clean

.PHONY: all release debug 
.PHONY: install install-backend install-frontend install-services install-doc
.PHONY: install-license install-copyright install-hplip-services
.PHONY: deb version version-check version-sync
.PHONY: reload test clean lint
