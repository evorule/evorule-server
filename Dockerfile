# syntax=docker/dockerfile:1
# evorule-server 多阶段构建（独立仓版本）
#
# 构建: docker build -t evorule-server .
# 运行: docker run -p 18080:18080 -v $(pwd)/data:/data evorule-server
#
# 此 Dockerfile 假设 build context 是 evorule-server 仓根目录,
# 包含:
#   - evorule-server/   (主 bin)
#   - core/             (server 配套 lib: auth/io_handlers/metrics/...)
#   - resources/        (CC0 资源: core_eval.json)
#   - Cargo.toml + Cargo.lock (workspace 顶层)
#
# evorule 核心 (evorule-tcb/reactor/governance) 从 crates.io 拉,
# 本地开发用 [patch.crates-io] override (见 Cargo.toml)。
#
# 由 scripts/build-docker.ps1 或 CI workflow 调用。
#
# 环境变量覆盖（优先级高于 CLI 默认值）:
#   EVORULE_ADDR, EVORULE_AUTH_TOKEN, EVORULE_LOG_LEVEL

# ===== 阶段 1: 构建 =====
FROM rust:1.92-slim AS builder

# 安装构建依赖(SQLite 开发库 + pkg-config)
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libsqlite3-dev \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# ===== 第 1 层: 复制所有 Cargo.toml + Cargo.lock(利用层缓存) =====
# workspace 顶层
COPY Cargo.toml Cargo.lock ./

# evorule-server (主 bin)
COPY evorule-server/Cargo.toml ./evorule-server/

# core/* lib
COPY core/auth/Cargo.toml ./core/auth/
COPY core/debug_control/Cargo.toml ./core/debug_control/
COPY core/hot_reload/Cargo.toml ./core/hot_reload/
COPY core/io_handlers/Cargo.toml ./core/io_handlers/
COPY core/metrics/Cargo.toml ./core/metrics/
COPY core/rule_tools/Cargo.toml ./core/rule_tools/
COPY core/semantic_invariants/Cargo.toml ./core/semantic_invariants/
COPY core/time_machine/Cargo.toml ./core/time_machine/

# ===== 第 2 层: 创建 dummy 源文件预编译依赖 =====
# evorule-server (bin)
RUN mkdir -p evorule-server/src && \
    echo "fn main() {}" > evorule-server/src/main.rs

# core/* lib
RUN mkdir -p \
        core/auth/src \
        core/debug_control/src \
        core/hot_reload/src \
        core/io_handlers/src \
        core/metrics/src \
        core/rule_tools/src \
        core/semantic_invariants/src \
        core/time_machine/src && \
    for c in auth debug_control hot_reload io_handlers metrics rule_tools semantic_invariants time_machine; do \
        echo "pub fn _dummy() {}" > core/$c/src/lib.rs; \
    done

# ===== 第 3 层: 预编译依赖(失败不阻断,因 dummy 与真实 features 可能不一致) =====
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin evorule-server || true

# ===== 第 4 层: 复制真实源码 =====
# 先清掉 dummy 文件
RUN rm -rf evorule-server/src \
           core/auth/src \
           core/debug_control/src \
           core/hot_reload/src \
           core/io_handlers/src \
           core/metrics/src \
           core/rule_tools/src \
           core/semantic_invariants/src \
           core/time_machine/src

# 复制真实源码
COPY evorule-server/src/ ./evorule-server/src/
COPY core/auth/src/ ./core/auth/src/
COPY core/debug_control/src/ ./core/debug_control/src/
COPY core/hot_reload/src/ ./core/hot_reload/src/
COPY core/io_handlers/src/ ./core/io_handlers/src/
COPY core/metrics/src/ ./core/metrics/src/
COPY core/rule_tools/src/ ./core/rule_tools/src/
COPY core/semantic_invariants/src/ ./core/semantic_invariants/src/
COPY core/time_machine/src/ ./core/time_machine/src/

# ===== 第 5 层: 真实构建 =====
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/build/target \
    cargo build --release --bin evorule-server && \
    cp /build/target/release/evorule-server /usr/local/bin/evorule-server

# ===== 阶段 2: 运行时 =====
FROM debian:bookworm-slim

# 运行时依赖:
# - libsqlite3-0: SQLite 动态库(sqlx 非 bundled 模式)
# - ca-certificates: HTTPS 请求(HTTP GET)
# - tini: 轻量 init,正确处理信号(优雅退出)
RUN apt-get update && apt-get install -y --no-install-recommends \
    libsqlite3-0 \
    ca-certificates \
    tini \
    && rm -rf /var/lib/apt/lists/*

# 复制二进制
COPY --from=builder /usr/local/bin/evorule-server /usr/local/bin/evorule-server

# 复制 CC0 资源 (本仓自带 core_eval.json)
COPY resources/core_eval.json /etc/evorule/core_eval.json

# 创建非 root 用户(安全: 容器逃逸时不获得 root 权限)
RUN useradd -r -u 1000 -m -d /home/evorule -s /usr/sbin/nologin evorule \
    && mkdir -p /data /var/log/evorule \
    && chown -R evorule:evorule /data /var/log/evorule /etc/evorule

# 数据卷(数据库 + memory handler + 日志)
VOLUME ["/data"]

EXPOSE 18080

# 默认启动配置
ENV EVORULE_ADDR=0.0.0.0:18080
ENV EVORULE_CORE_EVAL=/etc/evorule/core_eval.json
ENV EVORULE_DB_PATH=/data/evorule.db
ENV EVORULE_MEMORY_DIR=/data/memory
ENV EVORULE_LOG_LEVEL=info
# RUST_LOG 让 tracing-subscriber env-filter 工作
ENV RUST_LOG=info

USER 1000

# tini 作为 PID 1,正确转发 SIGTERM 给 evorule-server(优雅退出)
ENTRYPOINT ["/usr/bin/tini", "--", "evorule-server"]
