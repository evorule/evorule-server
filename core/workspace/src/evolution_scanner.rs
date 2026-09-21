//! 进化信号扫描（产品化自进化闭环的信号源）
//!
//! **只读聚合服务**——对会话审计链（FactsLog）中的 `Fact::Violation` 事实做
//! 确定性聚合，输出进化信号列表，供 LLM 代理（evo-agent `evolution_signals`
//! 工具）与报警面消费。核心 [`scan_violations`] 为纯函数（快照入 → 信号出，
//! 无 I/O、无墙钟、无随机源），单测覆盖排序/空窗/limit/聚合边界。
//!
//! 红线核验：本模块对审计链**只读**，不扩展 Fact 枚举、不触碰
//! 哈希链、不做任何写操作；无 server 内部定时器/自治循环——扫描只在显式
//! 端点调用时执行一次。
//!
//! 确定性口径：FactsLog 无墙钟时间戳（确定性红线），信号排序以
//! `count desc → rule_ref asc` 全序确定，`last_version` 取 FactsLog 单调
//! 版本号（确定性时钟，可回放对账）。归因 [`ViolationSnapshot::rule_ref`]
//! 由 server 采集层尽力填充（当前规则集下标 → 规则 id），取不到时回退
//! `rule_index={N}` 形式——两种形态对聚合语义等价。

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use utoipa::ToSchema;

/// 单条违规快照（server 采集层从 `Fact::Violation` 构造，scanner 不感知 FactsLog）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct ViolationSnapshot {
    /// FactsLog 版本号（违规事实发生时的链上版本；单调确定时钟）
    pub version: u64,
    /// 归因规则标识：规则 id 或 `rule_index={N}`（合并列表下标回退形态）
    pub rule_ref: String,
    /// 违规说明（enforce params.reason 原文）
    pub reason: String,
    /// 被拒指令类型（instruction 的 instruction_type / type 顶层键）
    pub instr_type: String,
}

/// 发布队列现状计数（治理链只读投影）
#[derive(Debug, Clone, Default, Serialize, Deserialize, ToSchema)]
pub struct QueueCounts {
    /// 待审普通发布条数（publish_queue status=pending kind=normal）
    pub pending_normal: u64,
    /// 待审元规则晋升条数（status=pending kind=meta_promotion）
    pub pending_meta_promotion: u64,
}

/// 单条进化信号（同归因聚合；展示层摘要，不含执行语义内容）
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
pub struct EvolutionSignal {
    /// 信号类别（当前唯一取值 "violation"）
    pub kind: String,
    /// 归因规则标识（聚合键）
    pub rule_ref: String,
    /// 最近一次违规说明（同归因最新快照的 reason）
    pub reason_summary: String,
    /// 窗口内违规次数
    pub count: u64,
    /// 最近一次违规的链上版本号（FactsLog 单调时钟）
    pub last_version: u64,
    /// 最近一次被拒指令类型
    pub last_instr_type: String,
}

/// 进化信号扫描响应（只读聚合投影）
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct EvolutionSignalsResponse {
    /// 会话标识（扫描口径 = 单会话审计链）
    pub session_id: u64,
    /// 窗口内违规事实总数（聚合前的原始计数）
    pub total_violations: u64,
    /// 聚合信号列表（count desc → rule_ref asc 全序；limit 截断）
    pub signals: Vec<EvolutionSignal>,
    /// 发布队列现状（治理链 pending 计数）
    pub queue: QueueCounts,
}

