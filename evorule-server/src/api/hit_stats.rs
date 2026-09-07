// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 规则命中统计聚合器
//!
//! 消费 reactor 发出的 [`Fact::TransitionTrace`] 归因事实，按
//! `规则集版本 × 来源 × 规则下标` 维护命中计数，并暴露：
//!
//! - Prometheus 指标：`evorule_rule_hits_total{rule,source,version}`、
//!   `evorule_rules_zero_hits`（注册注入的 registry 时才启用）
//! - 查询面数据：全量清单（含零命中筛选）与单规则跨版本时序切片
//!
//! # 语义口径
//! - "命中" = 引擎结构命中（直接指令执行成功 / branch 所选分支存在且非空 /
//!   io_request 产生信号），口径由引擎 `TransitionResult` 的 `rule_hits` 定义，
//!   本模块只做计数不重判。
//! - "零命中" = 当前（或指定）版本 layout 中存在、但聚合期内从未命中过的规则。
//!   死规则检测的直接数据源（治理哲学：报警面扩容——静默通过清剿）。
//! - 聚合存储 = 进程内存（重启归零）；审计链 WAL 是全量权威源，历史回溯
//!   走审计档案（37 号考古 #14 的"统计派生视图"后续项不在本条）。
//! - 广播事件落后（Lagged）时计数可能偏低——计数是尽力而为的运行时信号，
//!   不承载审计责任；warn 留痕不静默。
//!
//! # 下标 → 规则身份的解析
//! 引擎归因只携带合并规则列表的下标（与 `execute_transition` 输入等长）。
//! server 侧以 [`RulesetLayout`] 快照（core_eval 在前 + rules_dir 按文件名字典序，
//! 与引擎合并顺序一致）把下标解析为 `source`（"core_eval" 或相对文件路径）+
//! `instr_type`（规则顶层指令类型）。reload 换版后 layout 切换，历史计数按
//! 版本分桶互不混淆。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{SystemTime, UNIX_EPOCH};

use evorule_reactor::{Fact, TraceHit};
use evorule_tcb::JsonValue;
use prometheus::{IntCounterVec, IntGauge, Opts};
use serde::Serialize;
use utoipa::ToSchema;

/// 保留的规则集 layout 快照上限（reload 频率低，8 个足够覆盖近期版本切片）
const MAX_LAYOUTS: usize = 8;

/// 规则集版本哈希输入的域分隔前缀（防跨域哈希复用）
const VERSION_HASH_DOMAIN: &str = "evorule-hit-stats-v1";

/// 单条规则的元数据（layout 快照内）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, ToSchema)]
pub struct RuleMeta {
    /// 规则来源标签：`core_eval`（宪法规则集）或 rules_dir 下相对文件路径
    pub source: String,
    /// 规则顶层指令类型（如 "branch"、"set"；缺失记 "unknown"）
    pub instr_type: String,
}

/// 合并规则集快照：把引擎归因下标解析为规则身份
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RulesetLayout {
    /// 规则集版本（对合并列表的确定性哈希，reload 换版即切换）
    pub ruleset_version: String,
    /// 规则元数据（与合并规则列表等长、按执行顺序）
    pub rules: Vec<RuleMeta>,
}

impl RulesetLayout {
    /// 从合并规则列表 + 等长来源标签构造 layout
    ///
    /// 版本哈希 = BLAKE3(`VERSION_HASH_DOMAIN` + 规则数 + 各"规则序列化串|来源标签")
    /// —— 内容或来源任一变化都产生新版本（来源变化会改变下标→身份解析，
    /// 必须换版分桶），同一规则集得到同一版本号，与加载路径无关。
    pub fn from_rules(rules: &[JsonValue], sources: Vec<String>) -> Self {
        // zip 以较短侧为准（等长契约由调用方保证；防御性不 panic）
        let mut hasher = blake3::Hasher::new();
        hasher.update(VERSION_HASH_DOMAIN.as_bytes());
        hasher.update(&rules.len().to_le_bytes());
        let metas: Vec<RuleMeta> = rules
            .iter()
            .zip(sources)
            .map(|(rule, source)| {
                hasher.update(rule.to_string().as_bytes());
                hasher.update(b"|");
                hasher.update(source.as_bytes());
                RuleMeta {
                    source,
                    instr_type: rule
                        .get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown")
                        .to_string(),
                }
            })
            .collect();
        let full = hasher.finalize().to_hex().to_string();
        Self {
            ruleset_version: full[..16].to_string(),
            rules: metas,
        }
    }
}

