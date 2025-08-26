# Generated Dockerfile - Do not edit manually
# Project: app (rust)
# Cache invalidation starts at step: none
FROM ubuntu:18.04 AS base

# Set environment variables
ENV DEBIAN_FRONTEND=noninteractive
ENV RUST_VERSION=1.86.0
ENV PATH="/root/.cargo/bin:${PATH}"

# Proxy environment variables
ENV http_proxy=http://192.168.1.108:7890
ENV https_proxy=http://192.168.1.108:7890
ENV no_proxy=localhost,127.0.0.1,192.168.,10.126.

# Update package lists (always at the beginning for base layer caching)
RUN apt-get update

# Install base system packages (cached layer)
RUN apt-get install -y \
    curl \
    build-essential \
    pkg-config \
    libssl-dev \
    ca-certificates \
    gnupg \
    lsb-release \
    && rm -rf /var/lib/apt/lists/*

# Install Python3 and pip (cached layer)
RUN apt-get update && apt-get install -y python3 python3-pip \
    && rm -rf /var/lib/apt/lists/*

# Install Rust (cached layer)
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --default-toolchain 1.86.0 \
    && rustup component add rustfmt clippy

# Install APT package: build-essential (step 1) 
FROM base AS step_1
RUN apt-get update && apt-get install -y build-essential \
    && rm -rf /var/lib/apt/lists/*

# Install APT package: ca-certificates (step 2) 
FROM base AS step_2
RUN apt-get update && apt-get install -y ca-certificates \
    && rm -rf /var/lib/apt/lists/*

# Install APT package: curl (step 3) 
FROM base AS step_3
RUN apt-get update && apt-get install -y curl \
    && rm -rf /var/lib/apt/lists/*

# Install APT package: git (step 4) 
FROM base AS step_4
RUN apt-get update && apt-get install -y git \
    && rm -rf /var/lib/apt/lists/*

# Install APT package: gnupg (step 5) 
FROM base AS step_5
RUN apt-get update && apt-get install -y gnupg \
    && rm -rf /var/lib/apt/lists/*

# Install APT package: libssl-dev (step 6) 
FROM base AS step_6
RUN apt-get update && apt-get install -y libssl-dev \
    && rm -rf /var/lib/apt/lists/*

# Install APT package: lsb-release (step 7) 
FROM base AS step_7
RUN apt-get update && apt-get install -y lsb-release \
    && rm -rf /var/lib/apt/lists/*

# Install APT package: pkg-config (step 8) 
FROM base AS step_8
RUN apt-get update && apt-get install -y pkg-config \
    && rm -rf /var/lib/apt/lists/*

# Install APT package: protobuf-compiler (step 9) 
FROM base AS step_9
RUN apt-get update && apt-get install -y protobuf-compiler \
    && rm -rf /var/lib/apt/lists/*

# Install PIP package: pyyaml (step 10) 
FROM step_9 AS step_10
RUN pip3 install pyyaml

# Install PIP package: requests (step 11) 
FROM step_10 AS step_11
RUN pip3 install requests

# Final build stage
FROM step_11 AS build

# Set working directory
WORKDIR /app

# Copy Cargo files for dependency caching
COPY Cargo.toml Cargo.lock ./

# Create src directory and dummy main for dependency compilation
RUN mkdir src && echo "fn main() {}" > src/main.rs

# Build dependencies (cached layer)
RUN cargo build --release && rm -rf src/

# Copy source code
COPY src/ src/

# Build the application
RUN cargo build --release && echo "v1"

# Keep container alive
CMD ["tail", "-f", "/dev/null"]
