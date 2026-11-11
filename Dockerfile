# syntax=docker/dockerfile:1

# Frontend assets are platform-independent; build natively to avoid emulating esbuild.
FROM --platform=$BUILDPLATFORM node:22-alpine AS frontend-builder

WORKDIR /app/admin-ui
COPY admin-ui/package.json admin-ui/pnpm-lock.yaml admin-ui/.npmrc admin-ui/pnpm-workspace.yaml ./
RUN npm install -g pnpm@10.33.0
RUN --mount=type=cache,id=kiro-pnpm,target=/pnpm/store \
    pnpm install --frozen-lockfile --store-dir=/pnpm/store
COPY admin-ui ./
RUN pnpm build

FROM rust:1.92-alpine AS builder

RUN apk add --no-cache musl-dev gcc perl make

WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY --from=frontend-builder /app/admin-ui/dist /app/admin-ui/dist

# Keep dependencies and build artifacts across source/UI edits. Separate target
# caches by architecture and profile, including multi-platform Buildx builds.
ARG TARGETARCH
ARG BUILD_PROFILE=release
# Re-run rust-embed when the frontend adds/removes fingerprinted assets.
RUN --mount=type=cache,id=kiro-cargo-registry,target=/usr/local/cargo/registry \
    --mount=type=cache,id=kiro-cargo-git,target=/usr/local/cargo/git \
    --mount=type=cache,id=kiro-target-${TARGETARCH}-${BUILD_PROFILE},target=/app/target,sharing=locked \
    touch src/main.rs \
    && cargo build --locked --profile "$BUILD_PROFILE" --no-default-features \
    && mkdir -p /app/bin \
    && cp "target/$BUILD_PROFILE/kiro-rs" /app/bin/kiro-rs

FROM alpine:3.21

RUN apk add --no-cache ca-certificates

WORKDIR /app
COPY --from=builder /app/bin/kiro-rs /app/kiro-rs

VOLUME ["/app/config"]

EXPOSE 8990

CMD ["./kiro-rs", "-c", "/app/config/config.json", "--credentials", "/app/config/credentials.json"]
