# Makes sure the nightly-2023-06-01 toolchain is installed
# Reference: https://github.com/gattaca-com/helix/blob/f9e2fe84a22cb83f89ed6222a78cae8d8d07ae6b/.github/workflows/lint.yml#L31-L36
toolchain := "nightly-2023-06-01"

fmt:
  rustup toolchain install {{toolchain}} > /dev/null 2>&1 && \
  cargo +{{toolchain}} fmt

fmt-check:
  rustup toolchain install {{toolchain}} > /dev/null 2>&1 && \
  cargo +{{toolchain}} fmt --all --check

clippy:
  cargo clippy --all-features --no-deps -- -D warnings
