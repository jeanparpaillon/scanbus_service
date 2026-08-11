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

ifeq ($(CARGO_ENV),release)
profile_opt := --release
else
profile_opt :=
endif

targets := scanbus scanbus-daemon scanbus-gui

all: $(patsubst %, $(TARGET_DIR)/%, $(targets))

release:
	$(MAKE) CARGO_ENV=release all

debug:
	$(MAKE) CARGO_ENV=debug all

$(TARGET_DIR)/scanbus:
	cargo build -p $(notdir $@) $(profile_opt)

$(TARGET_DIR)/scanbus-daemon:
	cargo build -p $(notdir $@) $(profile_opt) --features $(BACKENDS)

$(TARGET_DIR)/scanbus-gui:
	cargo build -p $(notdir $@) $(profile_opt)

install: $(TARGET_DIR)/scanbus $(TARGET_DIR)/scanbus-daemon
	install -D -m 0755 "$(TARGET_DIR)/scanbus" "$(DESTDIR)$(BINDIR)/scanbus"
	install -D -m 0755 "$(TARGET_DIR)/scanbus-daemon" "$(DESTDIR)$(BINDIR)/scanbus-daemon"
	install -D -m 0644 packaging/systemd/user/scanbus.service \
		"$(DESTDIR)$(SYSTEMD_USER_DIR)/scanbus.service"
	install -D -m 0644 packaging/dbus-1/services/org.scanbus.service \
		"$(DESTDIR)$(DBUS_SERVICE_DIR)/org.scanbus.service"
	install -D -m 0755 packaging/libexec/scanbus/scanbus-scanimage \
		"$(DESTDIR)$(LIBEXECDIR)/scanbus-scanimage"

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

.PHONY: all release debug install reload test clean lint