/// 聚合计数键：`(规则集版本, 来源, 规则下标)`
type StatKey = (String, String, u64);

/// 单规则的聚合统计
#[derive(Debug, Clone)]
pub struct RuleStat {
    /// 结构命中次数
    pub hit_count: u64,
    /// 首次命中的记录序号（进程内单调）
    pub first_hit_seq: u64,
    /// 最近命中的记录序号
    pub last_hit_seq: u64,
    /// 最近命中墙钟时间（Unix 毫秒；时钟源为聚合观察时刻，非引擎事实内容）
    pub last_hit_at_ms: u64,
}

/// 聚合器内部状态（写锁保护；trace 频率 = 每命令 1 条，锁竞争可忽略）
struct Inner {
    current: Arc<RulesetLayout>,
    /// 已知 layout 快照（含 current；按采用顺序，超限淘汰最旧）
    layouts: Vec<Arc<RulesetLayout>>,
    stats: HashMap<StatKey, RuleStat>,
    seq: u64,
    rule_hits_total: Option<IntCounterVec>,
    rules_zero_hits: Option<IntGauge>,
}

/// 规则命中统计聚合器（Clone = 共享同一状态）
#[derive(Clone)]
pub struct HitStatsAggregator {
    inner: Arc<RwLock<Inner>>,
}

impl HitStatsAggregator {
    /// 创建未注册指标的聚合器（单元测试/内存模式）
    pub fn new(initial_layout: RulesetLayout) -> Self {
        Self::with_collectors(initial_layout, None, None)
    }

    /// 创建并注册 Prometheus 指标到给定 registry（生产路径）
    ///
    /// 指标命名见模块文档 D10；注册失败即返回 Err（fail-fast，不静默降级）。
    pub fn with_registry(
        initial_layout: RulesetLayout,
        registry: &prometheus::Registry,
    ) -> Result<Self, prometheus::Error> {
        let rule_hits_total = IntCounterVec::new(
            Opts::new(
                "evorule_rule_hits_total",
                "Rule structural-hit count by rule index, source and ruleset version",
            ),
            &["rule", "source", "version"],
        )?;
        let rules_zero_hits = IntGauge::new(
            "evorule_rules_zero_hits",
            "Rules with zero structural hits in the current ruleset version",
        )?;
        registry.register(Box::new(rule_hits_total.clone()))?;
        registry.register(Box::new(rules_zero_hits.clone()))?;
        Ok(Self::with_collectors(
            initial_layout,
            Some(rule_hits_total),
            Some(rules_zero_hits),
        ))
    }

    fn with_collectors(
        initial_layout: RulesetLayout,
        rule_hits_total: Option<IntCounterVec>,
        rules_zero_hits: Option<IntGauge>,
    ) -> Self {
        let current = Arc::new(initial_layout);
        let agg = Self {
            inner: Arc::new(RwLock::new(Inner {
                current: current.clone(),
                layouts: vec![current],
                stats: HashMap::new(),
                seq: 0,
                rule_hits_total,
                rules_zero_hits,
            })),
        };
        agg.refresh_zero_hits_gauge();
        agg
    }

    /// 采用新规则集 layout（启动装载 / reload 换版时调用）
    pub fn adopt_layout(&self, layout: RulesetLayout) {
        let same_version = {
            let mut inner = self
                .inner
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if inner.current.ruleset_version == layout.ruleset_version {
                true
            } else {
                // 回滚到历史版本时去重：移除同名版本旧快照，避免切片重复
                let new_layout = Arc::new(layout);
                inner
                    .layouts
                    .retain(|l| l.ruleset_version != new_layout.ruleset_version);
                inner.current = new_layout.clone();
                let current = inner.current.clone();
                inner.layouts.push(current);
                if inner.layouts.len() > MAX_LAYOUTS {
                    // 淘汰最旧且非 current 的快照
                    inner.layouts.remove(0);
                }
                false
            }
        };
        if !same_version {
            tracing::info!(
                version = %self.current_version(),
                "规则集 layout 已切换（hit-stats 版本分桶更新）"
            );
        }
        self.refresh_zero_hits_gauge();
    }

