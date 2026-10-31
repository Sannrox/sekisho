.PHONY: all test test-integration update validate
all:
	cargo build --all-targets --locked
test:
	cargo test --locked
test-integration:
	cargo test --locked --tests
update:
	@for s in $$(ls scripts/update-*.sh 2>/dev/null | sort); do bash $$s; done
validate:
	cargo fmt --check
	cargo clippy --all-targets --locked -- -D warnings
	cargo clippy --all-targets --locked --features postgres -- -D warnings
	bash scripts/validate-release.sh
