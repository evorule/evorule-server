// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 验证注入面对外端点 —— `POST /v1/criticality/verify`
//!
//! # 定位
//! 测量命题验证入口：接收 [`evorule_criticality::VerifyRequest`] 命题
//! （主张值 + SI 单位 + 不确定度 + 溯源上下文），以 law_host 装配的定律
//! 与常量集为参照执行纯函数验证管线，产出三态判定 + 逐步证据 + 提案
//! 物料包。挂 protected_routes（统一鉴权/限流/体限层之后）。
//!
//! # 安全面（设计稿 §3 注入面纪律）
//! - **判定只读（铁律 3）**：handler 对装配面零写路径——law_host 为启动期
//!   构建的不可变共享态，本端点仅调用其只读查询；判定结果只出提案
//!   物料包（人工经治理链入闸），无任何自动回写通道。
//! - **结构 fail-fast**：schema 校验 + 数值有限性/幅值 + 单位可解析
//!   全部前置；量纲不一致/超限/非法结构一律 422 结构化报因，不静默。
//! - **DoS 硬上限**：表达式长度/括号深度/标识符数/数值幅值/主张条数/
//!   定律扫描集上限由 core/criticality 管线统一执行；传输面体限/限流
//!   由挂载层统一执法。
//! - **缺省不变式**：law_host 未配置 = 503 明示（不降级为空判定）。

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};

use crate::api::server::AppState;
use evorule_criticality::{ConstantRef, LawRef, VerifyError};

/// 端点路由（挂 protected_routes：鉴权/限流/体限由装配层统一执法）
pub fn router() -> Router<AppState> {
    Router::new().route("/v1/criticality/verify", post(verify_handler))
}

/// 错误响应体（统一结构：code + message + detail）
fn error_body(code: &str, message: String, detail: Value) -> Value {
    json!({
        "error": {
            "code": code,
            "message": message,
            "detail": detail,
        }
    })
}

/// 拒判错误 → 422 响应（结构化报因：kind + 逐条 detail）
fn rejection(err: &VerifyError) -> Response {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(error_body(
            "verify_rejected",
            err.summary(),
            serde_json::to_value(err).unwrap_or(Value::Null),
        )),
    )
        .into_response()
}

