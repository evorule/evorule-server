// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 事实（Fact）定义 - 系统的原子通信单元

use evorule_tcb::JsonValue;

/// 事实唯一标识符
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct FactId(pub u64);

impl core::fmt::Display for FactId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "F{}", self.0)
    }
}

/// I/O 请求类型（与 core_eval.json 中的 io_type 对应）
///
/// v0.2.0：内部表示从 `&'static str` 改为 `Arc<str>`，支持应用层注册任意
/// io_type（如 `retrieve`/`file`/`http`）。`Arc<str>` 满足 `Clone + Eq + Hash +
/// Ord + Send + Sync`，可作 `BTreeMap`/`HashMap` key、跨线程共享、克隆廉价
/// （原子计数）。
///
/// # 失去 `Copy` 的影响
///
/// 所有按值传递处需显式 `.clone()`。典型场景：reactor 中
/// `state.register_io_request(id, io_type)` 后仍要在 `Fact::IoRequest` 与
/// `tracing::debug!` 中使用 `io_type`，需 `io_type.clone()`。
///
/// # 5 个旧 io_type
///
/// 从 `const` 改为工厂函数（`Arc::from` 非 const）。字符串值不变，故
/// `IoType::new("call_service") == IoType::call_service()` 成立，旧 WAL /
/// core_eval 无需改动。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct IoType(pub std::sync::Arc<str>);

impl IoType {
    /// 调用外部服务
    pub fn call_external() -> Self {
        Self(std::sync::Arc::from("call_external"))
    }
    /// 查询数据库
    pub fn query_db() -> Self {
        Self(std::sync::Arc::from("query_db"))
    }
    /// HTTP GET 请求
    pub fn http_get() -> Self {
        Self(std::sync::Arc::from("http_get"))
    }
    /// 保存到记忆
    pub fn save_memory() -> Self {
        Self(std::sync::Arc::from("save_memory"))
    }
    /// 调用外部服务（v0.1.x 借道路径：按 `params.service_name` 二级路由）
    pub fn call_service() -> Self {
        Self(std::sync::Arc::from("call_service"))
    }

    /// 运行时构造任意 io_type（v0.2.0 自定义 IoType 入口）
    ///
    /// 用于应用层注册自定义 io_type，如 `IoType::new("retrieve")`。
    /// 5 个旧 io_type 字符串值不变：`IoType::new("call_service") == IoType::call_service()`。
    pub fn new(name: &str) -> Self {
        Self(std::sync::Arc::from(name))
    }

    /// 从字符串解析 I/O 类型
    ///
    /// v0.2.0：无条件接受（校验责任移到 subscriber / `ReactorBuilder::known_io_types`）。
    /// 保留方法名仅为向后兼容旧调用点，行为从"未知返回 None"变为"始终返回 Some"，
    /// 避免静默破坏。
    #[deprecated(note = "v0.2.0 起用 IoType::new；parse 不再校验，保留仅为向后兼容")]
    pub fn parse(s: &str) -> Option<Self> {
        Some(Self::new(s))
    }

    /// 转为字符串
    pub fn as_str(&self) -> &str {
        self.0.as_ref()
    }
}

/// 控制流指令类型（G8 合规：唯一真值来源，位于 fact.rs 受豁免）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ControlFlowType {
    /// 顺序执行多条指令
    Sequence,
    /// 条件分支
    Conditional,
    /// 循环执行
    WhileLoop,
    /// 将指令推入队列
    Push,
}

impl ControlFlowType {
    /// 从字符串解析控制流类型
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "sequence" => Some(ControlFlowType::Sequence),
            "conditional" => Some(ControlFlowType::Conditional),
            "while_loop" => Some(ControlFlowType::WhileLoop),
            "push" => Some(ControlFlowType::Push),
            _ => None,
        }
    }

    /// 转换为字符串
    pub fn as_str(&self) -> &'static str {
        match self {
            ControlFlowType::Sequence => "sequence",
            ControlFlowType::Conditional => "conditional",
            ControlFlowType::WhileLoop => "while_loop",
            ControlFlowType::Push => "push",
        }
    }
}

impl core::fmt::Display for IoType {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// 事实 ID 生成器（单调递增）
#[derive(Debug)]
pub struct FactIdGenerator {
    next: u64,
}

impl FactIdGenerator {
    /// 创建新的生成器（从 1 开始）
    pub const fn new() -> Self {
        Self { next: 1 }
    }

