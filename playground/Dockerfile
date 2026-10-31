# Playground image, see CONTRACT.md in the playground-runner repo.
# Build from the repository root:
#   docker build -f playground/Dockerfile --build-arg REVISION=$(git rev-parse HEAD) -t luga-playground .

FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/src/target \
    cargo build --release --bin luga && cp target/release/luga /usr/local/bin/luga

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
# A Windows checkout has CRLF line endings, which break the shebang
RUN sed -i 's/\r$//' /playground/run && mkdir /playground/linter

ARG REVISION=unknown
LABEL playground.contract="1" \
      playground.language="luga" \
      playground.extensions=".luga" \
      playground.capabilities="multi-file" \
      org.opencontainers.image.revision="${REVISION}" \
      org.opencontainers.image.source="https://github.com/bolli24/luga"

USER 65534:65534
ENTRYPOINT ["/playground/run"]
