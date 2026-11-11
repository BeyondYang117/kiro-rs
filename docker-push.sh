#!/usr/bin/env bash
set -euo pipefail

usage() {
    cat <<'EOF'
用法: ./docker-push.sh [标签 ...]

构建 linux/amd64 和 linux/arm64 镜像并推送到 Docker Hub abu116/kiro-rs。
不传标签时使用 latest；多个标签共享同一次构建。

示例:
  docker login -u abu116
  ./docker-push.sh
  ./docker-push.sh 2026.10.1 latest

前置条件: Docker 已启动、已安装 Buildx、已登录具有仓库推送权限的账号。
Docker Desktop 默认支持跨架构模拟；Linux 主机需预先配置 QEMU/binfmt。
可通过 BUILDER_NAME 环境变量指定 Buildx builder，默认 kiro-rs-multiarch。
可通过 PLATFORMS 指定目标架构，例如 ARM 机器只发布本机架构：
  PLATFORMS=linux/arm64 ./docker-push.sh
EOF
}

if [[ "${1:-}" == "--help" || "${1:-}" == "-h" ]]; then
    usage
    exit 0
fi

if [[ "$#" -eq 0 ]]; then
    set -- latest
fi

image="abu116/kiro-rs"
builder_name="${BUILDER_NAME:-kiro-rs-multiarch}"
platforms="${PLATFORMS:-linux/amd64,linux/arm64}"
project_dir="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
tag_args=()

for tag in "$@"; do
    if [[ ! "$tag" =~ ^[a-zA-Z0-9_][a-zA-Z0-9_.-]{0,127}$ ]]; then
        printf '错误：无效的 Docker 标签：%s\n' "$tag" >&2
        exit 1
    fi
    tag_args+=(--tag "${image}:${tag}")
done

if ! command -v docker >/dev/null 2>&1; then
    printf '错误：未找到 Docker，请先安装 Docker 和 Buildx。\n' >&2
    exit 1
fi

if ! docker info >/dev/null 2>&1; then
    printf '错误：无法连接 Docker，请先启动 Docker 并检查当前 context。\n' >&2
    exit 1
fi

if ! docker buildx version >/dev/null 2>&1; then
    printf '错误：未找到 Docker Buildx，请先安装 Buildx 插件。\n' >&2
    exit 1
fi

if ! docker buildx inspect "$builder_name" >/dev/null 2>&1; then
    docker buildx create --name "$builder_name" --driver docker-container
fi

docker buildx inspect "$builder_name" --bootstrap

printf '开始构建并推送 %s 镜像。请确保已执行 docker login -u abu116。\n' "$platforms"
docker buildx build \
    --builder "$builder_name" \
    --platform "$platforms" \
    --file "$project_dir/Dockerfile" \
    "${tag_args[@]}" \
    --push \
    "$project_dir"

for tag in "$@"; do
    printf '已推送：%s:%s\n' "$image" "$tag"
done