    /// 从指定值恢复（崩溃恢复续跑用）
    ///
    /// `resume(n)` 后首次 `next_id()` 返回 `FactId(n)`。
    /// `resume(1)` 等价于 `new()`。
    ///
    /// 用于崩溃恢复场景：从历史 WAL 中的最大 FactId + 1 续跑，
    /// 避免新一轮产生的 FactId 与历史 WAL 中的同类型 Fact 重复。
    pub fn resume(from: u64) -> Self {
        Self { next: from }
    }

    /// 生成下一个 ID
    pub fn next_id(&mut self) -> FactId {
        let id = FactId(self.next);
        self.next += 1;
        id
    }
}

impl Default for FactIdGenerator {
    fn default() -> Self {
        Self::new()
    }
}

/// 域判定归因的 trace 条目（事实层自有类型，不绑定 tcb 类型）
///
/// 三态归因落账（专项-20261001 方案 2' v4）：branch/enforce 域求值的
/// 结果分类与 `on_missing` 静态声明值，以字符串标签承载（标签取值见
/// TCB `DomainOutcome::label` / `missing_reason` / `OnMissingPolicy::label`）。
/// R1 纪律：归因仅作审计用途，不回灌执行。
#[derive(Debug, Clone, PartialEq)]
pub struct TraceDomainAttribution {
    /// 域判定结果标签："sat" / "unsat" / "missing"
    pub outcome: String,
    /// Missing 分类标签：outcome 为 "missing" 时为 Some（"path_not_found" /
    /// "incomparable" / "value_literal_ambiguous"），否则 None
    pub missing_reason: Option<String>,
    /// 规则文本 `on_missing` 声明值："error" / "unsat"；未声明为 None
    pub on_missing: Option<String>,
}

impl TraceDomainAttribution {
    /// 从 TCB 域归因映射（构造期转换；字段仍为 String 标签，类型不绑定 tcb）
    pub fn from_tcb(attr: &evorule_tcb::domain::DomainAttribution) -> Self {
        Self {
            outcome: attr.outcome.label().into(),
            missing_reason: attr.outcome.missing_reason().map(str::to_string),
            on_missing: attr.on_missing.map(str::to_string),
        }
    }
}

/// 单条规则命中归因的 trace 条目
///
/// 事实层自有类型（不绑定 tcb 类型，保持 crate 边界解耦）。
/// `index` 为规则在合并规则列表中的下标，来源标签由装载层映射。
#[derive(Debug, Clone, PartialEq)]
pub struct TraceHit {
    /// 规则在合并规则列表中的下标
    pub index: u64,
    /// 规则顶层指令类型（如 "branch"、"set"）
    pub instr_type: String,
    /// 是否结构命中
    pub hit: bool,
    /// 域判定归因：仅 branch/enforce 指令求值成功时为 Some，其余 None
    pub domain_attr: Option<TraceDomainAttribution>,
}

impl TraceHit {
    /// 从 TCB 规则命中归因构造事实层 trace 条目（归因映射单一出口）
    pub fn from_rule_hit(hit: &evorule_tcb::transition::RuleHit) -> Self {
        Self {
            index: hit.index as u64,
            instr_type: hit.instr_type.clone(),
            hit: hit.hit,
            domain_attr: hit
                .domain_attr
                .as_ref()
                .map(TraceDomainAttribution::from_tcb),
        }
    }
}

/// 事实（Fact）—— 系统的原子通信单元
///
/// 所有组件之间仅通过事实通信，无直接函数调用。
///
/// # 设计约定
/// - 所有变体携带 `id: FactId`，用于全局唯一标识与审计追踪
/// - `StateTransition` 和 `IoRequest` 携带 `cause: FactId`，指向触发该事实的来源
/// - `IoResponse` 携带 `error: Option<String>`，None 表示成功，Some 表示 I/O 失败
#[derive(Debug, Clone, PartialEq)]
pub enum Fact {
    /// 用户提交新指令（触发执行）
    Command {
        /// 事实唯一标识符
        id: FactId,
        /// 待执行的指令对象
        instruction: JsonValue,
    },

    /// 外部更新 payload 字段（由治理层注入）
    PayloadUpdate {
        /// 事实唯一标识符
        id: FactId,
        /// 要更新的 payload 路径
        path: String,
        /// 新值
        value: JsonValue,
    },