/// 从被拒指令 JSON 提取指令类型（顶层 `instruction_type`，回退 `type`；均缺省 "unknown"）
pub fn extract_instr_type(instruction: &serde_json::Value) -> String {
    instruction
        .get("instruction_type")
        .or_else(|| instruction.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("unknown")
        .to_string()
}

/// 扫描核心：违规快照聚合 → 进化信号（纯函数，全序确定）
///
/// 排序：`count desc → rule_ref asc`；`limit = 0` 视为不限（返回全部）。
/// 空窗（无违规）→ 空信号列表 + 队列计数照常返回（fail-soft 口径）。
pub fn scan_violations(
    session_id: u64,
    snapshots: &[ViolationSnapshot],
    queue: QueueCounts,
    limit: usize,
) -> EvolutionSignalsResponse {
    // 聚合键 rule_ref → (count, last_version, last_reason, last_instr_type)
    let mut agg: BTreeMap<String, (u64, u64, String, String)> = BTreeMap::new();
    for s in snapshots {
        let e = agg
            .entry(s.rule_ref.clone())
            .or_insert_with(|| (0, 0, String::new(), String::new()));
        e.0 += 1;
        if s.version >= e.1 {
            // 同键取链上最新快照（版本单调，>= 保证重复版本下后到者胜）
            e.1 = s.version;
            e.2 = s.reason.clone();
            e.3 = s.instr_type.clone();
        }
    }
    let mut signals: Vec<EvolutionSignal> = agg
        .into_iter()
        .map(
            |(rule_ref, (count, last_version, reason_summary, last_instr_type))| EvolutionSignal {
                kind: "violation".to_string(),
                rule_ref,
                reason_summary,
                count,
                last_version,
                last_instr_type,
            },
        )
        .collect();
    // 全序确定：count desc → rule_ref asc
    signals.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then_with(|| a.rule_ref.cmp(&b.rule_ref))
    });
    if limit > 0 && signals.len() > limit {
        signals.truncate(limit);
    }
    EvolutionSignalsResponse {
        session_id,
        total_violations: snapshots.len() as u64,
        signals,
        queue,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap(version: u64, rule_ref: &str, reason: &str, instr: &str) -> ViolationSnapshot {
        ViolationSnapshot {
            version,
            rule_ref: rule_ref.to_string(),
            reason: reason.to_string(),
            instr_type: instr.to_string(),
        }
    }

    #[test]
    fn empty_window_yields_empty_signals_with_queue() {
        // 空窗 fail-soft：零违规 → 空清单，队列计数照常投影
        let r = scan_violations(
            7,
            &[],
            QueueCounts {
                pending_normal: 2,
                pending_meta_promotion: 1,
            },
            20,
        );
        assert_eq!(r.session_id, 7);
        assert_eq!(r.total_violations, 0);
        assert!(r.signals.is_empty());
        assert_eq!(r.queue.pending_normal, 2);
        assert_eq!(r.queue.pending_meta_promotion, 1);
    }

    #[test]
    fn aggregates_by_rule_ref_with_deterministic_order() {
        // 排序契约：count desc → rule_ref asc
        let snaps = vec![
            snap(3, "guard.b", "未放行", "robot_move"),
            snap(5, "guard.a", "越权", "robot_move"),
            snap(7, "guard.b", "未放行", "robot_move"),
            snap(9, "guard.a", "越权", "robot_move"),
            snap(11, "guard.a", "越权", "robot_move"),
        ];
        let r = scan_violations(1, &snaps, QueueCounts::default(), 0);
        assert_eq!(r.total_violations, 5);
        assert_eq!(r.signals.len(), 2, "两归因各聚一条");
        assert_eq!(r.signals[0].rule_ref, "guard.a", "count 3 > 2 排前");
        assert_eq!(r.signals[0].count, 3);
        assert_eq!(r.signals[0].last_version, 11, "取链上最新");
        assert_eq!(r.signals[0].reason_summary, "越权");
        assert_eq!(r.signals[1].rule_ref, "guard.b");
        assert_eq!(r.signals[1].count, 2);

        // 平级 tie：同 count → rule_ref asc
        let tied = vec![snap(2, "z.rule", "r", "i"), snap(3, "a.rule", "r", "i")];
        let r2 = scan_violations(1, &tied, QueueCounts::default(), 0);
        assert_eq!(r2.signals[0].rule_ref, "a.rule");
    }

    #[test]
    fn limit_truncates_zero_means_unbounded() {
        let snaps: Vec<ViolationSnapshot> = (1..=5)
            .map(|i| snap(i, &format!("r.{i}"), "x", "set"))
            .collect();
        // 全量
        assert_eq!(
            scan_violations(1, &snaps, QueueCounts::default(), 0)
                .signals
                .len(),
            5
        );
        // 截断 top-2（count 均为 1 → rule_ref asc：r.1, r.2）
        let r = scan_violations(1, &snaps, QueueCounts::default(), 2);
        assert_eq!(r.signals.len(), 2);
        assert_eq!(r.signals[0].rule_ref, "r.1");
        assert_eq!(r.signals[1].rule_ref, "r.2");
    }

    #[test]
    fn instr_type_extraction_prefers_instruction_type() {
        let v = serde_json::json!({"instruction_type": "robot_move", "type": "alias"});
        assert_eq!(extract_instr_type(&v), "robot_move");
        let v2 = serde_json::json!({"type": "branch"});
        assert_eq!(extract_instr_type(&v2), "branch");
        let v3 = serde_json::json!({});
        assert_eq!(extract_instr_type(&v3), "unknown");
    }
}
