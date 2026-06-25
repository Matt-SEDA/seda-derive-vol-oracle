.PHONY: build check clean fmt

clean:
	cargo clean

fmt:
	cargo +nightly fmt --all

check:
	cargo clippy --all-features --locked -- -D warnings

build:
	cargo build --target wasm32-wasip1 --profile release-wasm
	wasm-strip target/wasm32-wasip1/release-wasm/derive-vol-oracle.wasm;
	wasm-opt -Oz --enable-bulk-memory --enable-nontrapping-float-to-int target/wasm32-wasip1/release-wasm/derive-vol-oracle.wasm -o target/wasm32-wasip1/release-wasm/derive-vol-oracle.wasm;

install-tools:
	bun install
