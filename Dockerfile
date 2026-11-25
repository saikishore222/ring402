FROM rust:1.95-bookworm AS build
WORKDIR /app
ARG CARGO_BUILD_JOBS
COPY . .
RUN cargo build --release --locked --example demo

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN useradd --system --no-create-home demo
COPY --from=build /app/target/release/examples/demo /usr/local/bin/ring402-demo
USER demo
ENV HOST=0.0.0.0 PORT=80
EXPOSE 80
CMD ["ring402-demo"]
