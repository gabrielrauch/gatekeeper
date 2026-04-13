FROM rust:1.77-slim AS builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
RUN cargo build --release

FROM gcr.io/distroless/cc-debian12
COPY --from=builder /app/target/release/gatekeeper /gatekeeper
COPY config/ /config/
EXPOSE 8080
ENTRYPOINT ["/gatekeeper"]
CMD ["--config", "/config/gatekeeper.toml"]