    /// 当前规则集版本号
    pub fn current_version(&self) -> String {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .current
            .ruleset_version
            .clone()
    }

    /// 记录一条 TransitionTrace 归因（每次收敛转换 1 条）
    pub fn record_trace(&self, rule_hits: &[TraceHit]) {
        let now_ms = wall_now_ms();
        let mut inner = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        inner.seq = inner.seq.wrapping_add(1);
        let seq = inner.seq;
        let version = inner.current.ruleset_version.clone();
        let layout = inner.current.clone();
        for hit in rule_hits {
            if !hit.hit {
                continue;
            }
            let Some(meta) = layout.rules.get(hit.index as usize) else {
                tracing::warn!(
                    index = hit.index,
                    version = %version,
                    "hit-stats: 归因下标越界（layout 与引擎规则列表不等长？），该条丢弃"
                );
                continue;
            };
            let key = (version.clone(), meta.source.clone(), hit.index);
            let stat = inner.stats.entry(key).or_insert_with(|| RuleStat {
                hit_count: 0,
                first_hit_seq: seq,
                last_hit_seq: seq,
                last_hit_at_ms: now_ms,
            });
            stat.hit_count = stat.hit_count.saturating_add(1);
            stat.last_hit_seq = seq;
            stat.last_hit_at_ms = now_ms;
            if let Some(counter) = &inner.rule_hits_total {
                counter
                    .with_label_values(&[&hit.index.to_string(), &meta.source, &version])
                    .inc();
            }
        }
        if let Some(gauge) = &inner.rules_zero_hits {
            let zero = count_zero_rules(&inner, &version);
            gauge.set(zero as i64);
        }
    }

    /// 重算并设置当前版本零命中规则数 gauge
    fn refresh_zero_hits_gauge(&self) {
        let inner = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(gauge) = &inner.rules_zero_hits {
            let zero = count_zero_rules(&inner, &inner.current.ruleset_version);
            gauge.set(zero as i64);
        }
    }

    /// 全量清单快照（含零命中筛选）
    ///
    /// `version` 为 None 时取当前版本；指定未知版本返回 None（404 语义）。
    pub fn snapshot(&self, version: Option<&str>, filter: HitFilter) -> Option<HitStatsResponse> {
        let inner = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let layout = match version {
            Some(v) => inner
                .layouts
                .iter()
                .find(|l| l.ruleset_version == v)
                .cloned()?,
            None => inner.current.clone(),
        };
        let ver = layout.ruleset_version.clone();
        let mut entries = Vec::new();
        for (index, meta) in layout.rules.iter().enumerate() {
            if let Some(stat) = inner
                .stats
                .get(&(ver.clone(), meta.source.clone(), index as u64))
            {
                entries.push(HitStatEntry {
                    source: meta.source.clone(),
                    index: index as u64,
                    instr_type: meta.instr_type.clone(),
                    hit_count: stat.hit_count,
                    first_hit_seq: stat.first_hit_seq,
                    last_hit_seq: stat.last_hit_seq,
                    last_hit_at_ms: stat.last_hit_at_ms,
                });
            }
        }
        let zero_hits = zero_rules_of(&layout, &inner.stats, &ver);
        Some(HitStatsResponse {
            ruleset_version: ver,
            total_rules: layout.rules.len() as u64,
            generated_at_ms: wall_now_ms(),
            entries: match filter {
                HitFilter::All | HitFilter::Hits => entries,
                HitFilter::Zero => Vec::new(),
            },
            zero_hits: match filter {
                HitFilter::All | HitFilter::Zero => zero_hits,
                HitFilter::Hits => Vec::new(),
            },
        })
    }

