# Release image. Deploy by digest (image@sha256:...), never by tag.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked

FROM gcr.io/distroless/cc-debian12:nonroot
COPY --from=build /src/target/release/sekisho /usr/local/bin/sekisho
ENTRYPOINT ["/usr/local/bin/sekisho"]
CMD ["serve", "--config", "/etc/sekisho/sekisho.toml"]
