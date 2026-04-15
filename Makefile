check:
	cargo check --workspace

clippy:
	cargo clippy --workspace --all-targets -- -D warnings

test:
	cargo test --workspace

fmt:
	cargo fmt --all -- --check