    /// 单规则跨版本时序切片
    pub fn rule_series(&self, source: &str, index: u64) -> RuleSeriesResponse {
        let inner = self
            .inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut series = Vec::new();
        let mut hit_total = 0u64;
        let mut instr_type = String::from("unknown");
        for layout in &inner.layouts {
            let Some(meta) = layout.rules.get(index as usize) else {
                continue;
            };
            if meta.source != source {
                continue;
            }
            if instr_type == "unknown" {
                instr_type = meta.instr_type.clone();
            }
            let stat =
                inner
                    .stats
                    .get(&(layout.ruleset_version.clone(), source.to_string(), index));
            // 已知版本都进切片：有统计给计数，无统计记 0（零命中版本可见）
            series.push(VersionStatEntry {
                ruleset_version: layout.ruleset_version.clone(),
                hit_count: stat.map(|s| s.hit_count).unwrap_or(0),
                first_hit_seq: stat.map(|s| s.first_hit_seq),
                last_hit_seq: stat.map(|s| s.last_hit_seq),
                last_hit_at_ms: stat.map(|s| s.last_hit_at_ms),
            });
            hit_total = hit_total.saturating_add(stat.map(|s| s.hit_count).unwrap_or(0));
        }
        RuleSeriesResponse {
            source: source.to_string(),
            index,
            instr_type,
            hit_total,
            series,
        }
    }
}

/// 当前版本内零命中规则数（gauge/内部复用；持锁调用）
fn count_zero_rules(inner: &Inner, version: &str) -> u64 {
    let Some(layout) = inner.layouts.iter().find(|l| l.ruleset_version == version) else {
        return 0;
    };
    zero_rules_of(layout, &inner.stats, version).len() as u64
}

/// 零命中清单 = layout 规则全集 - 聚合期内命中过的键
fn zero_rules_of(
    layout: &RulesetLayout,
    stats: &HashMap<StatKey, RuleStat>,
    version: &str,
) -> Vec<ZeroHitEntry> {
    layout
        .rules
        .iter()
        .enumerate()
        .filter(|(index, meta)| {
            !stats.contains_key(&(version.to_string(), meta.source.clone(), *index as u64))
        })
        .map(|(index, meta)| ZeroHitEntry {
            source: meta.source.clone(),
            index: index as u64,
            instr_type: meta.instr_type.clone(),
        })
        .collect()
}

/// Unix 毫秒墙钟（聚合观察时刻；时钟未设置时记 0，不 panic）
fn wall_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// hit-stats 事件记录循环：订阅会话/单反应器 event 通道，消费 TransitionTrace
///
/// 通道关闭（会话结束）自动退出；Lagged 时 warn 留痕（计数尽力而为，不承载审计责任）。
pub async fn run_recorder(mut rx: tokio::sync::broadcast::Receiver<Fact>, agg: HitStatsAggregator) {
    loop {
        match rx.recv().await {
            Ok(Fact::TransitionTrace { rule_hits, .. }) => agg.record_trace(&rule_hits),
            Ok(_) => {}
            Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!(
                    dropped = n,
                    "hit-stats: 事件流落后丢帧（计数可能偏低；审计链不受影响）"
                );
            }
            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
        }
    }
}

/// 清单筛选语义
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitFilter {
    /// 全部：命中清单 + 零命中清单
    All,
    /// 仅命中过的规则
    Hits,
    /// 仅零命中（死规则候选）
    Zero,
}

impl HitFilter {
    /// 解析查询参数；空值 = All；未知值返回 None（400 语义）
    pub fn parse(raw: Option<&str>) -> Option<Self> {
        match raw {
            None | Some("") | Some("all") => Some(Self::All),
            Some("hit") | Some("hits") => Some(Self::Hits),
            Some("zero") => Some(Self::Zero),
            Some(_) => None,
        }
    }
}

/// GET /api/rules/hit-stats 响应
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HitStatsResponse {
    /// 规则集版本（layout 哈希前 16 hex）
    pub ruleset_version: String,
    /// 该版本合并规则总数
    pub total_rules: u64,
    /// 响应生成墙钟（Unix 毫秒）
    pub generated_at_ms: u64,
    /// 命中过的规则统计（filter=zero 时为空）
    pub entries: Vec<HitStatEntry>,
    /// 零命中规则清单（filter=hits 时为空）
    pub zero_hits: Vec<ZeroHitEntry>,
}

