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
#   - resources/        (CC0 资源: server_eval.json)
#   - Cargo.toml + Cargo.lock (workspace 顶层)
#
# evorule 核心 (evorule-tcb/reactor/governance) 从 crates.io 拉,
# 本地开发用 [patch.crates-io] override (见 Cargo.toml)。
#
# 由 scripts/build-docker.ps1 或 CI workflow 调用。
#
# 环境变量覆盖（优先级高于 CLI 默认值）:
#   EVORULE_ADDR, EVORULE_AUTH_TOKEN, EVORULE_SERVICE_TOKEN, EVORULE_LOG_LEVEL

# ===== 阶段 1: 构建 =====
FROM rust:1.92-slim AS builder

# 安装构建依赖(SQLite 开发库 + OpenSSL + curl(utoipa-swagger-ui 构建时下载) + pkg-config)
RUN apt-get update && apt-get install -y --no-install-recommends \
    pkg-config \
    libsqlite3-dev \
    libssl-dev \
    curl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# ===== 全量源码复制 =====
# 旧版采用 dummy 源文件预编译技巧(manifest 层 + dummy src 预编译依赖层),
# 存在三重缺陷,已废弃:
#   1. `cargo build || true` 静默吞错(违反 fail-fast 原则,历史债务清剿红线);
#   2. CI 每次全新 runner 无 docker 层缓存,预编译层每次全量白编,时长翻倍;
#   3. dummy 与真实 features 漂移时预编译失败或 mtime 误判 fresh
#      (touch 补丁即为此而生),脆弱难维护。
# 现简化为 COPY . . 单次真实构建:CI 一次编译(~10-15min)在 job 限额内;
# 本地开发依赖缓存由宿主 cargo target/ 增量承担,不依赖 docker 层缓存。
COPY . .

RUN cargo build --release --bin evorule-server && \
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

# 复制 CC0 资源 (本仓自带 server_eval.json;旧名 core_eval.json 保留兼容检测)
COPY resources/server_eval.json /etc/evorule/server_eval.json

# 创建非 root 用户(安全: 容器逃逸时不获得 root 权限)
RUN useradd -r -u 1000 -m -d /home/evorule -s /usr/sbin/nologin evorule \
    && mkdir -p /data /var/log/evorule \
    && chown -R evorule:evorule /data /var/log/evorule /etc/evorule

# 数据卷(数据库 + memory handler + 日志)
VOLUME ["/data"]

EXPOSE 18080

# 默认启动配置
# ⚠️ B3 fail-closed:绑定 0.0.0.0 且未设 EVORULE_AUTH_TOKEN 时,server 将拒绝
# 启动(exit 1,含自诊断指引)——生产部署必须传 token:
#   docker run -p 18080:18080 -e EVORULE_AUTH_TOKEN=<your-secret> ...
# 仅本地开发可改绑 loopback 免认证: -e EVORULE_ADDR=127.0.0.1:18080(容器内
# loopback 无法端口映射,不适用于 docker run -p 场景)
ENV EVORULE_ADDR=0.0.0.0:18080
ENV EVORULE_CORE_EVAL=/etc/evorule/server_eval.json
ENV EVORULE_DB_PATH=/data/evorule.db
ENV EVORULE_MEMORY_DIR=/data/memory
ENV EVORULE_LOG_LEVEL=info
# RUST_LOG 让 tracing-subscriber env-filter 工作
ENV RUST_LOG=info

USER 1000

# tini 作为 PID 1,正确转发 SIGTERM 给 evorule-server(优雅退出)
ENTRYPOINT ["/usr/bin/tini", "--", "evorule-server"]
