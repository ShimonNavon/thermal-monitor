FROM rust:alpine AS build
RUN apk add --no-cache musl-dev
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --target x86_64-unknown-linux-musl

FROM scratch
COPY --from=build /src/target/x86_64-unknown-linux-musl/release/thermal-monitor /thermal-monitor
ENTRYPOINT ["/thermal-monitor"]
CMD ["serve"]
