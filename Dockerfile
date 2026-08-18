# Oxid daemon (`oxidd`) — self-hosted control plane.
#
# Static musl build -> distroless runtime: no shell, no package manager, just
# the binary. Matches SPEC.md §1's "Eficiencia Absoluta" — final image is
# tens of MB, not hundreds.
#
# Build:
#   docker build -t oxid-daemon .
#
# Run (SPEC.md §6):
#   docker run -d --name oxid-daemon \
#     -v /var/run/docker.sock:/var/run/docker.sock \
#     -v oxid-data:/data \
#     -p 8080:8080 \
#     -e OXID_WEBHOOK_SECRET=change-me \
#     -e OXID_API_TOKEN=change-me \
#     oxid-daemon
#
# See docker-compose.yml for a fuller example wired to Traefik.

# syntax=docker/dockerfile:1.7

FROM rust:1-bookworm AS builder

# git2 vendors libgit2 + OpenSSL (Cargo.toml: vendored-libgit2,
# vendored-openssl) — cmake + perl are needed to build those from source.
# musl-tools gives us a fully static binary for the distroless runtime stage.
RUN apt-get update && apt-get install -y --no-install-recommends \
        musl-tools cmake perl pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# rust-toolchain.toml pins the exact toolchain rustup activates for every
# `cargo`/`rustup` invocation below — copy it first so `target add` installs
# onto the toolchain that will actually run the build, not whatever the base
# image's default happened to be.
COPY rust-toolchain.toml ./
RUN rustup target add x86_64-unknown-linux-musl

COPY . .

RUN cargo build --release --target x86_64-unknown-linux-musl -p oxid-daemon

FROM gcr.io/distroless/static-debian12:latest AS runtime

COPY --from=builder \
    /build/target/x86_64-unknown-linux-musl/release/oxidd \
    /usr/local/bin/oxidd

ENV OXID_DATA_DIR=/data
ENV OXID_ADDR=0.0.0.0:8080

VOLUME ["/data"]
EXPOSE 8080

# Runs as root by default: it needs read/write access to the mounted
# /var/run/docker.sock, which on most hosts is owned by root or a `docker`
# group the container user would need to match anyway. Given the socket
# already grants Docker-host-level control, running as a distinct non-root
# UID buys little here — see distroless's `:nonroot` tag if your host's
# socket permissions make that workable for you.
ENTRYPOINT ["/usr/local/bin/oxidd"]
