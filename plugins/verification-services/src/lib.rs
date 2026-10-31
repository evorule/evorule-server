// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 逻辑验证原生服务（CNF 可满足性/有限状态机可达性/DFA 等价判定）。
//!
//! # 定位
//! - 规则约束的确定性机器断言：命题约束（SAT）、状态空间性质（可达性/
//!   语言空性）、两机行为一致性（等价判定）——互补于 inference 族的
//!   规则集环检测（层次不同：本族分析显式给定的公式/机器对象，
//!   非规则集推导）；与 mechanics 公式族亦互补
//!   （数值代数 vs 逻辑判定）；独立 crate，仅经 `IoDispatcher` 挂载；
//! - 纯整数/布尔/逻辑域，无浮点、无时钟、无随机；同输入双跑位元一致。
//!
//! # 路由设计
//! - `VerificationServiceRouter` 实现 [`IoHandler`]，按 `params.service_name` 分发：
//!   - 命中原生服务名 → 调用对应原生实现（进程内，确定性）
//!   - 未命中 → 回落 `ServiceRegistryHandler`（HTTP，兼容其他外部服务）
//!
//! # 统一信封 v1
//! - 全部 3 服务成功出参含保留键 `status`（恒 `determined`）+
//!   `determinism`（恒 `bitwise_replayable`）；无舍入形态，契约不含
//!   scale/rounding（checksum 家族先例，按适用条件诚实省略）；error
//!   走框架 `Err(String)` 通道并带自诊断指引。
//!
//! # 诚实边界
//! - SAT：UNSAT 在上限内为完备判定（DPLL 穷尽搜索空间）；超上限
//!   fail-visible 拒解（资源守卫，不静默降级为启发式）；
//! - FSM 可达性：`max_steps` 受限探查与全空间完备判定以出参 `bounded`
//!   显式区分（未达 ≠ 不可达，如实标注）；
//! - FSM 等价：仅完全 DFA（转移函数全定义），不完全表 fail-visible 拒解；
//! - witness/反例词均可机械复核（沿路径逐转移验真/逐终态验接受性）。
//!
//! # 服务清单（SSOT = official_native_services.json，双侧守卫锁定）
//! 1. `verification_sat_solve` — CNF 命题可满足性判定（确定性 DPLL）
//! 2. `verification_fsm_reachability` — 显式 FSM 可达性分析
//! 3. `verification_fsm_equivalence` — 两台完全 DFA 等价判定

#![forbid(unsafe_code)]

use std::sync::Arc;

use async_trait::async_trait;
use evorule_plugin_kit::NativeServiceDef;
use evorule_reactor::{IoHandler, IoResult};
use evorule_tcb::JsonValue;

pub mod fsm;
pub mod sat;
pub mod services;

use services::{mk_fsm_equivalence, mk_fsm_reachability, mk_sat_solve};

// ============================================================================
// 声明表（SSOT 配对文件: official_native_services.json; 顺序即声明序,
// name/sensitive/description 三字段与顺序均被双侧守卫锁定）
// ============================================================================

/// 原生服务声明表（新增原生能力 = 本表追加一项 + SSOT 文件追加一项 +
/// 运行 sync 脚本同步治理侧嵌入副本）
pub const NATIVE_SERVICES: &[NativeServiceDef] = &[
    NativeServiceDef {
        name: "verification_sat_solve",
        sensitive: false,
        description: "CNF 命题可满足性判定(确定性 DPLL:变量首现序+正枝先行+单元传播,变量上限200子句上限2000;UNSAT 为完备判定,SAT 给出满足指派,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"vars":{"type":"integer","description":"变量数 n(1..200,变量按 1..n 编号)"},"clauses":{"type":"array","items":{"type":"array","items":{"type":"integer"}},"description":"CNF 子句列表,DIMACS 字面量(非零整数,负号表否定);空子句=恒假(保留语义)"}},"required":["vars","clauses"]}"#,
        ),
        make: mk_sat_solve,
    },
    NativeServiceDef {
        name: "verification_fsm_reachability",
        sensitive: false,
        description: "显式有限状态机可达性分析(指定初始状态集与目标状态集,最短步数与一条 witness 路径,可选步数上界,返回接受语言空性,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"states":{"type":"integer","description":"状态数(状态 0..states,初始状态须含 0 语义由调用方给 initial)"},"symbols":{"type":"integer","description":"符号数(符号 0..symbols)"},"transitions":{"type":"array","items":{"type":"array","items":{"type":"integer"}},"description":"完全定义转移表[state][symbol]=目标状态,行数=states 列数=symbols"},"accepting":{"type":"array","items":{"type":"integer"},"description":"接受状态列表"},"initial":{"type":"array","items":{"type":"integer"},"description":"初始状态列表(至少一个)"},"targets":{"type":"array","items":{"type":"integer"},"description":"目标状态列表(至少一个)"},"max_steps":{"type":"integer","description":"可选步数上界(受限探查,出参 bounded=true 如实标注,未达≠不可达)"}},"required":["states","symbols","transitions","accepting","initial","targets"]}"#,
        ),
        make: mk_fsm_reachability,
    },
    NativeServiceDef {
        name: "verification_fsm_equivalence",
        sensitive: false,
        description: "两台完全确定有限自动机等价判定(乘积机同步遍历,等价=true;不等价给出最短区分反例词与两机轨迹,确定性)",
        parameters: Some(
            r#"{"type":"object","properties":{"a_states":{"type":"integer"},"a_symbols":{"type":"integer"},"a_transitions":{"type":"array","items":{"type":"array","items":{"type":"integer"}},"description":"机器 A 完全定义转移表[state][symbol]"},"a_accepting":{"type":"array","items":{"type":"integer"}},"b_states":{"type":"integer"},"b_symbols":{"type":"integer"},"b_transitions":{"type":"array","items":{"type":"array","items":{"type":"integer"}},"description":"机器 B 完全定义转移表[state][symbol]"},"b_accepting":{"type":"array","items":{"type":"integer"}}},"required":["a_states","a_symbols","a_transitions","a_accepting","b_states","b_symbols","b_transitions","b_accepting"]}"#,
        ),
        make: mk_fsm_equivalence,
    },
];

