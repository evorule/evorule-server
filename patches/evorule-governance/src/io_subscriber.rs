// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! I/O 订阅者 - 订阅 event broadcast 通道，过滤 IoRequest 事实，执行 I/O，回写 IoResponse
//!
//! # 工作流程
//! 1. 从 EventReceiver 接收 Fact
//! 2. 过滤 `Fact::IoRequest { id, io_type, params, .. }`
//! 3. 调用 `IoDispatcher::dispatch` 执行实际 I/O
//! 4. 通过 FactSender 发送 `Fact::IoResponse { id, request_id, result, error }` 回反应器
//! 5. 其他 Fact 类型忽略（不阻塞，继续接收）
//!
//! # ID 分配策略
//! `IoSubscriber` 维护独立的 ID 计数器，从 `10000` 起步，避免与反应器自身的
//! `FactIdGenerator`（从 1 开始）发生 ID 冲突，便于审计追踪时区分事实来源。
//!
//! # 错误处理
//! - `RecvError::Lagged`：订阅者落后于 broadcast 通道容量，记录 `warn` 并继续
//! - `RecvError::Closed`：通道已关闭，正常退出循环
//! - `SendError`：command 通道关闭（反应器已退出），返回 `IoSubscriberError::CommandClosed`

use std::sync::Arc;
use std::time::{Duration, Instant};

use evorule_reactor::{EventReceiver, Fact, FactId, FactSender, IoCallContext, IoType};
use evorule_tcb::JsonValue;

use crate::circuit_breaker::{CircuitState, CIRCUIT_BREAKER_ATTR};
use crate::io_dispatcher::IoDispatcher;
use crate::metrics::{NoOpMetrics, SharedMetrics};
use crate::permission::{PermissionGate, Verdict};

/// ID 起始偏移量，避免与反应器自身的 FactId 冲突
const ID_OFFSET: u64 = 10000;

/// 最大重试次数（P0-2：瞬时错误指数退避重试，最多 3 次，即总计 4 次尝试）
const MAX_RETRIES: u32 = 3;
/// 初始退避时长（P0-2：指数退避 200ms → 400ms → 800ms）
const INITIAL_BACKOFF: Duration = Duration::from_millis(200);

/// 判断 I/O 错误是否可重试（P0-2）
///
/// 仅瞬时错误重试：超时、连接问题、HTTP 5xx 服务端错误。
/// 客户端错误（4xx、参数缺失、工具未找到）不重试——重试不会改变结果。
fn is_retryable_error(err: &str) -> bool {
    let lower = err.to_lowercase();
    // 超时（reqwest tokio timeout、db timeout 等）
    if lower.contains("timeout") || lower.contains("timed out") {
        return true;
    }
    // 连接问题（connection refused / reset / closed）
    if lower.contains("connection") {
        return true;
    }
    // 瞬时服务端错误
    if lower.contains("temporarily") || lower.contains("temporary") {
        return true;
    }
    // HTTP 5xx 服务端错误
    for code in ["500", "502", "503", "504"] {
        if lower.contains(code) {
            return true;
        }
    }
    false
}

/// I/O 订阅者错误
#[derive(Debug, thiserror::Error)]
pub enum IoSubscriberError {
    /// Event broadcast 通道关闭（所有发送端已释放）
    ///
    /// 此错误在 `run()` 内部已被处理为正常退出（`Ok(())`），保留枚举变体以供
    /// 调用方在自定义流程中显式表达该语义。
    #[error("Event channel closed")]
    ChannelClosed,

    /// Command 通道关闭：反应器已退出，无法回写 `IoResponse`
    ///
    /// 携带错误上下文描述（如正在处理的 request_id）。
    #[error("Command channel closed: {0}")]
    CommandClosed(String),
}

/// 跳过谓词：返回 `true` 时该 IoRequest **不由本订阅者自动应答**，
/// 留给外部执行者（如审计桥的浏览器/agent 侧本地 LLM 执行）处理。
pub type SkipPredicate = Arc<dyn Fn(&IoType, &JsonValue) -> bool + Send + Sync>;

