FROM rust:1.89.0 AS builder
RUN rustup target add wasm32-unknown-unknown && cargo install trunk
WORKDIR /app
COPY . .
RUN cd frontend && trunk build --release
RUN cargo build --release -p server

FROM debian:trixie-slim
RUN apt-get update \
    && apt-get install -y ca-certificates openssl \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /app/target/release/server ./server
COPY --from=builder /app/frontend/dist ./dist
RUN openssl req -x509 -newkey rsa:4096 \
        -keyout /app/key.pem \
        -out /app/cert.pem \
        -days 3650 \
        -nodes \
        -subj "/CN=v-note"
ENV TLS_CERT=/app/cert.pem
ENV TLS_KEY=/app/key.pem
ENV STATIC_DIR=/app/dist
EXPOSE 443
CMD ["./server"]
