//! WAL 观察面（P1 W-3）：三通路 WAL 的 server 侧包装计数与存储轻采样。
//!
//! 定位：运维观测，不改机制层——计数在 server 包装点完成（checkpoint /
//! wal_rotate 端点），采样为后台轻任务，均不触碰 vendored patch 写路径。
//! 指标注册复用 hit-stats 同款先例（经主 registry 自注册，不改 IoMetrics trait）。
//!
//! 指标：
//! - `evorule_wal_rotate_total{channel,result}` — 轮换调用计数
//!   （channel = session | reactor | shared_facts；result = ok | error）
//! - `evorule_wal_checkpoint_last_success_timestamp_seconds` — 最近一次
//!   全通道 checkpoint 成功时刻（Unix 秒；从未成功则该序列缺省为 0）
//! - `evorule_wal_file_bytes{file}` — WAL 文件大小（后台轻采样；file 标签
//!   有界防基数爆炸：shared_facts / reactor / sessions 聚合）
//! - `evorule_wal_free_bytes` — wal_dir 所在分区剩余空间（轻采样）
//!
//! 未注册（测试态/未初始化）时全局访问器返回 `None`，调用方零操作——
//! 缺省行为与未部署观察面完全一致。

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use prometheus::{IntCounterVec, IntGauge, IntGaugeVec, Opts, Registry};

/// 采样周期（秒）。轻采样：一次 read_dir + stat，60s 节奏对负担可忽略。
pub const SAMPLE_INTERVAL_SECS: u64 = 60;

static WAL_OBSERVATION: std::sync::OnceLock<Arc<WalObservation>> = std::sync::OnceLock::new();

/// 后台轻采样快照（health wal 节读取）
#[derive(Debug, Clone, serde::Serialize)]
pub struct WalStorageSnapshot {
    /// wal_dir 下 WAL 文件总数（*.wal 及其分片 *.N.wal）
    pub files: u64,
    /// WAL 文件总字节数
    pub total_bytes: u64,
    /// 分区剩余字节数（获取失败时 None——不猜，不造数）
    pub free_bytes: Option<u64>,
    /// 本次采样墙钟（Unix 毫秒）
    pub sampled_at_ms: u64,
}

/// checkpoint 累计统计（health wal 节读取）
#[derive(Debug, Clone, serde::Serialize)]
pub struct WalCheckpointStats {
    /// 全通道 checkpoint 执行总次数（含 partial failure）
    pub total: u64,
    /// 存在通道路换失败的 checkpoint 次数
    pub failed: u64,
    /// 最近一次全成功 checkpoint 墙钟（Unix 毫秒；从未成功 None）
    pub last_success_at_ms: Option<u64>,
}

pub struct WalObservation {
    rotate_total: IntCounterVec,
    checkpoint_last_success: IntGauge,
    file_bytes: IntGaugeVec,
    free_bytes: IntGauge,
    snapshot: RwLock<Option<WalStorageSnapshot>>,
    checkpoint_total: AtomicU64,
    checkpoint_failed: AtomicU64,
    checkpoint_last_success_ms: AtomicU64,
}

impl WalObservation {
    /// 注册指标到给定 registry 并设为进程级观察面（生产路径）
    ///
    /// 注册失败即返回 Err（fail-fast，不静默降级）。重复注册（进程内仅
    /// 启动期一次）由 OnceLock 语义兜底：后到者返回既有实例。
    pub fn register(registry: &Registry) -> Result<Arc<Self>, prometheus::Error> {
        if let Some(existing) = WAL_OBSERVATION.get() {
            return Ok(existing.clone());
        }
        let obs = Self::build(registry)?;
        let _ = WAL_OBSERVATION.set(obs.clone());
        Ok(obs)
    }

