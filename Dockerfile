FROM rust:1.86

WORKDIR /app
COPY Cargo.toml Cargo.toml
COPY Cargo.lock Cargo.lock
COPY src src

RUN cargo install --path .

CMD ["globed-relay"]
