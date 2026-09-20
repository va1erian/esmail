# Standalone container form of mail-mock-server, for manual/exploratory
# testing of esmail against a real (if fake-data) IMAP + SMTP server. CI
# does NOT use this image -- it runs the server in-process from the test
# binary instead, which is faster and needs no Docker-in-Docker. This
# image is for `docker compose up` at the repo root; see
# crates/mail-mock-server/README.md.

FROM rust:1.90-slim-bookworm AS build
WORKDIR /build

# native-tls (used for the IMAP server's TLS identity) links against
# system OpenSSL via openssl-sys, which needs its headers and pkg-config to
# build -- neither is in the base image.
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Cargo needs every workspace member's Cargo.toml present to resolve the
# lockfile even though `-p mail-mock-server` below only *compiles* this one
# package -- `mail-mock-server` doesn't depend on `esmail` or
# `egui-litehtml-webview`, so their source is never actually built, just
# parsed as manifests. Build context is the repo
# root (see docker-compose.yml's `context: .`) for exactly this reason; see
# also this repo's root `.dockerignore`, which keeps that context from
# including the multi-gigabyte cargo `target/` directory.
COPY . .
RUN cargo build --release -p mail-mock-server

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /build/target/release/mail-mock-server /usr/local/bin/mail-mock-server

EXPOSE 1993 1025
ENV IMAP_BIND=0.0.0.0:1993
ENV SMTP_BIND=0.0.0.0:1025
ENTRYPOINT ["/usr/local/bin/mail-mock-server"]