/// `POST /v1/criticality/verify`
///
/// 请求体 = verify_request/v1.0 命题文档；响应 = 三态判定报告
/// （{verdict, steps, proposals}）或结构化拒因。
async fn verify_handler(State(app): State<AppState>, Json(doc): Json<Value>) -> Response {
    // 装配前置：law_host 未配置 = 明示不可用（不降级为空判定）
    let Some(host) = app.law_host() else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(error_body(
                "law_host_not_configured",
                "law_set 装配未配置：验证注入面需要 law 集与常量集装配后可用".to_string(),
                Value::Null,
            )),
        )
            .into_response();
    };

    // 结构层 fail-fast（schema + 有限性/幅值 + 单位可解析）
    let schema_report = evorule_criticality::validate_verify_request(&doc);
    if !schema_report.valid {
        let err = VerifyError::Schema {
            errors: schema_report.errors,
        };
        return rejection(&err);
    }
    let request: evorule_criticality::VerifyRequest = match serde_json::from_value(doc) {
        Ok(r) => r,
        Err(e) => {
            return rejection(&VerifyError::Schema {
                errors: vec![format!("命题反序列化失败: {e}")],
            });
        }
    };

    // 装配面注入（只读借用视图；dataset 序 = BTreeMap 序，确定性）
    let ds_ids = host.dataset_ids();
    let mut laws: Vec<LawRef<'_>> = Vec::new();
    for ds in &ds_ids {
        for law in host.laws_in(ds) {
            laws.push(LawRef {
                dataset_id: ds,
                law,
            });
        }
    }
    let constants: Vec<ConstantRef<'_>> = host
        .constant_refs()
        .into_iter()
        .map(|(dataset_id, constant)| ConstantRef {
            dataset_id,
            constant,
        })
        .collect();

    // 纯函数管线（判定只读：装配面零写路径；铁律 3 由 LawHost 不可变性保证）
    match evorule_criticality::verify(&request, &laws, &constants) {
        Ok(report) => {
            let mut body = serde_json::to_value(&report).unwrap_or(Value::Null);
            // 均衡观测记账（纯观测零拦截：只记窗计数，不改判定/响应语义）——
            // 提案入管按物料包逐件计数（与消化面逐件口径对称）；三态判定
            // 单源映射认证连计（stable=true 余 false）；alert/contradiction
            // 另记窗内警情，窗结算时清零。
            let eq = crate::equilibrium_obs::shared();
            if let Some(proposals) = body.get("proposals").and_then(|p| p.as_array()) {
                for _ in proposals {
                    eq.record_proposal();
                }
            }
            match body.get("verdict").and_then(|v| v.as_str()) {
                Some("stable") => eq.record_certificate(true),
                Some("alert") | Some("contradiction") => {
                    // 域归因（纯观测零拦截）：本判定为全域三态聚合，警情按
                    // 提案物料包 dataset_id 归账——全部提案同库 = 记入该域；
                    // 跨库多提案/缺物料包 = 不可单域归因，落全局归因桶 ""
                    // （不丢账 fail-visible，不静默）。全局计数在
                    // record_alert_in 内同步 +1，全局口径恒 = 分账之和。
                    let mut domains: std::collections::BTreeSet<&str> =
                        std::collections::BTreeSet::new();
                    if let Some(proposals) = body.get("proposals").and_then(|p| p.as_array()) {
                        for p in proposals {
                            if let Some(d) = p.get("dataset_id").and_then(|d| d.as_str()) {
                                domains.insert(d);
                            }
                        }
                    }
                    match domains.len() {
                        1 => eq.record_alert_in(domains.iter().next().copied().unwrap_or_default()),
                        _ => eq.record_alert_in(""),
                    }
                    eq.record_certificate(false);
                }
                _ => eq.record_certificate(false),
            }
            // 待重裁推进记账（纯观测零拦截）：待裁账中 law 在本报告重新产出
            // 裁决（law_substitution 步骤目标 / law_review 提案条目）即推进
            // 其重裁水位——与上方均衡记账同零拦截口径，不改裁决/响应语义。
            advance_readjudication_from_report(&body);
            // 分级裁决面（缺省关）：显式配置激活后，对 verify 提案物料包
            // 逐笔三闸裁决并在响应附 adjudications[]；低危自动出站由
            // 机器闸受端六检最终执法，降级决策只记录不改写人工路径。
            if crate::tiered_adjudicator::AdjudicatorConfig::from_env().is_some() {
                body["adjudications"] = run_tiered_adjudication(&app, &body).await;
            }
            (StatusCode::OK, Json(body)).into_response()
        }
        Err(err) => rejection(&err),
    }
}

/// 待重裁推进记账（纯观测零拦截）：扫描本报告内 law 重裁证据——
/// `law_substitution` 步骤目标（定律 id）与 `law_review` 提案条目
/// （entry_id）——对命中待裁账的 id 调 [`crate::equilibrium_obs::
/// record_readjudicated`] 推进重裁水位；未知 id 账面 no-op（纯观测）。
/// 只读报告记观测，不改裁决/响应语义。
fn advance_readjudication_from_report(report: &serde_json::Value) {
    let mut targets: std::collections::BTreeSet<&str> = std::collections::BTreeSet::new();
    if let Some(steps) = report.get("steps").and_then(|v| v.as_array()) {
        for s in steps {
            if s.get("kind").and_then(|k| k.as_str()) == Some("law_substitution") {
                if let Some(t) = s.get("target").and_then(|t| t.as_str()) {
                    targets.insert(t);
                }
            }
        }
    }
    if let Some(proposals) = report.get("proposals").and_then(|v| v.as_array()) {
        for p in proposals {
            if p.get("kind").and_then(|k| k.as_str()) == Some("law_review") {
                if let Some(e) = p.get("entry_id").and_then(|e| e.as_str()) {
                    targets.insert(e);
                }
            }
        }
    }
    for id in targets {
        crate::equilibrium_obs::record_readjudicated(id);
    }
}

// ============================================================================
// 分级裁决面（消费 verify 提案物料包；缺省关，显式配置激活）
// ============================================================================