    /// 状态转换（由反应器自动产生）
    StateTransition {
        /// 事实唯一标识符
        id: FactId,
        /// 触发此转换的源事实 ID（通常是 Command 或 IoResponse）
        cause: FactId,
        /// 转换后的新 payload
        new_payload: JsonValue,
        /// 转换后的新指令队列
        new_queue: Vec<JsonValue>,
    },

    /// I/O 请求（由 TCB 产生，由治理层消费）
    IoRequest {
        /// 事实唯一标识符，用于 IoRequest ↔ IoResponse 配对
        id: FactId,
        /// 触发此 I/O 请求的源事实 ID
        cause: FactId,
        /// I/O 类型
        io_type: IoType,
        /// 请求参数
        params: JsonValue,
    },

    /// I/O 响应（由治理层产生，由反应器消费）
    IoResponse {
        /// 事实唯一标识符
        id: FactId,
        /// 对应的 IoRequest ID
        request_id: FactId,
        /// I/O 执行结果
        result: JsonValue,
        /// I/O 错误信息（None=成功，Some=失败描述）
        error: Option<String>,
    },

    /// 系统稳定（无更多指令可执行）
    Stable {
        /// 事实唯一标识符
        id: FactId,
        /// 稳定时的会话版本号
        ///
        /// # 设计（O(n²) 修复）
        /// 原字段 `final_snapshot: JsonValue`（全量 payload 快照）在长驻会话下
        /// 每命令 O(n) 写入事实链，累计 O(n²)（实测 ~1500 命令 → 100MB WAL、
        /// 2.5s/命令）。恢复路径（FactsLog::recover）对 Stable 仅更新
        /// last_stable_version、从不读取快照内容，快照为纯冗余 → 瘦身为版本号。
        /// 状态本体由最近一条 StateTransition.new_payload 确定，消费方经
        /// snapshot API 获取，信息零丢失。
        version: u64,
    },

    /// 系统错误（超时或 TCB 内部错误）
    Error {
        /// 事实唯一标识符
        id: FactId,
        /// 错误描述
        message: String,
    },

    /// 规则命中归因轨迹（由反应器在收敛转换后追加）
    ///
    /// 每次收敛（State/Ignored）追加一条：`cause` 指向同次转换的
    /// `StateTransition` 或 `Error`(ignored) 事实；`rule_hits` 与合并
    /// 规则列表等长。`IoRequired` 中途信号**不产生**本事实（D11 重放
    /// 契约：以收敛后重放为准，避免重复计数）。
    /// 本事实不推进会话版本（记录性事实，不改变业务状态）。
    TransitionTrace {
        /// 事实唯一标识符
        id: FactId,
        /// 同次转换的 StateTransition / Error(ignored) 事实 ID
        cause: FactId,
        /// 各规则命中归因（与合并规则列表等长，按执行顺序）
        rule_hits: Vec<TraceHit>,
    },

    /// 违规被强制拦截（`enforce` 原语命中，回归验证）
    ///
    /// # 系统独占（三权分立：阻止权/记录权在系统）
    ///
    /// 本事实**仅由反应器**在收到 `TransitionResult::Halted` 时发射——
    /// LLM、规则、任何指令类型均无通道产生本事实（杜绝伪造拦截记录）。
    ///
    /// # 记录性事实（不 bump version）
    ///
    /// 与 `TransitionTrace` 同形态：不改变业务状态。违规指令被**拒绝执行**
    /// （不产生 StateTransition、不推回队列），payload/queue 保持该步前原样。
    Violation {
        /// 事实唯一标识符
        id: FactId,
        /// 触发违规的来源事实 ID（通常是 Command 或 IoResponse）
        cause: FactId,
        /// 命中的 enforce 规则在合并规则列表中的下标
        rule_index: u64,
        /// 违规说明（enforce params.reason）
        reason: String,
        /// 被拒指令完整回显（审计可追溯"谁想干什么"）
        instruction: JsonValue,
    },
}

impl Fact {
    /// 返回事实类型的字符串名称（用于日志）
    pub fn type_name(&self) -> &'static str {
        match self {
            Fact::Command { .. } => "Command",
            Fact::PayloadUpdate { .. } => "PayloadUpdate",
            Fact::StateTransition { .. } => "StateTransition",
            Fact::IoRequest { .. } => "IoRequest",
            Fact::IoResponse { .. } => "IoResponse",
            Fact::Stable { .. } => "Stable",
            Fact::Error { .. } => "Error",
            Fact::TransitionTrace { .. } => "TransitionTrace",
            Fact::Violation { .. } => "Violation",
        }
    }