/// 单条规则的命中统计条目
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct HitStatEntry {
    /// 规则来源标签：`core_eval` 或 rules_dir 相对文件路径
    pub source: String,
    /// 合并规则列表下标
    pub index: u64,
    /// 规则顶层指令类型
    pub instr_type: String,
    /// 结构命中次数
    pub hit_count: u64,
    /// 首次命中记录序号（进程内单调）
    pub first_hit_seq: u64,
    /// 最近命中记录序号
    pub last_hit_seq: u64,
    /// 最近命中墙钟（Unix 毫秒）
    pub last_hit_at_ms: u64,
}

/// 零命中规则条目（死规则候选）
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct ZeroHitEntry {
    pub source: String,
    pub index: u64,
    pub instr_type: String,
}

/// GET /api/rules/hit-stats/{rule_key} 响应：单规则跨版本切片
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RuleSeriesResponse {
    pub source: String,
    pub index: u64,
    pub instr_type: String,
    /// 各版本命中计数之和
    pub hit_total: u64,
    /// 每个已知版本一条（无统计的版本计 0，零命中版本可见）
    pub series: Vec<VersionStatEntry>,
}

/// 单规则在单一版本下的统计切片
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct VersionStatEntry {
    pub ruleset_version: String,
    pub hit_count: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_hit_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_hit_seq: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_hit_at_ms: Option<u64>,
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;

    fn rule(t: &str) -> JsonValue {
        JsonValue::object_from_pairs(&[("type", JsonValue::string(t))])
    }

    fn layout() -> RulesetLayout {
        RulesetLayout::from_rules(
            &[rule("branch"), rule("set"), rule("collect")],
            vec![
                "core_eval".to_string(),
                "core_eval".to_string(),
                "rules/bundles/expenses.json".to_string(),
            ],
        )
    }

    fn hit(index: u64) -> TraceHit {
        TraceHit {
            index,
            instr_type: "branch".to_string(),
            hit: true,
        }
    }

    fn miss(index: u64) -> TraceHit {
        TraceHit {
            index,
            instr_type: "set".to_string(),
            hit: false,
        }
    }

    #[test]
    fn test_layout_version_is_content_hash() {
        let a = layout();
        let b = layout();
        assert_eq!(a.ruleset_version, b.ruleset_version, "同内容同版本");
        let swapped = RulesetLayout::from_rules(
            &[rule("set"), rule("branch"), rule("collect")],
            vec![
                "core_eval".to_string(),
                "core_eval".to_string(),
                "rules/bundles/expenses.json".to_string(),
            ],
        );
        assert_ne!(a.ruleset_version, swapped.ruleset_version, "内容变则版本变");
        assert_eq!(a.rules[0].source, "core_eval");
        assert_eq!(a.rules[2].source, "rules/bundles/expenses.json");
    }

    #[test]
    fn test_record_and_snapshot() {
        let agg = HitStatsAggregator::new(layout());
        agg.record_trace(&[hit(0), miss(1), hit(2)]);
        agg.record_trace(&[hit(0), miss(1), miss(2)]);
        let snap = agg.snapshot(None, HitFilter::All).unwrap();
        assert_eq!(snap.total_rules, 3);
        // 规则 0 命中 2 次
        let e0 = snap
            .entries
            .iter()
            .find(|e| e.index == 0)
            .expect("rule 0 should be recorded");
        assert_eq!(e0.hit_count, 2);
        // 规则 1 未命中过 → 零命中清单
        assert!(snap.entries.iter().all(|e| e.index != 1));
        let zero: Vec<&ZeroHitEntry> = snap.zero_hits.iter().filter(|z| z.index == 1).collect();
        assert_eq!(zero.len(), 1);
        // filter=zero 时 entries 为空；规则 2 在第一条 trace 中命中过 → 仅规则 1 零命中
        let snap_zero = agg.snapshot(None, HitFilter::Zero).unwrap();
        assert!(snap_zero.entries.is_empty());
        assert_eq!(snap_zero.zero_hits.len(), 1);
    }

    #[test]
    fn test_version_buckets_isolated_on_reload() {
        let agg = HitStatsAggregator::new(layout());
        agg.record_trace(&[hit(0), miss(1), miss(2)]);
        let v1 = agg.current_version();
        // reload：规则内容变化 → 新版本
        let new_layout = RulesetLayout::from_rules(
            &[rule("branch"), rule("merge"), rule("collect")],
            vec![
                "core_eval".to_string(),
                "core_eval".to_string(),
                "rules/bundles/expenses.json".to_string(),
            ],
        );
        agg.adopt_layout(new_layout);
        let v2 = agg.current_version();
        assert_ne!(v1, v2);
        agg.record_trace(&[miss(0), hit(1), miss(2)]);
        // 旧版本切片不变（规则 0 仍为 1 次命中）
        let old = agg.snapshot(Some(&v1), HitFilter::All).unwrap();
        let e0 = old.entries.iter().find(|e| e.index == 0).unwrap();
        assert_eq!(e0.hit_count, 1);
        // 新版本切片：规则 1 命中 1 次，规则 0 零命中
        let cur = agg.snapshot(None, HitFilter::All).unwrap();
        assert!(cur.entries.iter().all(|e| e.index != 0));
        assert_eq!(
            cur.entries.iter().find(|e| e.index == 1).unwrap().hit_count,
            1
        );
    }

    #[test]
    fn test_rule_series_across_versions() {
        let agg = HitStatsAggregator::new(layout());
        agg.record_trace(&[hit(1), miss(0), miss(2)]);
        let v1 = agg.current_version();
        // reload：来源变化（规则 2 换文件）→ 新版本（规则 1 来源不变，切片可跨版本）
        let new_layout = RulesetLayout::from_rules(
            &[rule("branch"), rule("set"), rule("collect")],
            vec![
                "core_eval".to_string(),
                "core_eval".to_string(),
                "rules/bundles/other.json".to_string(),
            ],
        );
        agg.adopt_layout(new_layout);
        let v2 = agg.current_version();
        assert_ne!(v1, v2);
        agg.record_trace(&[hit(1), miss(0), miss(2)]);
        let series = agg.rule_series("core_eval", 1);
        assert_eq!(series.hit_total, 2);
        assert_eq!(series.series.len(), 2);
        assert_eq!(series.series[0].ruleset_version, v1);
        assert_eq!(series.series[0].hit_count, 1);
        assert_eq!(series.series[1].ruleset_version, v2);
        assert_eq!(series.series[1].hit_count, 1);
    }

    #[test]
    fn test_same_layout_readopt_no_duplicate_series() {
        let agg = HitStatsAggregator::new(layout());
        agg.record_trace(&[hit(0), miss(1), miss(2)]);
        agg.adopt_layout(layout()); // 同版本重复采用
        agg.record_trace(&[hit(0), miss(1), miss(2)]);
        let series = agg.rule_series("core_eval", 0);
        assert_eq!(series.series.len(), 1, "同版本不新增切片");
        assert_eq!(series.hit_total, 2);
    }

    #[test]
    fn test_unknown_version_returns_none() {
        let agg = HitStatsAggregator::new(layout());
        assert!(agg.snapshot(Some("deadbeef"), HitFilter::All).is_none());
    }

    #[test]
    fn test_filter_parse() {
        assert_eq!(HitFilter::parse(None), Some(HitFilter::All));
        assert_eq!(HitFilter::parse(Some("zero")), Some(HitFilter::Zero));
        assert_eq!(HitFilter::parse(Some("hit")), Some(HitFilter::Hits));
        assert_eq!(HitFilter::parse(Some("bogus")), None);
    }

    #[test]
    fn test_index_out_of_bounds_is_dropped_not_panic() {
        let agg = HitStatsAggregator::new(layout());
        agg.record_trace(&[hit(99)]); // 越界：丢弃不 panic
        let snap = agg.snapshot(None, HitFilter::All).unwrap();
        assert!(snap.entries.is_empty());
        assert_eq!(snap.zero_hits.len(), 3);
    }

    #[test]
    fn test_zero_hits_gauge_with_registry() {
        let registry = prometheus::Registry::new();
        let agg = HitStatsAggregator::with_registry(layout(), &registry).unwrap();
        agg.record_trace(&[hit(0), miss(1), miss(2)]);
        let text = {
            let enc = prometheus::TextEncoder::new();
            enc.encode_to_string(&registry.gather()).unwrap()
        };
        assert!(text.contains("evorule_rule_hits_total"));
        assert!(text.contains("evorule_rules_zero_hits 2"));
    }
}
