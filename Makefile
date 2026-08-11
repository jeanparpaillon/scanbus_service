backends := brother,hplip,mobile
env := debug

all: build

build:
	cargo build --workspace --features $(backends)

test:
	cargo test --workspace --features $(backends)

clean:
	cargo clean

.PHONY: all build test clean
