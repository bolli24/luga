# Playground image, see CONTRACT.md in the playground-runner repo.
# Build from the repository root:
#   docker build -f playground/Dockerfile --build-arg REVISION=$(git rev-parse HEAD) -t luga-playground .

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin luga && cp target/release/luga /usr/local/bin/luga

# The browser linter: crates/luga_wasm built for wasm32 and bound with wasm-bindgen
FROM rust:1-bookworm AS linter
RUN rustup target add wasm32-unknown-unknown
WORKDIR /src
# wasm-bindgen-cli must match the wasm-bindgen crate, so take its version from Cargo.lock. Copying only
# the lock file first keeps this slow install cached until the lock file changes.
COPY Cargo.lock .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    cargo install --locked wasm-bindgen-cli \
        --version "$(grep -A1 '^name = "wasm-bindgen"$' Cargo.lock | sed -n 's/^version = "\(.*\)"$/\1/p')"
COPY . .
# Without --remove-name-section, function names make up over 90% of the module
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --locked -p luga_wasm --target wasm32-unknown-unknown \
    && wasm-bindgen --target web --out-dir /linter --remove-name-section --remove-producers-section \
        target/wasm32-unknown-unknown/release/luga_wasm.wasm

FROM debian:bookworm-slim
# luga links with gcc against libc
RUN apt-get update \
    && apt-get install -y --no-install-recommends gcc libc6-dev \
    && rm -rf /var/lib/apt/lists/*

COPY --from=build /usr/local/bin/luga /usr/local/bin/luga
COPY --chmod=755 playground/run.sh /playground/run
COPY playground/fixtures /playground/fixtures
# Examples are the test programs; examples.toml picks which ones are shown
COPY test /playground/examples
COPY playground/examples.toml /playground/examples/examples.toml
COPY --from=linter /linter /playground/linter
COPY playground/linter/index.js /playground/linter/index.js
# A Windows checkout has CRLF line endings, which break the shebang
RUN sed -i 's/\r$//' /playground/run

ARG REVISION=unknown
LABEL playground.contract="1" \
      playground.language="luga" \
      playground.extensions=".luga" \
      playground.capabilities="multi-file" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.source="https://github.com/bolli24/luga"

USER 65534:65534
ENTRYPOINT ["/playground/run"]