    /// 构造实例并注册到给定 registry（不触碰进程级全局）
    ///
    /// register 的构建核心。独立暴露（模块内私有）供测试态构建隔离实例——
    /// WAL_OBSERVATION 是进程级 OnceLock，测试若都走 register，后到者拿到
    /// 绑定他人 registry 的全局实例，本地断言随并行调度顺序漂移（W-3 存量
    /// 缺陷修复：测试态一律 build 独立实例，register 语义单独锁定）。
    fn build(registry: &Registry) -> Result<Arc<Self>, prometheus::Error> {
        let rotate_total = IntCounterVec::new(
            Opts::new(
                "evorule_wal_rotate_total",
                "WAL rotation calls by channel and result",
            ),
            &["channel", "result"],
        )?;
        let checkpoint_last_success = IntGauge::new(
            "evorule_wal_checkpoint_last_success_timestamp_seconds",
            "Unix timestamp of the last fully successful WAL checkpoint (0 = never)",
        )?;
        let file_bytes = IntGaugeVec::new(
            Opts::new(
                "evorule_wal_file_bytes",
                "WAL file size in bytes by bounded file label (shared_facts / reactor / sessions)",
            ),
            &["file"],
        )?;
        let free_bytes =
            IntGauge::new("evorule_wal_free_bytes", "Free bytes on the wal_dir filesystem")?;

        registry.register(Box::new(rotate_total.clone()))?;
        registry.register(Box::new(checkpoint_last_success.clone()))?;
        registry.register(Box::new(file_bytes.clone()))?;
        registry.register(Box::new(free_bytes.clone()))?;

        Ok(Arc::new(Self {
            rotate_total,
            checkpoint_last_success,
            file_bytes,
            free_bytes,
            snapshot: RwLock::new(None),
            checkpoint_total: AtomicU64::new(0),
            checkpoint_failed: AtomicU64::new(0),
            checkpoint_last_success_ms: AtomicU64::new(0),
        }))
    }

