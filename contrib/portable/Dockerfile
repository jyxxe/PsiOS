# Native amd64 Rust 1.100.0 toolchain on Alpine 3.23 (musl).
FROM rust@sha256:60059ab041ce2a7628fb0814ca3671ce568243e188c4ddda7f2228805bb6784d AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates crates
COPY docs/v1/schemas docs/v1/schemas
RUN --mount=type=secret,id=cargo_ca \
    if [ -f /run/secrets/cargo_ca ]; then export CARGO_HTTP_CAINFO=/run/secrets/cargo_ca; fi; \
    cargo build --locked --release --workspace --target=x86_64-unknown-linux-musl
RUN set -eu; mkdir /out; \
    for name in psiosd psiosctl; do \
        cp "target/x86_64-unknown-linux-musl/release/$name" "/out/$name"; \
        strip "/out/$name"; \
        chmod 0755 "/out/$name"; \
        readelf --wide --program-headers --dynamic "/out/$name" > /tmp/elf; \
        if grep -Eq ' INTERP |\(NEEDED\)' /tmp/elf; then echo "$name requires a shared library or loader" >&2; exit 1; fi; \
    done

FROM scratch
COPY --from=build /out/ /