/// 复合 IoHandler：`service_name` 命中原生服务名 → 原生执行；否则回落 HTTP。
///
/// 薄壳具名路由器：机制已上提 plugin-kit，本插件仅自持声明表并保持具名 API。
pub struct VerificationServiceRouter(evorule_plugin_kit::NativeServiceRouter);

impl VerificationServiceRouter {
    /// 构造复合路由。`fallback` 为未命中原生服务名时的 HTTP 处理器。
    pub fn new(fallback: Arc<dyn IoHandler>) -> Self {
        Self(evorule_plugin_kit::NativeServiceRouter::new(
            NATIVE_SERVICES,
            fallback,
        ))
    }

    /// 部署期启用子集构造。
    pub fn with_enabled(fallback: Arc<dyn IoHandler>, enabled: &[&str]) -> Result<Self, String> {
        evorule_plugin_kit::NativeServiceRouter::with_enabled(NATIVE_SERVICES, fallback, enabled)
            .map(Self)
    }

    /// 当前实例已启用的原生服务名列表(健康可见性/能力对账用)
    pub fn enabled_service_names(&self) -> Vec<&'static str> {
        self.0.enabled_service_names()
    }

    /// 原生服务名列表（server 侧服务绑定核对/能力对账用；自声明表派生）
    pub fn native_service_names() -> Vec<&'static str> {
        evorule_plugin_kit::NativeServiceRouter::native_service_names(NATIVE_SERVICES)
    }
}

#[async_trait]
impl IoHandler for VerificationServiceRouter {
    async fn execute(&self, params: &JsonValue) -> IoResult {
        self.0.execute(params).await
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    #[test]
    fn test_native_service_table_invariants() {
        // 声明表不变量:服务名唯一;每项构造子可用
        let mut names: Vec<&str> = NATIVE_SERVICES.iter().map(|d| d.name).collect();
        let total = names.len();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), total, "声明表服务名必须唯一");
        assert_eq!(total, 3, "本插件声明表应为 3 项");
        for d in NATIVE_SERVICES {
            let _ = (d.make)();
        }
    }

    #[test]
    fn test_declaration_ssot_matches_native_services() {
        // 同步守卫:SSOT 声明文件与本表 name/sensitive/description 三字段+顺序全量比对
        let raw = include_str!("../official_native_services.json");
        let doc: serde_json::Value = serde_json::from_str(raw).expect("SSOT 文件须为合法 JSON");
        let items = doc
            .get("services")
            .and_then(|v| v.as_array())
            .expect("SSOT 须含 services 数组");
        assert_eq!(
            items.len(),
            NATIVE_SERVICES.len(),
            "SSOT 条目数与声明表不一致"
        );
        for (i, (item, def)) in items.iter().zip(NATIVE_SERVICES).enumerate() {
            let name = item.get("name").and_then(|v| v.as_str()).expect("name");
            let sensitive = item
                .get("sensitive")
                .and_then(|v| v.as_bool())
                .expect("sensitive");
            let description = item
                .get("description")
                .and_then(|v| v.as_str())
                .expect("description");
            assert_eq!(name, def.name, "第 {i} 条服务名漂移");
            assert_eq!(sensitive, def.sensitive, "服务 {name} sensitive 漂移");
            assert_eq!(description, def.description, "服务 {name} description 漂移");
        }
    }

    #[test]
    fn test_parameters_contract_parses_and_covers_all() {
        for d in NATIVE_SERVICES {
            let raw = d.parameters.expect("每项声明须带 parameters 契约");
            let parsed: serde_json::Value =
                serde_json::from_str(raw).expect("parameters 须为合法 JSON");
            assert_eq!(
                parsed.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "服务 {} 契约 type 须为 object",
                d.name
            );
        }
    }

    #[tokio::test]
    async fn test_all_native_services_route_natively() {
        // 声明表每一项都必须命中原生执行(而非 HTTP 回落)——
        // fallback 用哨兵报错,任何一项落到 fallback 即失败
        struct SentinelHandler;
        #[async_trait]
        impl IoHandler for SentinelHandler {
            async fn execute(&self, _params: &JsonValue) -> IoResult {
                Err("fallback-called".to_string())
            }
        }
        let router = VerificationServiceRouter::new(Arc::new(SentinelHandler));
        for d in NATIVE_SERVICES {
            let params = JsonValue::object_from_pairs(&[
                ("service_name", JsonValue::string(d.name)),
                ("args", JsonValue::object_from_pairs(&[])),
            ]);
            // 空参数下业务结果可成功可失败,但绝不能是 fallback 哨兵
            if let Err(e) = router.execute(&params).await {
                assert_ne!(e, "fallback-called", "服务 {} 落到了 HTTP 回落", d.name);
            }
        }
    }
}
