.PHONY: test build clean build

test:
	anchor test -- --features test-env

build:
	anchor build

clean:
	cargo clean