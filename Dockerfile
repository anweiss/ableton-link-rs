# Build stage
FROM rust:latest AS builder

# Install system dependencies needed for building
RUN apt-get update && apt-get install -y \
    cmake \
    build-essential \
    pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Set the working directory in the container
WORKDIR /app

# Copy the Cargo.toml and Cargo.lock files first for better caching
COPY Cargo.toml Cargo.lock ./

# Copy the source code
COPY src/ ./src/
COPY examples/ ./examples/
COPY vendor/ ./vendor/

# Build the rusthut example in release mode
RUN cargo build --release --example rusthut

# Runtime stage
FROM debian:bookworm-slim AS runtime

# Install only runtime dependencies
RUN apt-get update && apt-get install -y \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Create a non-root user
RUN useradd -m -u 1000 rustuser

# Set the working directory
WORKDIR /app

# Copy the built binary from the builder stage
COPY --from=builder /app/target/release/examples/rusthut /app/rusthut

# Change ownership to the non-root user
RUN chown rustuser:rustuser /app/rusthut && chmod +x /app/rusthut

# Switch to non-root user
USER rustuser

# Expose the port that Ableton Link uses (by default it uses UDP port 20808)
EXPOSE 20808/udp

# Set environment variables for the container
ENV RUST_LOG=info

# Run the rusthut example
CMD ["./rusthut"]
