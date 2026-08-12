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

ifeq ($(CARGO_ENV),release)
profile_opt := --release
else
profile_opt :=
endif

binaries := scanbus scanbus-daemon scanbus-gui
targets := $(patsubst %, $(TARGET_DIR)/%, $(binaries))

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

install: install-backend install-frontend install-services install-doc

install-services:
	install -D -m 0644 packaging/systemd/user/scanbus.service \
		"$(DESTDIR)$(SYSTEMD_USER_DIR)/scanbus.service"
	install -D -m 0644 packaging/dbus-1/services/org.scanbus.service \
		"$(DESTDIR)$(DBUS_SERVICE_DIR)/org.scanbus.service"

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

install-doc: target/$(CARGO_ENV)/scanbus 
	target/$(CARGO_ENV)/scanbus manpage > \
		"$(DESTDIR)$(PREFIX)/share/man/man1/scanbus.1"

reload:
	systemctl --user daemon-reload
	dbus-send --session --type=method_call --dest=org.freedesktop.DBus \
		/ org.freedesktop.DBus.ReloadConfig

test:
	cargo test --workspace --features $(BACKENDS)

lint:
	cargo clippy --workspace --all-targets --all-features -- -D warnings

clean:
	cargo clean

.PHONY: all release debug 
.PHONY: install install-backend install-frontend install-services install-doc
.PHONY: reload test clean lint