    /// 进程级观察面访问器（未注册时 None——调用方零操作）
    pub fn global() -> Option<&'static Arc<Self>> {
        WAL_OBSERVATION.get()
    }

    /// 轮换调用计数（server 包装点调用；rotated=false 纯内存也算 ok）
    pub fn record_rotate(&self, channel: &str, ok: bool) {
        let result = if ok { "ok" } else { "error" };
        self.rotate_total.with_label_values(&[channel, result]).inc();
    }

    /// checkpoint 计数（ok = 本轮三通路无一 error）
    pub fn record_checkpoint(&self, ok: bool) {
        self.checkpoint_total.fetch_add(1, Ordering::Relaxed);
        if !ok {
            self.checkpoint_failed.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let now_ms = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.checkpoint_last_success_ms.store(now_ms, Ordering::Relaxed);
        self.checkpoint_last_success.set((now_ms / 1000) as i64);
    }

    /// checkpoint 累计统计快照
    pub fn checkpoint_stats(&self) -> WalCheckpointStats {
        let last_ms = self.checkpoint_last_success_ms.load(Ordering::Relaxed);
        WalCheckpointStats {
            total: self.checkpoint_total.load(Ordering::Relaxed),
            failed: self.checkpoint_failed.load(Ordering::Relaxed),
            last_success_at_ms: if last_ms == 0 { None } else { Some(last_ms) },
        }
    }

    /// 后台轻采样：扫 wal_dir 统计文件大小 + 分区剩余空间
    ///
    /// file 标签有界：`shared_facts`（shared_facts.wal 及分片）、`reactor`
    /// （governance_single_reactor.wal 及分片）、`sessions`（session_*.wal
    /// 及分片聚合）——会话文件不逐个打标签，防基数爆炸。其余文件不计。
    /// 采样失败静默保留上次快照（观测面不产生告警噪声；无快照时 health
    /// 节省略 storage 细节）。
    pub fn sample(&self, wal_dir: &Path) {
        let mut files: u64 = 0;
        let mut shared_bytes: u64 = 0;
        let mut reactor_bytes: u64 = 0;
        let mut sessions_bytes: u64 = 0;

        if let Ok(entries) = std::fs::read_dir(wal_dir) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !(name.starts_with("shared_facts.")
                    || name.starts_with("governance_single_reactor.")
                    || name.starts_with("session_"))
                {
                    continue;
                }
                // 只认 WAL 形态：*.wal 与分片 *.N.wal（.wal 结尾或含 .wal. 前缀段）
                if !(name.ends_with(".wal") || name.contains(".wal.")) {
                    continue;
                }
                let len = match entry.metadata() {
                    Ok(m) if m.is_file() => m.len(),
                    _ => continue,
                };
                files += 1;
                if name.starts_with("shared_facts.") {
                    shared_bytes += len;
                } else if name.starts_with("governance_single_reactor.") {
                    reactor_bytes += len;
                } else {
                    sessions_bytes += len;
                }
            }
        }

        self.file_bytes.with_label_values(&["shared_facts"]).set(shared_bytes as i64);
        self.file_bytes.with_label_values(&["reactor"]).set(reactor_bytes as i64);
        self.file_bytes.with_label_values(&["sessions"]).set(sessions_bytes as i64);

        let free = fs4::free_space(wal_dir).ok();
        if let Some(free) = free {
            self.free_bytes.set(free as i64);
        }

        let snapshot = WalStorageSnapshot {
            files,
            total_bytes: shared_bytes + reactor_bytes + sessions_bytes,
            free_bytes: free,
            sampled_at_ms: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
        };
        if let Ok(mut slot) = self.snapshot.write() {
            *slot = Some(snapshot);
        }
    }

    /// 最近一次采样快照（未采样过 None）
    pub fn storage_snapshot(&self) -> Option<WalStorageSnapshot> {
        self.snapshot.read().ok().and_then(|s| s.clone())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]

    use super::*;

    #[test]
    fn test_record_rotate_and_checkpoint_counters() {
        // 测试态用 build 独立实例（WAL_OBSERVATION 是进程级 OnceLock，
        // 全走 register 会与并行测试互相串扰——W-3 存量缺陷修复）
        let registry = Registry::new();
        let obs = WalObservation::build(&registry).unwrap();

        obs.record_rotate("session", true);
        obs.record_rotate("session", false);
        obs.record_rotate("reactor", true);
        obs.record_checkpoint(true);
        obs.record_checkpoint(false);

        let stats = obs.checkpoint_stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.failed, 1);
        assert!(stats.last_success_at_ms.is_some());
        // register 的 OnceLock 语义（顺序无关断言）：register 返回值与
        // global() 恒同一——首个调用者创建并落全局，后到者返回既有实例
        let again = WalObservation::register(&Registry::new()).unwrap();
        let global = WalObservation::global().unwrap();
        assert_eq!(Arc::as_ptr(&again), Arc::as_ptr(global));
    }

    #[test]
    fn test_checkpoint_never_success_has_no_last_success() {
        let registry = Registry::new();
        let obs = WalObservation::build(&registry).unwrap();
        obs.record_checkpoint(false);
        let stats = obs.checkpoint_stats();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.failed, 1);
        assert!(stats.last_success_at_ms.is_none());
    }

    #[test]
    fn test_sample_counts_bounded_labels_and_sizes() {
        let registry = Registry::new();
        let obs = WalObservation::build(&registry).unwrap();
        let dir = tempfile::tempdir().unwrap();

        std::fs::write(dir.path().join("shared_facts.wal"), b"abcd").unwrap();
        std::fs::write(dir.path().join("shared_facts.1.wal"), b"ab").unwrap();
        std::fs::write(dir.path().join("governance_single_reactor.wal"), b"abc").unwrap();
        std::fs::write(dir.path().join("session_1.wal"), b"ab").unwrap();
        std::fs::write(dir.path().join("session_2.wal.3"), b"a").unwrap();
        // 非观测对象：不计
        std::fs::write(dir.path().join("shared_facts_meta.json"), b"x").unwrap();
        std::fs::write(dir.path().join("unrelated.txt"), b"xyz").unwrap();

        obs.sample(dir.path());

        let snap = obs.storage_snapshot().unwrap();
        assert_eq!(snap.files, 5);
        assert_eq!(snap.total_bytes, 4 + 2 + 3 + 2 + 1);

        let text = render(&registry);
        assert!(text.contains(r#"file="shared_facts""#));
        assert!(text.contains(r#"file="reactor""#));
        assert!(text.contains(r#"file="sessions""#));
        // 分区剩余：测试环境取不到也不造数，但 gauge 序列应已注册
        assert!(text.contains("evorule_wal_free_bytes"));
    }

    fn render(registry: &Registry) -> String {
        let encoder = prometheus::TextEncoder::new();
        encoder
            .encode_to_string(&registry.gather())
            .unwrap()
    }
}