/// I/O 订阅者
///
/// 订阅反应器的 event broadcast 通道，过滤出 `Fact::IoRequest`，交由
/// `IoDispatcher` 执行实际 I/O，并通过 command 通道回写 `Fact::IoResponse`。
///
/// # 设计要点
/// - 持有自己的 ID 计数器（从 `10000` 起），与反应器 ID 隔离
/// - 对 `Lagged` 容错（记录日志并继续），不中断订阅循环
/// - 非阻塞处理：忽略非 `IoRequest` 类型 Fact，不影响其他订阅者
/// - 可选 `skip` 谓词：命中时**不自动应答**（不回写任何 IoResponse），
///   留给外部执行者处理。用于 LLM 审计形态的 `call_external`（prompt 全文
///   经命令事实入审计链，结果由外部执行者经 `submit_io_response` 回写）——
///   若本订阅者抢先以"missing service_name"错误应答，外部执行者的
///   io_response 将被反应器忽略（Unknown IoResponse），审计回路永远失败
///
/// # 示例
/// ```ignore
/// use evorule_governance::{IoDispatcher, IoSubscriber};
/// // dispatcher 由具体 handler 构造
/// let dispatcher: IoDispatcher = /* ... */;
/// let subscriber = IoSubscriber::new(dispatcher);
/// let event_rx = event_tx.subscribe();
/// subscriber.run(event_rx, command_tx).await.ok();
/// ```
pub struct IoSubscriber {
    /// I/O 分发器
    dispatcher: IoDispatcher,
    /// 下一个 FactId（独立计数器，从 10000 起）
    next_id: u64,
    /// I/O 指标收集器（默认 NoOpMetrics，应用层可通过 `with_metrics()` 注入 Prometheus 实现）
    metrics: SharedMetrics,
    /// 可选跳过谓词（默认 None = 全部自动应答，行为与历史版本一致）
    skip: Option<SkipPredicate>,
    /// 可选 I/O 权限前置判定门（ / B6）。
    /// 默认 None = 不做权限判定（行为与历史版本一致）；注入后每次自动分发的
    /// IoRequest 在 dispatch 前按权限快照做"入口仲裁"（`PermissionGate::check`）。
    gate: Option<PermissionGate>,
    /// 可选会话级熔断状态（SG-3 family-②「白名单外锚点连发」）。
    /// 默认 None = 熔断面关闭（行为与 SG-2 一致）；注入后静态层 deny 按滑动
    /// 窗口计数，达阈值跳闸：熔断信号指令提交会话（规则层 enforce 拒链入链）
    /// + 跳闸持续期内 egress 全量拒绝，窗口过期自动复位。
    circuit: Option<CircuitState>,
}

impl IoSubscriber {
    /// 创建新的订阅者
    ///
    /// ID 计数器从 `10000` 起，避免与反应器的 `FactIdGenerator` 冲突。
    /// 默认使用 `NoOpMetrics`（不收集指标），应用层可通过 `with_metrics()` 注入实现。
    pub fn new(dispatcher: IoDispatcher) -> Self {
        Self {
            dispatcher,
            next_id: ID_OFFSET,
            metrics: Arc::new(NoOpMetrics),
            skip: None,
            gate: None,
            circuit: None,
        }
    }

    /// 注入 I/O 权限前置判定门（builder 模式； / B6）
    ///
    /// 注入后，本订阅者自动分发的每条 IoRequest 在 dispatch 之前先经
    /// [`PermissionGate::check`] 做"入口仲裁"（而非事后审计）：
    /// - `Allow` → 照常 dispatch；
    /// - `Deny` / `Candidate` → 不 dispatch，回写错误 IoResponse
    ///   （反应器恢复执行，IoRequest 不会悬空）。
    ///
    /// # 判定上下文
    /// 以 IoRequest 的 `cause` 构造 `IoCallContext`（默认 caller_role=Unknown，
    /// 应用层可经 `PermissionGate::with_caller_role_resolver` 注入解析器）；
    /// `v_trigger` 由门内部冻结为 SharedFactsLog 当前版本（D8 语义）。
    ///
    /// # 与 skip 谓词的顺序
    /// skip 谓词**先**于权限门：skip 命中 = 请求交由外部执行者（审计桥 LLM）
    /// 处理，不属本订阅者分发器的权限域——若先门后跳，默认策略下 LLM 角色
    /// Deny 会抢答错误响应，外部执行者的应答将被反应器忽略（Unknown
    /// IoResponse），审计回路断裂。权限门只仲裁本订阅者自己分发的 I/O。
    pub fn with_permission_gate(mut self, gate: PermissionGate) -> Self {
        self.gate = Some(gate);
        self
    }

    /// 权限前置判定门是否已装配（可观测性）
    ///
    /// 守卫是**可选**装配的（`gate: Option<..>`）：未装配时 `run()` 会 warn，
    /// 且本订阅者自动分发的每条 IoRequest 都跳过权限仲裁（fail-open）。
    ///
    /// 「已装配」这件事此前在核心仓内不可查询、不可断言，只能检查调用方
    /// 的装配代码才能发现。本方法把它变成可在编译期断言的事实，供集成测试
    /// 与部署自检使用。
    ///
    /// 注意：本方法只回答「装配与否」，**不**回答「策略是否正确」——
    /// 装了一个策略为空的门同样是装配，是否达到预期仍需调用方自证。
    pub fn permission_gate_enabled(&self) -> bool {
        self.gate.is_some()
    }

