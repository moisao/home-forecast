# syntax=docker/dockerfile:1
#
# home-forecast: 24 hourly point forecasts from NOAA RRFS/HRRR, as JSON.
# Published by .github/workflows/ci.yml as ghcr.io/moisao/home-forecast for
# linux/amd64 and linux/arm64.
#
# Compiled on the build machine's own architecture and cross-linked for the
# other, rather than emulated under QEMU (a Rust release build there takes
# many times longer).
#
#   docker build -t home-forecast .
#   docker run --rm -p 8080:8080 -e LOCATIONS="little-black=32.988,-117.122" home-forecast

FROM --platform=$BUILDPLATFORM rust:1.99-slim-trixie AS build
ARG BUILDARCH TARGETARCH
RUN if [ "$BUILDARCH" != "$TARGETARCH" ]; then \
        case "$TARGETARCH" in \
            arm64) pkgs="gcc-aarch64-linux-gnu libc6-dev-arm64-cross" target=aarch64-unknown-linux-gnu ;; \
            amd64) pkgs="gcc-x86-64-linux-gnu libc6-dev-amd64-cross" target=x86_64-unknown-linux-gnu ;; \
            *) echo "unsupported $TARGETARCH" >&2; exit 1 ;; \
        esac; \
        apt-get update \
        && apt-get install -y --no-install-recommends $pkgs \
        && rm -rf /var/lib/apt/lists/* \
        && rustup target add "$target"; \
    fi
ENV CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER=aarch64-linux-gnu-gcc \
    CC_aarch64_unknown_linux_gnu=aarch64-linux-gnu-gcc \
    CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER=x86_64-linux-gnu-gcc \
    CC_x86_64_unknown_linux_gnu=x86_64-linux-gnu-gcc
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/src/target,id=target-$TARGETARCH \
    case "$TARGETARCH" in \
        arm64) target=aarch64-unknown-linux-gnu ;; \
        amd64) target=x86_64-unknown-linux-gnu ;; \
    esac \
    && if [ "$BUILDARCH" = "$TARGETARCH" ]; then \
        unset CARGO_TARGET_AARCH64_UNKNOWN_LINUX_GNU_LINKER CC_aarch64_unknown_linux_gnu \
              CARGO_TARGET_X86_64_UNKNOWN_LINUX_GNU_LINKER CC_x86_64_unknown_linux_gnu; \
    fi \
    && cargo build --release --locked --target "$target" \
    && cp "target/$target/release/home-forecast" /home-forecast

# glibc and CA certificates, nothing else; runs as uid 65532.
FROM gcr.io/distroless/cc-debian13:nonroot
COPY --from=build /home-forecast /home-forecast
EXPOSE 8080
ENTRYPOINT ["/home-forecast"]
CMD ["serve"]