    /// 返回事实的唯一标识符
    pub fn id(&self) -> FactId {
        match self {
            Fact::Command { id, .. }
            | Fact::PayloadUpdate { id, .. }
            | Fact::StateTransition { id, .. }
            | Fact::IoRequest { id, .. }
            | Fact::IoResponse { id, .. }
            | Fact::Stable { id, .. }
            | Fact::Error { id, .. }
            | Fact::TransitionTrace { id, .. }
            | Fact::Violation { id, .. } => *id,
        }
    }

    /// 是否为终止事实（Stable 或 Error）
    pub fn is_terminal(&self) -> bool {
        matches!(self, Fact::Stable { .. } | Fact::Error { .. })
    }

    /// 将 Fact 序列化为 JsonValue（供 HTTP API 返回完整 Fact 详情）
    ///
    /// 返回的 JSON 对象包含 `type` 字段和各变体的所有字段，
    /// 可通过 `tcb_to_serde` 转为 `serde_json::Value` 用于 HTTP 响应。
    //
    // 注：match 全部 Fact 变体的序列化分支，有意保持单一函数便于对账字段映射。
    #[allow(clippy::too_many_lines)]
    pub fn to_json(&self) -> JsonValue {
        use evorule_tcb::JsonValue as J;
        match self {
            Fact::Command { id, instruction } => J::object_from_pairs(&[
                ("type", J::string("Command")),
                ("id", J::integer(id.0 as i64)),
                ("instruction", instruction.clone()),
            ]),
            Fact::PayloadUpdate { id, path, value } => J::object_from_pairs(&[
                ("type", J::string("PayloadUpdate")),
                ("id", J::integer(id.0 as i64)),
                ("path", J::string(path.clone())),
                ("value", value.clone()),
            ]),
            Fact::StateTransition {
                id,
                cause,
                new_payload,
                new_queue,
            } => J::object_from_pairs(&[
                ("type", J::string("StateTransition")),
                ("id", J::integer(id.0 as i64)),
                ("cause", J::integer(cause.0 as i64)),
                ("new_payload", new_payload.clone()),
                ("new_queue", J::array(new_queue.clone())),
            ]),
            Fact::IoRequest {
                id,
                cause,
                io_type,
                params,
            } => J::object_from_pairs(&[
                ("type", J::string("IoRequest")),
                ("id", J::integer(id.0 as i64)),
                ("cause", J::integer(cause.0 as i64)),
                ("io_type", J::string(io_type.as_str())),
                ("params", params.clone()),
            ]),
            Fact::IoResponse {
                id,
                request_id,
                result,
                error,
            } => {
                let pairs: Vec<(&str, J)> = vec![
                    ("type", J::string("IoResponse")),
                    ("id", J::integer(id.0 as i64)),
                    ("request_id", J::integer(request_id.0 as i64)),
                    ("result", result.clone()),
                    (
                        "error",
                        match error {
                            Some(msg) => J::string(msg.clone()),
                            None => J::null(),
                        },
                    ),
                ];
                J::object_from_pairs(&pairs)
            }
            Fact::Stable { id, version } => J::object_from_pairs(&[
                ("type", J::string("Stable")),
                ("id", J::integer(id.0 as i64)),
                ("version", J::integer(*version as i64)),
            ]),
            Fact::Error { id, message } => J::object_from_pairs(&[
                ("type", J::string("Error")),
                ("id", J::integer(id.0 as i64)),
                ("message", J::string(message.clone())),
            ]),
            Fact::TransitionTrace {
                id,
                cause,
                rule_hits,
            } => {
                let hits: Vec<J> = rule_hits
                    .iter()
                    .map(|h| {
                        J::object_from_pairs(&[
                            ("index", J::integer(h.index as i64)),
                            ("instr_type", J::string(h.instr_type.clone())),
                            ("hit", J::Bool(h.hit)),
                        ])
                    })
                    .collect();
                J::object_from_pairs(&[
                    ("type", J::string("TransitionTrace")),
                    ("id", J::integer(id.0 as i64)),
                    ("cause", J::integer(cause.0 as i64)),
                    ("rule_hits", J::array(hits)),
                ])
            }
            Fact::Violation {
                id,
                cause,
                rule_index,
                reason,
                instruction,
            } => J::object_from_pairs(&[
                ("type", J::string("Violation")),
                ("id", J::integer(id.0 as i64)),
                ("cause", J::integer(cause.0 as i64)),
                ("rule_index", J::integer(*rule_index as i64)),
                ("reason", J::string(reason.clone())),
                ("instruction", instruction.clone()),
            ]),
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;

    #[test]
    fn test_fact_id_generator() {
        let mut gen = FactIdGenerator::new();
        assert_eq!(gen.next_id(), FactId(1));
        assert_eq!(gen.next_id(), FactId(2));
        assert_eq!(gen.next_id(), FactId(3));
    }

    #[test]
    fn test_fact_id_generator_resume() {
        let mut gen = FactIdGenerator::resume(100);
        assert_eq!(gen.next_id(), FactId(100));
        assert_eq!(gen.next_id(), FactId(101));
        assert_eq!(gen.next_id(), FactId(102));
    }

    #[test]
    fn test_fact_id_generator_resume_from_1_equals_new() {
        let mut a = FactIdGenerator::new();
        let mut b = FactIdGenerator::resume(1);
        for _ in 0..5 {
            assert_eq!(a.next_id(), b.next_id());
        }
    }

    #[test]
    fn test_fact_type_name_and_id() {
        let fact = Fact::Command {
            id: FactId(1),
            instruction: JsonValue::empty_object(),
        };
        assert_eq!(fact.type_name(), "Command");
        assert_eq!(fact.id(), FactId(1));

        let fact = Fact::Stable {
            id: FactId(2),
            version: 0,
        };
        assert_eq!(fact.type_name(), "Stable");
        assert_eq!(fact.id(), FactId(2));
        assert!(fact.is_terminal());
    }

    #[test]
    fn test_fact_cause_field() {
        let transition = Fact::StateTransition {
            id: FactId(10),
            cause: FactId(5),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        };
        assert_eq!(transition.id(), FactId(10));

        let io_req = Fact::IoRequest {
            id: FactId(20),
            cause: FactId(10),
            io_type: IoType::call_external(),
            params: JsonValue::empty_object(),
        };
        assert_eq!(io_req.id(), FactId(20));
    }

    #[test]
    fn test_io_response_with_error() {
        let ok_resp = Fact::IoResponse {
            id: FactId(1),
            request_id: FactId(2),
            result: JsonValue::string("ok"),
            error: None,
        };
        assert_eq!(ok_resp.type_name(), "IoResponse");
        assert!(!ok_resp.is_terminal());

        let err_resp = Fact::IoResponse {
            id: FactId(2),
            request_id: FactId(2),
            result: JsonValue::Null,
            error: Some("timeout".to_string()),
        };
        assert_eq!(err_resp.id(), FactId(2));
    }

    #[test]
    fn test_io_type_roundtrip() {
        // 所有 IoType 变体的 new ↔ as_str 往返
        for expected in [
            IoType::call_external(),
            IoType::query_db(),
            IoType::http_get(),
            IoType::save_memory(),
            IoType::call_service(),
        ] {
            let s = expected.as_str();
            let constructed = IoType::new(s);
            assert_eq!(constructed, expected, "roundtrip failed for {}", s);
        }
    }

    #[test]
    fn test_io_type_parse_accepts_any() {
        // v0.2.0: parse 无条件接受（校验责任移到 subscriber / known_io_types）
        #[allow(deprecated)]
        {
            assert!(IoType::parse("unknown").is_some());
            assert!(IoType::parse("retrieve").is_some());
            assert!(IoType::parse("call_service").is_some());
            // 空字符串也被接受（校验由上层负责）
            assert!(IoType::parse("").is_some());
        }
    }

    #[test]
    fn test_io_type_new_equals_factory() {
        // v0.2.0: new 构造与工厂函数对相同字符串应相等（HashMap/BTreeMap key 一致性）
        assert_eq!(IoType::new("call_service"), IoType::call_service());
        assert_eq!(IoType::new("call_external"), IoType::call_external());
        assert_eq!(IoType::new("retrieve"), IoType::new("retrieve"));
        assert_ne!(IoType::new("retrieve"), IoType::new("file"));
    }

    #[test]
    fn test_io_type_display() {
        assert_eq!(format!("{}", IoType::call_external()), "call_external");
        assert_eq!(format!("{}", IoType::query_db()), "query_db");
        assert_eq!(format!("{}", IoType::http_get()), "http_get");
        assert_eq!(format!("{}", IoType::save_memory()), "save_memory");
        assert_eq!(format!("{}", IoType::call_service()), "call_service");
    }

    #[test]
    fn test_fact_id_display() {
        assert_eq!(format!("{}", FactId(0)), "F0");
        assert_eq!(format!("{}", FactId(1)), "F1");
        assert_eq!(format!("{}", FactId(42)), "F42");
    }

    #[test]
    fn test_violation_fact_to_json_and_meta() {
        // 回归验证：Violation 事实序列化 + 元信息（type_name/id/is_terminal）
        let fact = Fact::Violation {
            id: FactId(9),
            cause: FactId(3),
            rule_index: 2,
            reason: "违规：禁删数据集".to_string(),
            instruction: JsonValue::object_from_pairs(&[("type", JsonValue::string("delete_all"))]),
        };

        assert_eq!(fact.type_name(), "Violation");
        assert_eq!(fact.id(), FactId(9));
        // 记录性事实：非终止（不结束会话，会话继续处理队列）
        assert!(!fact.is_terminal());

        let json = fact.to_json();
        assert_eq!(json.get("type"), Some(&JsonValue::string("Violation")));
        assert_eq!(json.get("id"), Some(&JsonValue::Integer(9)));
        assert_eq!(json.get("cause"), Some(&JsonValue::Integer(3)));
        assert_eq!(json.get("rule_index"), Some(&JsonValue::Integer(2)));
        assert_eq!(
            json.get("reason"),
            Some(&JsonValue::string("违规：禁删数据集"))
        );
        assert_eq!(
            json.get("instruction").and_then(|v| v.get("type")),
            Some(&JsonValue::string("delete_all"))
        );
    }

    #[test]
    fn test_fact_is_terminal_all_variants() {
        // 终止事实
        assert!(Fact::Stable {
            id: FactId(1),
            version: 0,
        }
        .is_terminal());
        assert!(Fact::Error {
            id: FactId(2),
            message: "err".to_string(),
        }
        .is_terminal());

        // 非终止事实
        assert!(!Fact::Command {
            id: FactId(3),
            instruction: JsonValue::empty_object(),
        }
        .is_terminal());
        assert!(!Fact::PayloadUpdate {
            id: FactId(4),
            path: "x".to_string(),
            value: JsonValue::Null,
        }
        .is_terminal());
        assert!(!Fact::StateTransition {
            id: FactId(5),
            cause: FactId(0),
            new_payload: JsonValue::empty_object(),
            new_queue: vec![],
        }
        .is_terminal());
        assert!(!Fact::IoRequest {
            id: FactId(6),
            cause: FactId(0),
            io_type: IoType::call_external(),
            params: JsonValue::empty_object(),
        }
        .is_terminal());
        assert!(!Fact::IoResponse {
            id: FactId(7),
            request_id: FactId(6),
            result: JsonValue::Null,
            error: None,
        }
        .is_terminal());
    }

    #[test]
    fn test_fact_type_name_all_variants() {
        assert_eq!(
            Fact::Command {
                id: FactId(1),
                instruction: JsonValue::empty_object(),
            }
            .type_name(),
            "Command"
        );
        assert_eq!(
            Fact::PayloadUpdate {
                id: FactId(1),
                path: "x".to_string(),
                value: JsonValue::Null,
            }
            .type_name(),
            "PayloadUpdate"
        );
        assert_eq!(
            Fact::StateTransition {
                id: FactId(1),
                cause: FactId(0),
                new_payload: JsonValue::empty_object(),
                new_queue: vec![],
            }
            .type_name(),
            "StateTransition"
        );
        assert_eq!(
            Fact::IoRequest {
                id: FactId(1),
                cause: FactId(0),
                io_type: IoType::call_external(),
                params: JsonValue::empty_object(),
            }
            .type_name(),
            "IoRequest"
        );
        assert_eq!(
            Fact::IoResponse {
                id: FactId(1),
                request_id: FactId(0),
                result: JsonValue::Null,
                error: None,
            }
            .type_name(),
            "IoResponse"
        );
        assert_eq!(
            Fact::Stable {
                id: FactId(1),
                version: 0,
            }
            .type_name(),
            "Stable"
        );
        assert_eq!(
            Fact::Error {
                id: FactId(1),
                message: "e".to_string(),
            }
            .type_name(),
            "Error"
        );
    }

    #[test]
    fn test_fact_id_generator_default() {
        // Default 应从 0 开始，next_id 从 1 开始
        let mut gen = FactIdGenerator::default();
        assert_eq!(gen.next_id(), FactId(1));
    }
}
