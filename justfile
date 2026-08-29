set default-list := true

build:
    cargo build

fmt:
    cargo fmt --all

clippy:
    cargo clippy --all-targets -- -D warnings