    /// 注入会话级熔断状态（builder 模式；SG-3 family-②）
    ///
    /// 注入后，本订阅者对**静态路由层 deny**（`route` 命中，即白名单外锚点
    /// 拒绝）按滑动窗口计数：
    /// - 达阈值 → 跳闸：熔断信号指令 `set meta_security.circuit_breaker = ...`
    ///   经 command 通道提交会话（规则层 `00_constraint_enforce_anchor.json`
    ///   第 3 条 enforce 拒链 = 跳闸证据确定性入链）+ 跳闸持续期内后续 egress
    ///   全量拒绝（error 串带 `circuit=tripped` 段）；
    /// - 窗口过期自动复位重新计数。
    ///
    /// 未装配 = 熔断面关闭（行为与 SG-2 一致）。计数/跳闸语义见
    /// [`crate::circuit_breaker`] 模块文档。
    pub fn with_circuit_breaker(mut self, state: CircuitState) -> Self {
        self.circuit = Some(state);
        self
    }

    /// 注入跳过谓词（builder 模式）
    ///
    /// 谓词命中的 IoRequest **不自动应答**（不回写任何 IoResponse），
    /// 留给外部执行者（审计桥）处理。
    pub fn with_skip(mut self, predicate: SkipPredicate) -> Self {
        self.skip = Some(predicate);
        self
    }

    /// 注入指标收集器（builder 模式）
    ///
    /// 注入后，`dispatch_and_respond` 将通过 trait object 分发调用具体的指标记录方法。
    /// 应用层通常注入 `PrometheusMetrics` 实现。
    pub fn with_metrics(mut self, metrics: SharedMetrics) -> Self {
        self.metrics = metrics;
        self
    }

    /// 生成下一个 FactId 并推进计数器
    fn next_fact_id(&mut self) -> FactId {
        let id = FactId(self.next_id);
        self.next_id += 1;
        id
    }

    /// 启动订阅循环
    ///
    /// # 参数
    /// - `event_rx`：event broadcast 通道接收端（可通过 `event_tx.subscribe()` 创建）
    /// - `command_tx`：command 通道发送端（用于回写 `IoResponse`）
    ///
    /// # 返回
    /// - `Ok(())`：通道正常关闭，订阅循环结束
    /// - `Err(IoSubscriberError::CommandClosed)`：反应器已退出，无法回写 `IoResponse`
    ///
    /// # 行为
    /// - 接收 `Fact::IoRequest` → 调度执行 → 回写 `Fact::IoResponse`
    /// - 忽略其他 Fact 类型（仅记录 `trace` 日志）
    /// - `RecvError::Lagged` 记录 `warn` 并继续，不中断循环
    /// - `RecvError::Closed` 视为正常结束，返回 `Ok(())`
    pub async fn run(
        mut self,
        mut event_rx: EventReceiver,
        command_tx: FactSender,
    ) -> Result<(), IoSubscriberError> {
        tracing::info!(
            id_offset = ID_OFFSET,
            "IoSubscriber 启动，开始订阅 event broadcast 通道"
        );

        // 可观测性：守卫未装配 = 本订阅者的 I/O 全部跳过权限仲裁（fail-open）。
        // 此前该状态在核心仓内完全静默，只能检查调用方的装配代码才能发现。
        if !self.permission_gate_enabled() {
            tracing::warn!(
                "IoSubscriber 未装配 PermissionGate：本订阅者自动分发的 IoRequest \
                 将跳过权限仲裁（fail-open）。生产环境须经 with_permission_gate(..) 注入。"
            );
        }

        loop {
            match event_rx.recv().await {
                Ok(fact) => {
                    self.handle_fact(fact, &command_tx).await?;
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(
                        skipped = n,
                        "IoSubscriber 落后于 event 通道，已跳过 {} 条 Fact",
                        n
                    );
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                    tracing::info!("Event 通道已关闭，IoSubscriber 正常退出");
                    return Ok(());
                }
            }
        }
    }

    /// 处理单个 Fact：若是 `IoRequest` 则调度执行并回写 `IoResponse`，否则忽略。
    ///
    /// 发送失败（反应器已退出）时返回 `CommandClosed` 错误。
    async fn handle_fact(
        &mut self,
        fact: Fact,
        command_tx: &FactSender,
    ) -> Result<(), IoSubscriberError> {
        match fact {
            Fact::IoRequest {
                id,
                cause,
                io_type,
                params,
            } => {
                // 跳过谓词命中 → 不自动应答，留给外部执行者（审计桥）。
                // 必须在 dispatch（与权限门）之前判断：一旦回写（哪怕错误），外部执行者的
                // io_response 会被反应器忽略（Unknown IoResponse）。
                if let Some(skip) = &self.skip {
                    if skip(&io_type, &params) {
                        tracing::trace!(
                            fact_id = %id,
                            io_type = %io_type,
                            "IoSubscriber 命中跳过谓词，IoRequest 留待外部执行者应答"
                        );
                        return Ok(());
                    }
                }
                self.dispatch_and_respond(id, cause, io_type, params, command_tx)
                    .await
            }
            other => {
                tracing::trace!(
                    fact_id = %other.id(),
                    fact_type = other.type_name(),
                    "IoSubscriber 忽略非 IoRequest 事实"
                );
                Ok(())
            }
        }
    }

