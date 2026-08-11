BACKENDS := brother,hplip,mobile
CARGO_ENV := debug

ifeq ($(CARGO_ENV),release)
	profile_opt := --release
else
	profile_opt :=
endif

all: build

build:
	cargo build --workspace $(profile_opt) --features $(BACKENDS)

test:
	cargo test --workspace --features $(BACKENDS)

clean:
	cargo clean

.PHONY: all build test clean