/// 对 verify 报告内提案逐笔三闸裁决（类别层一票否决/出处闸/水位预检/
/// 配额检查），响应附 adjudications[] 可回放；`auto_integrate` 决策
/// fire-and-forget 出站（机器闸受端六检最终执法），非 2xx/传输失败
/// 仅记录不重试不吞错——人工提案路径现状零改动。
async fn run_tiered_adjudication(app: &AppState, report: &Value) -> Value {
    use crate::tiered_adjudicator::{self, AdjudicatorConfig};
    let Some(cfg) = AdjudicatorConfig::from_env() else {
        return Value::Null;
    };
    let proposals = report
        .get("proposals")
        .and_then(|p| p.as_array())
        .cloned()
        .unwrap_or_default();
    // 水位预检输入：companion 伴跑容量实测（None=尚无报告→从严拒自动）；
    // 上限与治理常数同源（TCB MAX_TRANSFORM_RULES，见 rule_schema）
    let cap_default = evorule_rule_schema::MAX_TRANSFORM_RULES as u64;
    let (rule_count, cap) = match app.companion() {
        Some(hub) => {
            let s = hub.status_json();
            (
                s["ruleset_capacity"]["rule_count"].as_u64(),
                s["ruleset_capacity"]["cap"].as_u64().unwrap_or(cap_default),
            )
        }
        None => (None, cap_default),
    };
    let quota = app.tiered_quota();
    let mut out: Vec<Value> = Vec::new();
    for p in &proposals {
        // 先做纯三闸裁决（配额前置放行），auto 决策再原子占位：
        // 占位与自动出站一一对应，human 决策不占配额
        let Some(mut d) = tiered_adjudicator::decide(p, rule_count, cap, true) else {
            continue;
        };
        if d.decision == "auto_integrate" && !quota.acquire() {
            d = crate::tiered_adjudicator::AdjudicationDecision {
                entry_id: d.entry_id,
                proposal_class: d.proposal_class,
                decision: "human_path",
                reason: "配额检查：本窗自动集成频次超限（降级人工提案入闸，fail-visible 不丢弃）"
                    .to_string(),
            };
        }
        if d.decision == "auto_integrate" {
            let dataset = p
                .get("dataset_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let entry = d.entry_id.clone();
            let class = d.proposal_class.to_string();
            let subject = p
                .get("subject")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            let cause = format!("tiered adjudication auto-integration | subject={subject}");
            let cfg2 = cfg.clone();
            tokio::spawn(async move {
                match tiered_adjudicator::post_transition_entry(
                    &cfg2, &dataset, &entry, &class, &cause,
                )
                .await
                {
                    Ok(code) if (200..300).contains(&code) => {
                        tracing::info!(
                            target: "evorule::adjudicator",
                            "tiered adjudication: auto integration accepted \
                             (entry={entry} class={class} http={code})"
                        );
                    }
                    Ok(code) => {
                        // 受端拒（422 tier 报告/429 配额等）：如实记录，
                        // 人工提案路径保持原状（不重试不吞错）
                        tracing::warn!(
                            target: "evorule::adjudicator",
                            "tiered adjudication: intake rejected, human path \
                             stands (entry={entry} class={class} http={code})"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "evorule::adjudicator",
                            "tiered adjudication: transport failed, human path \
                             stands (entry={entry}): {e}"
                        );
                    }
                }
            });
        }
        out.push(d.render());
    }
    Value::Array(out)
}

// ============================================================================
// 测试（错误体形态；端点级行为测试在 api/server.rs tests，与既有
// 401/413 测试同模块共用测试装配面）
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_body_shape_stable() {
        let body = error_body("verify_rejected", "拒绝原因".to_string(), json!({"k": 1}));
        assert_eq!(body["error"]["code"], "verify_rejected");
        assert_eq!(body["error"]["message"], "拒绝原因");
        assert_eq!(body["error"]["detail"]["k"], 1);
    }

    #[test]
    fn rejection_maps_to_422_with_structured_detail() {
        let err = VerifyError::LawScanLimit {
            count: 65,
            limit: 64,
        };
        let resp = rejection(&err);
        assert_eq!(resp.status(), StatusCode::UNPROCESSABLE_ENTITY);
    }
}