    /// 执行 I/O 调度并回写 `IoResponse`
    ///
    /// - 成功：`result = JsonValue`，`error = None`
    /// - 失败：`result = JsonValue::Null`，`error = Some(msg)`
    ///
    /// # P0-2 重试策略
    /// 对瞬时错误（超时/连接/5xx）执行指数退避重试，最多 `MAX_RETRIES` 次：
    /// 200ms → 400ms → 800ms。客户端错误（参数缺失/4xx/工具未找到）不重试。
    /// 重试耗尽后回写最终错误 `IoResponse`，让反应器恢复而非永久阻塞。
    // IO dispatch + 重试 + 错误回写多分支, 拆函数需共享 self/cmd 状态。详见 GATE_REFERENCE.md §六(豁免索引)
    #[allow(clippy::cognitive_complexity, clippy::too_many_lines)]
    async fn dispatch_and_respond(
        &mut self,
        request_id: FactId,
        cause: FactId,
        io_type: IoType,
        params: JsonValue,
        command_tx: &FactSender,
    ) -> Result<(), IoSubscriberError> {
        tracing::info!(
            request_id = %request_id,
            io_type = %io_type,
            "处理 IoRequest"
        );

        //：权限前置判定门（可选装配）。
        // Deny/Candidate → 不 dispatch，回写错误 IoResponse 让反应器恢复
        // （IoRequest 不会悬空）；Candidate 需审批人裁决，自动路径按拒绝处理
        // （fail-closed）。
        // SG-2：check_routed 额外给出静态路由语义，拼入 error 串（确定性构造，
        // 无时间戳/路径分隔符），使 deny / ask-fallback 与 default deny 可区分。
        // SG-3：熔断闸位先于权限门——跳闸持续期内 egress 全量拒绝（静态层
        // deny 连发熔断的持续语义；窗口过期自动复位）。
        if let Some(circuit) = &mut self.circuit {
            if circuit.is_tripped(Instant::now()) {
                self.metrics.inc_io_errors(io_type.as_str());
                tracing::warn!(
                    request_id = %request_id,
                    io_type = %io_type.as_str(),
                    "IoRequest 被会话熔断闸位拒绝（circuit=tripped）"
                );
                let response = Fact::IoResponse {
                    id: self.next_fact_id(),
                    request_id,
                    result: JsonValue::Null,
                    error: Some(format!(
                        "circuit breaker tripped (circuit=tripped): io_type={} {}",
                        io_type.as_str(),
                        "egress denied until window expiry"
                    )),
                };
                return command_tx.send(response).map_err(|_| {
                    IoSubscriberError::CommandClosed(format!("request_id={request_id}"))
                });
            }
        }
        if let Some(gate) = &self.gate {
            let mut ctx = IoCallContext::new(cause, 0, None);
            let (verdict, route) = gate.check_routed(&mut ctx, io_type.as_str(), Some(&params));
            if verdict != Verdict::Allow {
                let reason = match verdict {
                    Verdict::Deny => "denied by permission gate",
                    Verdict::Candidate => "pending permission approval (fail-closed)",
                    Verdict::Allow => unreachable!(),
                };
                let route_seg = match route {
                    Some(r) => format!(" (route={r})"),
                    None => String::new(),
                };
                tracing::warn!(
                    request_id = %request_id,
                    io_type = %io_type.as_str(),
                    caller_role = ?ctx.caller_role,
                    verdict = ?verdict,
                    route = ?route,
                    "IoRequest 权限判定未通过，回写错误 IoResponse"
                );
                // SG-3：静态层 deny 计数（family-② 白名单外锚点连发）——仅
                // route 命中的静态收紧拒绝计数（动态表 deny 属 PermissionGate
                // 级留痕观察，威胁建模 §十一 D-3 拍板）；达阈值 → 跳闸 + 熔断
                // 信号指令提交会话（规则层 enforce 拒链 = 跳闸证据确定性入链）。
                if route.is_some() {
                    if let Some(circuit) = &mut self.circuit {
                        if let Some(signal) = circuit.record_deny(Instant::now()) {
                            let instruction = JsonValue::object_from_pairs(&[
                                ("type", JsonValue::from("set")),
                                (
                                    "params",
                                    JsonValue::object_from_pairs(&[
                                        ("attr", JsonValue::from(CIRCUIT_BREAKER_ATTR)),
                                        ("operation", JsonValue::from("set")),
                                        ("value", signal),
                                    ]),
                                ),
                            ]);
                            // 信号提交失败不阻断本次 deny 回写（通道关闭时主
                            // 响应同样失败）；warn 留痕禁静默。
                            if let Err(err) = command_tx.send(Fact::Command {
                                id: self.next_fact_id(),
                                instruction,
                            }) {
                                tracing::warn!(
                                    request_id = %request_id,
                                    error = %err,
                                    "熔断信号指令提交失败（command 通道异常）"
                                );
                            } else {
                                tracing::warn!(
                                    request_id = %request_id,
                                    "熔断跳闸：静态层 deny 达阈值，circuit_breaker 信号已提交会话"
                                );
                            }
                        }
                    }
                }
                self.metrics.inc_io_errors(io_type.as_str());
                let response = Fact::IoResponse {
                    id: self.next_fact_id(),
                    request_id,
                    result: JsonValue::Null,
                    error: Some(format!(
                        "permission denied{}: io_type={} {}, resource={}{}",
                        route_seg,
                        io_type.as_str(),
                        reason,
                        "io:",
                        io_type.as_str()
                    )),
                };
                return command_tx.send(response).map_err(|_| {
                    IoSubscriberError::CommandClosed(format!("request_id={request_id}"))
                });
            }
        }

        // 记录整体 I/O 耗时（包含重试）。通过 trait object 分发，
        // 默认 NoOpMetrics 空转，应用层注入的 PrometheusMetrics 会实际记录。
        let overall_start = std::time::Instant::now();
        let io_type_str: &str = io_type.as_str();
        let mut had_error = false;

        let mut attempt: u32 = 0;
        let response = loop {
            attempt += 1;
            match self.dispatcher.dispatch(&io_type, &params).await {
                Ok(result) => {
                    if attempt > 1 {
                        tracing::info!(
                            request_id = %request_id,
                            attempt,
                            "IoRequest 在重试后执行成功"
                        );
                    } else {
                        tracing::info!(
                            request_id = %request_id,
                            "IoRequest 执行成功，回写 IoResponse"
                        );
                    }
                    break Fact::IoResponse {
                        id: self.next_fact_id(),
                        request_id,
                        result,
                        error: None,
                    };
                }
                Err(err_msg) => {
                    // P0-2：瞬时错误指数退避重试
                    if attempt <= MAX_RETRIES && is_retryable_error(&err_msg) {
                        let backoff =
                            INITIAL_BACKOFF.saturating_mul(2u32.saturating_pow(attempt - 1));
                        tracing::warn!(
                            request_id = %request_id,
                            attempt,
                            max_attempts = MAX_RETRIES + 1,
                            backoff_ms = backoff.as_millis() as u64,
                            error = %err_msg,
                            "IoRequest 瞬时错误，指数退避重试"
                        );
                        tokio::time::sleep(backoff).await;
                        continue;
                    }
                    tracing::warn!(
                        request_id = %request_id,
                        attempt,
                        error = %err_msg,
                        "IoRequest 执行失败（最终），回写错误 IoResponse"
                    );
                    had_error = true;
                    break Fact::IoResponse {
                        id: self.next_fact_id(),
                        request_id,
                        result: JsonValue::Null,
                        error: Some(err_msg),
                    };
                }
            }
        };

        // 通过 trait object 分发记录指标（NoOpMetrics 空转，PrometheusMetrics 实际记录）
        self.metrics
            .observe_io_duration(io_type_str, overall_start.elapsed());
        if had_error {
            self.metrics.inc_io_errors(io_type_str);
        }

        command_tx
            .send(response)
            .map_err(|_| IoSubscriberError::CommandClosed(format!("request_id={request_id}")))?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    #![allow(clippy::panic, clippy::expect_used)]
    use super::*;
    use evorule_reactor::IoType;

    #[test]
    fn test_id_starts_at_offset() {
        // 仅验证 ID 计数器逻辑，不依赖 dispatcher 构造
        // （dispatcher 的实际构造需要 DB / LLM 等外部资源，留给集成测试）
        let next_id = ID_OFFSET;
        assert_eq!(FactId(next_id), FactId(10000));
        assert_eq!(FactId(next_id + 1), FactId(10001));
        assert_eq!(FactId(next_id + 2), FactId(10002));
    }

    #[test]
    fn test_io_subscriber_error_display() {
        let e = IoSubscriberError::ChannelClosed;
        assert_eq!(format!("{e}"), "Event channel closed");

        let e = IoSubscriberError::CommandClosed("request_id=F42".to_string());
        assert_eq!(format!("{e}"), "Command channel closed: request_id=F42");
    }

    #[test]
    fn test_io_type_import_available() {
        // 确保 IoType 在本模块内可见且可使用（防止 import 被意外删除）
        let t = IoType::call_external();
        assert_eq!(t.as_str(), "call_external");
    }

    #[test]
    fn test_is_retryable_error_timeout() {
        assert!(is_retryable_error(
            "http request failed: operation timed out"
        ));
        assert!(is_retryable_error("db query timed out after 5s"));
        assert!(is_retryable_error("request timeout"));
    }

    #[test]
    fn test_is_retryable_error_connection() {
        assert!(is_retryable_error("connection refused"));
        assert!(is_retryable_error("connection reset by peer"));
        assert!(is_retryable_error("Connection closed"));
    }

    #[test]
    fn test_is_retryable_error_http_5xx() {
        assert!(is_retryable_error(
            "LLM API returned 503: service unavailable"
        ));
        assert!(is_retryable_error("http request failed with status: 500"));
        assert!(is_retryable_error("gateway 502 bad gateway"));
    }

    #[test]
    fn test_is_retryable_error_temporary() {
        assert!(is_retryable_error("service temporarily unavailable"));
        assert!(is_retryable_error("Temporary failure in name resolution"));
    }

    #[test]
    fn test_is_not_retryable_error_client_errors() {
        // 参数缺失——bug，重试无意义
        assert!(!is_retryable_error("missing required param: prompt"));
        // 工具未找到——配置问题
        assert!(!is_retryable_error("tool not found: foo"));
        // HTTP 4xx 客户端错误
        assert!(!is_retryable_error("LLM API returned 401: unauthorized"));
        assert!(!is_retryable_error("http request failed with status: 404"));
        assert!(!is_retryable_error("bad request 400"));
    }

    #[test]
    fn test_retry_constants() {
        // 验证 P0-2 重试配置
        assert_eq!(MAX_RETRIES, 3);
        assert_eq!(INITIAL_BACKOFF, Duration::from_millis(200));
        // 退避序列：200ms, 400ms, 800ms
        assert_eq!(
            INITIAL_BACKOFF.saturating_mul(2u32.saturating_pow(0)),
            Duration::from_millis(200)
        );
        assert_eq!(
            INITIAL_BACKOFF.saturating_mul(2u32.saturating_pow(1)),
            Duration::from_millis(400)
        );
        assert_eq!(
            INITIAL_BACKOFF.saturating_mul(2u32.saturating_pow(2)),
            Duration::from_millis(800)
        );
    }

    // ===== skip 谓词（审计桥 2026-08-30）：命中时不自动应答 =====

    /// 构造 LLM 审计形态的 call_external params（有 messages、无 service_name）
    fn llm_audit_params() -> JsonValue {
        JsonValue::object_from_pairs(&[(
            "messages",
            JsonValue::array(vec![JsonValue::object_from_pairs(&[
                ("role", JsonValue::string("user")),
                ("content", JsonValue::string("hi")),
            ])]),
        )])
    }

    /// skip 命中 → handle_fact 返回 Ok 且**不回写任何 IoResponse**（留给外部执行者）
    #[tokio::test]
    async fn test_skip_predicate_leaves_io_request_unanswered() {
        // 空 dispatcher：若未跳过而走 dispatch，必然回写错误 IoResponse
        let mut subscriber = IoSubscriber::new(IoDispatcher::builder().build()).with_skip(
            Arc::new(|io_type: &IoType, params: &JsonValue| {
                io_type.as_str() == "call_external"
                    && params.get("messages").is_some()
                    && params.get("service_name").is_none()
                    && params.get("name").is_none()
            }),
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fact = Fact::IoRequest {
            id: FactId(1),
            cause: FactId(0),
            io_type: IoType::call_external(),
            params: llm_audit_params(),
        };

        let result = subscriber.handle_fact(fact, &tx).await;
        assert!(result.is_ok());
        // 关键断言：command 通道无任何 IoResponse（外部执行者的 io_response 不会被抢答）
        assert!(rx.try_recv().is_err(), "skip 命中时不应回写任何 IoResponse");
    }

    /// 对照组：默认（无 skip）→ dispatch 失败 → 回写错误 IoResponse（历史行为不变）
    #[tokio::test]
    async fn test_without_skip_error_response_is_written() {
        let mut subscriber = IoSubscriber::new(IoDispatcher::builder().build());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fact = Fact::IoRequest {
            id: FactId(2),
            cause: FactId(0),
            io_type: IoType::call_external(),
            params: llm_audit_params(),
        };

        let result = subscriber.handle_fact(fact, &tx).await;
        assert!(result.is_ok());
        // 空 dispatcher → dispatch Err → 错误 IoResponse 回写
        match rx.try_recv() {
            Ok(Fact::IoResponse {
                request_id, error, ..
            }) => {
                assert_eq!(request_id, FactId(2));
                assert!(error.is_some(), "未注册类型应回写错误 IoResponse");
            }
            other => panic!("应回写错误 IoResponse，实际: {other:?}"),
        }
    }

    /// 谓词只命中 LLM 审计形态：带 service_name 的 call_external 不跳过（照常分发）
    #[tokio::test]
    async fn test_skip_predicate_does_not_hit_service_calls() {
        let mut subscriber = IoSubscriber::new(IoDispatcher::builder().build()).with_skip(
            Arc::new(|io_type: &IoType, params: &JsonValue| {
                io_type.as_str() == "call_external"
                    && params.get("messages").is_some()
                    && params.get("service_name").is_none()
                    && params.get("name").is_none()
            }),
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        // service 调用形态：service_name 存在、无 messages
        let params = JsonValue::object_from_pairs(&[(
            "service_name",
            JsonValue::string("inverse_kinematics_solver"),
        )]);
        let fact = Fact::IoRequest {
            id: FactId(3),
            cause: FactId(0),
            io_type: IoType::call_external(),
            params,
        };

        let result = subscriber.handle_fact(fact, &tx).await;
        assert!(result.is_ok());
        // 未跳过 → dispatch（空 dispatcher）→ 错误 IoResponse 回写
        assert!(
            rx.try_recv().is_ok(),
            "带 service_name 的调用不应被跳过，应照常分发并回写"
        );
    }

    // ===== 权限前置判定门（ / B6）=====

    /// 构造一个 service 调用形态的 IoRequest（供门测试用）
    fn service_call_request(id: u64) -> Fact {
        let params = JsonValue::object_from_pairs(&[(
            "service_name",
            JsonValue::string("inverse_kinematics_solver"),
        )]);
        Fact::IoRequest {
            id: FactId(id),
            cause: FactId(1),
            io_type: IoType::call_external(),
            params,
        }
    }

    /// 可观测性：`permission_gate_enabled()` 必须如实反映装配状态。
    ///
    /// 这是「守卫未装配」能被自动发现的前提 —— 此前该事实需检查调用方装配代码。
    /// 注：本测试断言的是**装配状态**；`run()` 中同状态输出的 warn 因缺少
    /// tracing 捕获依赖，未纳入断言（可观测性仍不完整，待后续补强）。
    #[test]
    fn test_permission_gate_enabled_reflects_assembly() {
        use crate::permission::PermissionGate;
        use std::sync::Arc as StdArc;

        // 未装配 → false
        let bare = IoSubscriber::new(IoDispatcher::builder().build());
        assert!(
            !bare.permission_gate_enabled(),
            "默认构造的 IoSubscriber 必须报告守卫未装配"
        );

        // 装配后 → true
        let gate = PermissionGate::new(StdArc::new(crate::shared_facts_log::SharedFactsLog::new()));
        let gated = IoSubscriber::new(IoDispatcher::builder().build()).with_permission_gate(gate);
        assert!(
            gated.permission_gate_enabled(),
            "注入 PermissionGate 后必须报告守卫已装配"
        );
    }

    /// 门 Deny（默认 Unknown 角色 + 空表 → fail-closed Deny）→ 回写
    /// "permission denied" 错误 IoResponse，不进入 dispatch
    #[tokio::test]
    async fn test_permission_gate_deny_writes_error_response() {
        use crate::permission::PermissionGate;
        use std::sync::Arc as StdArc;

        let gate = PermissionGate::new(StdArc::new(crate::shared_facts_log::SharedFactsLog::new()));
        let mut subscriber =
            IoSubscriber::new(IoDispatcher::builder().build()).with_permission_gate(gate);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        subscriber
            .handle_fact(service_call_request(10), &tx)
            .await
            .unwrap();

        match rx.try_recv() {
            Ok(Fact::IoResponse {
                request_id, error, ..
            }) => {
                assert_eq!(request_id, FactId(10));
                let err = error.expect("Deny 必须回写错误 IoResponse");
                assert!(
                    err.contains("permission denied"),
                    "错误消息应表明权限拒绝，实际: {err}"
                );
            }
            other => panic!("Deny 应回写错误 IoResponse，实际: {other:?}"),
        }
    }

    /// 门 Allow（resolver→Human，空表默认 human=Allow）→ 照常进入 dispatch
    /// （空 dispatcher 报"分发失败"类错误，而非权限拒绝）
    #[tokio::test]
    async fn test_permission_gate_allow_proceeds_to_dispatch() {
        use crate::permission::PermissionGate;
        use evorule_reactor::CallerRole;
        use std::sync::Arc as StdArc;

        let gate = PermissionGate::new(StdArc::new(crate::shared_facts_log::SharedFactsLog::new()))
            .with_caller_role_resolver(StdArc::new(|_| CallerRole::Human));
        let mut subscriber =
            IoSubscriber::new(IoDispatcher::builder().build()).with_permission_gate(gate);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        subscriber
            .handle_fact(service_call_request(11), &tx)
            .await
            .unwrap();

        match rx.try_recv() {
            Ok(Fact::IoResponse { error, .. }) => {
                let err = error.expect("空 dispatcher 必然分发失败");
                assert!(
                    !err.contains("permission denied"),
                    "Allow 后错误应来自 dispatch 而非权限门，实际: {err}"
                );
            }
            other => panic!("Allow 应照常 dispatch 并回写，实际: {other:?}"),
        }
    }

    /// SG-2 负向实测①（J-SG2）：llm 命中静态 deny → 错误 IoResponse 含 route=deny，
    /// 与 default deny（无 route 段）可区分；每决策一 Fact（错误 IoResponse 落链）
    #[tokio::test]
    async fn test_sg2_route_deny_negative_case() {
        use crate::permission::{PermissionGate, RoutePolicy};
        use evorule_reactor::CallerRole;
        use std::sync::Arc as StdArc;

        const POLICY: &str = r#"{
            "version": 1,
            "rules": [
                { "caller_role": "llm", "io_type": "http_get", "target": "api.internal:*", "decision": "deny" }
            ]
        }"#;
        let policy = RoutePolicy::from_json_str(POLICY).unwrap();
        let gate = PermissionGate::new(StdArc::new(crate::shared_facts_log::SharedFactsLog::new()))
            .with_caller_role_resolver(StdArc::new(|_| CallerRole::Llm))
            .with_route_policy(policy);
        let mut subscriber =
            IoSubscriber::new(IoDispatcher::builder().build()).with_permission_gate(gate);

        let params =
            JsonValue::object_from_pairs(&[("target", JsonValue::string("api.internal:8080/x"))]);
        let fact = Fact::IoRequest {
            id: FactId(20),
            cause: FactId(1),
            io_type: IoType::http_get(),
            params,
        };

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        subscriber.handle_fact(fact, &tx).await.unwrap();

        match rx.try_recv() {
            Ok(Fact::IoResponse {
                request_id, error, ..
            }) => {
                assert_eq!(request_id, FactId(20));
                let err = error.expect("静态 deny 必须回写错误 IoResponse");
                assert!(
                    err.contains("permission denied"),
                    "权限拒绝语义保留，实际: {err}"
                );
                assert!(
                    err.contains("route=deny"),
                    "J-SG2：静态 deny 必须携带 route 语义与 default deny 可区分，实际: {err}"
                );
            }
            other => panic!("静态 deny 应回写错误 IoResponse，实际: {other:?}"),
        }
    }

    /// SG-2 负向实测②（J-SG2）：llm 命中 ask（无人值守）→ 按资产 ask_fallback
    /// 回退 deny，error 含 route=ask-fallback（SG-5 挂账的 ask 语义收口）
    #[tokio::test]
    async fn test_sg2_ask_fallback_negative_case() {
        use crate::permission::{PermissionGate, RoutePolicy};
        use evorule_reactor::CallerRole;
        use std::sync::Arc as StdArc;

        const POLICY: &str = r#"{
            "version": 1,
            "ask_fallback": { "unattended": "deny" },
            "rules": [
                { "caller_role": "*", "io_type": "call_external", "target": "*", "decision": "ask" }
            ]
        }"#;
        let policy = RoutePolicy::from_json_str(POLICY).unwrap();
        let gate = PermissionGate::new(StdArc::new(crate::shared_facts_log::SharedFactsLog::new()))
            .with_caller_role_resolver(StdArc::new(|_| CallerRole::Llm))
            .with_route_policy(policy);
        let mut subscriber =
            IoSubscriber::new(IoDispatcher::builder().build()).with_permission_gate(gate);

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        subscriber
            .handle_fact(service_call_request(21), &tx)
            .await
            .unwrap();

        match rx.try_recv() {
            Ok(Fact::IoResponse {
                request_id, error, ..
            }) => {
                assert_eq!(request_id, FactId(21));
                let err = error.expect("ask 无人值守回退必须回写错误 IoResponse");
                assert!(
                    err.contains("route=ask-fallback"),
                    "J-SG2：ask 回退必须携带 ask-fallback 语义，实际: {err}"
                );
            }
            other => panic!("ask 回退应回写错误 IoResponse，实际: {other:?}"),
        }
    }

    /// skip 命中优先于权限门：外部执行者（审计桥 LLM）请求不被门抢答
    #[tokio::test]
    async fn test_permission_gate_does_not_override_skip() {
        use crate::permission::PermissionGate;
        use std::sync::Arc as StdArc;

        let gate = PermissionGate::new(StdArc::new(crate::shared_facts_log::SharedFactsLog::new()));
        let mut subscriber = IoSubscriber::new(IoDispatcher::builder().build())
            .with_permission_gate(gate)
            .with_skip(StdArc::new(|io_type: &IoType, params: &JsonValue| {
                io_type.as_str() == "call_external"
                    && params.get("messages").is_some()
                    && params.get("service_name").is_none()
            }));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let fact = Fact::IoRequest {
            id: FactId(12),
            cause: FactId(1),
            io_type: IoType::call_external(),
            params: llm_audit_params(),
        };
        subscriber.handle_fact(fact, &tx).await.unwrap();

        assert!(
            rx.try_recv().is_err(),
            "skip 命中时权限门不得抢答（审计回路依赖外部执行者应答）"
        );
    }
}
