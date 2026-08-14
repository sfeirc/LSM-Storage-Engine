FROM rust:1-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --bin lsm-cli

FROM debian:bookworm-slim
WORKDIR /app
COPY --from=build /src/target/release/lsm-cli ./
ENTRYPOINT ["./lsm-cli"]
