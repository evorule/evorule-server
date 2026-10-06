// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 输出门禁（io_guard）——最终应答绕工具行为封堵
//!
//! 设计目标：不能让 LLM 有绕开工具实施的机会——最终应答（`is_finished=true`）
//! 文本含可执行动作特征 × 该能力有工具在装配面 × 本会话该工具零调用 → 命中。
//! 三条件判据全部取自既有链上事实（收尾文本 / 装配面 manifest / 本轮消息序列），
//! 零新增判据源；零语义判官，只字面配对（壳核分离：语义归 LLM，行为归闸）。
//!
//! - 特征表：治理正本 `{rules_dir}/00_constraint_io_guard.json`（kind=
//!   `io_guard_features`）。`transform: []` 空数组使其对规则装载器零规则静默
//!   共存（tier 门禁按 00_constraint_ 前缀 + `metadata.tier=constraint` 通过），
//!   特征由本模块按路径直读。
//! - 模式开关：env `EVORULE_IO_GUARD` = `observe` | `enforce` | `off`，缺省
//!   observe（命中只落 Violation 账不阻断；观察期后转 enforce）。非法值按
//!   observe 处理 + warn（fail-open，与 discipline_gate 同纪律）。
//! - 挂点：`session_io_response` handler，仅最终应答分支触发；工具结果回喂
//!   路径（无 `is_finished` 标记）与错误应答（`error` 非空）不触。
//! - 时序注记：检测不依赖 facts_log 的 tool_trace——收尾时序 io_response 先于
//!   submit_tool_traces，挂点时刻 tool_trace 恒空；已调用工具的地面真值取自
//!   `result.messages`（本轮 ReAct 消息序列），tool_trace 仅作跨 run 残留兜底。
//! - Violation 落链：`rule_index = u64::MAX` 为保留值，标记特征表命中（区别于
//!   规则层 enforce 的规则下标）；`cause` 指向被拒的 IoResponse 事实。

use evorule_reactor::{Fact, FactsLog};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::sync::OnceLock;

/// 特征表正本文件名（置于 rules_dir 根目录，与 00_constraint_ 元规则同目录）
pub const FEATURE_TABLE_FILE: &str = "00_constraint_io_guard.json";

/// 门禁模式（env `EVORULE_IO_GUARD`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoGuardMode {
    /// 观察：命中只落 Violation 账，不阻断（缺省）
    Observe,
    /// 强制：命中落账并以 422 拒绝收尾
    Enforce,
    /// 关闭：零开销直通
    Off,
}

impl IoGuardMode {
    /// env 解析；缺省 observe。非法值按 observe + warn（fail-open，
    /// 门禁配置错误不放大为可用性事故）。
    pub fn from_env() -> Self {
        match std::env::var("EVORULE_IO_GUARD") {
            Ok(v) if v.eq_ignore_ascii_case("observe") => Self::Observe,
            Ok(v) if v.eq_ignore_ascii_case("enforce") => Self::Enforce,
            Ok(v) if v.eq_ignore_ascii_case("off") => Self::Off,
            Ok(other) => {
                tracing::warn!(
                    target: "io_guard",
                    value = %other,
                    "EVORULE_IO_GUARD 非法值，按 observe（纯观测）处理"
                );
                Self::Observe
            }
            Err(_) => Self::Observe,
        }
    }
}

/// 当前门禁模式（进程级一次解析）
pub fn guard_mode() -> IoGuardMode {
    static MODE: OnceLock<IoGuardMode> = OnceLock::new();
    *MODE.get_or_init(IoGuardMode::from_env)
}

/// 单个能力域的特征条目：域 × 域内工具名 × 动作特征词形
#[derive(Debug, Clone)]
pub struct FeatureDomain {
    /// 能力域标识（如 shell_exec / file_write / git_commit）
    pub domain: String,
    /// 该域工具名集合（与装配面 / 调用记录做交集判定）
    pub tools: Vec<String>,
    /// 动作特征词形（小写化子串匹配；如"已执行命令"）
    pub phrases: Vec<String>,
}

/// 特征表（解析自治理正本 JSON）
#[derive(Debug, Clone, Default)]
pub struct FeatureTable {
    pub features: Vec<FeatureDomain>,
}

/// 单次命中：命中的域与短语
#[derive(Debug, Clone)]
pub struct GuardHit {
    pub domain: String,
    pub phrase: String,
}

impl FeatureTable {
    /// 三条件判据：特征短语命中 × 装配面交集非空 × 该域零调用。
    ///
    /// 任一域同时满足三条件即命中（返回首个命中）；零语义判官，只字面配对。
    pub fn inspect(
        &self,
        content: &str,
        assembled: &BTreeSet<String>,
        called: &BTreeSet<String>,
    ) -> Option<GuardHit> {
        let lower = content.to_lowercase();
        for f in &self.features {
            let Some(phrase) = f.phrases.iter().find(|p| lower.contains(&p.to_lowercase())) else {
                continue;
            };
            let in_assembly = f.tools.iter().any(|t| assembled.contains(t));
            let any_called = f.tools.iter().any(|t| called.contains(t));
            if in_assembly && !any_called {
                return Some(GuardHit {
                    domain: f.domain.clone(),
                    phrase: phrase.clone(),
                });
            }
        }
        None
    }
}

/// 从治理正本 JSON 解析特征表；缺 `features` 键返回 None（调用方按未部署处理）。
/// 单条域条目残缺（缺 domain / tools / phrases）时跳过该条（fail-soft）。
pub fn parse_features(json: &Value) -> Option<FeatureTable> {
    let features_json = json.get("features")?.as_array()?;
    let mut features = Vec::new();
    for f in features_json {
        let Some(domain) = f.get("domain").and_then(|v| v.as_str()) else {
            continue;
        };
        let tools: Vec<String> = f
            .get("tools")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|t| t.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        let phrases: Vec<String> = f
            .get("phrases")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|p| p.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if tools.is_empty() || phrases.is_empty() {
            continue;
        }
        features.push(FeatureDomain {
            domain: domain.to_string(),
            tools,
            phrases,
        });
    }
    Some(FeatureTable { features })
}

/// 加载特征表（rules_dir 根目录正本）。未部署 / 解析失败均返回 None（io_guard
/// 不生效——fail-open，warn 每进程一次防日志刷屏）。
fn load_feature_table(rules_dir: &Path) -> Option<FeatureTable> {
    static MISSING_WARNED: OnceLock<()> = OnceLock::new();
    static PARSE_WARNED: OnceLock<()> = OnceLock::new();
    let path = rules_dir.join(FEATURE_TABLE_FILE);
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(_) => {
            if MISSING_WARNED.set(()).is_ok() {
                tracing::warn!(
                    target: "io_guard",
                    path = %path.display(),
                    "输出门禁特征表未部署，io_guard 不生效"
                );
            }
            return None;
        }
    };
    let json: Value = match serde_json::from_str(&raw) {
        Ok(j) => j,
        Err(e) => {
            if PARSE_WARNED.set(()).is_ok() {
                tracing::warn!(
                    target: "io_guard",
                    path = %path.display(),
                    error = %e,
                    "输出门禁特征表解析失败，io_guard 不生效"
                );
            }
            return None;
        }
    };
    parse_features(&json)
}

/// 从任意 JSON 结构递归收集已调用工具名（serde_json 版）。
///
/// 识别两种形态：`{"tool_name": "..."}`（translator Message::Tool / ToolCall
/// 序列化）与 `{"function": {"name": "..."}}`（OpenAI tool_calls 形态）。
/// 只匹配 JSON 结构键，不扫描字符串内容（用户消息文本不受影响）。
pub fn collect_tool_names(v: &Value, out: &mut BTreeSet<String>) {
    match v {
        Value::Object(map) => {
            if let Some(Value::String(t)) = map.get("tool_name") {
                out.insert(t.clone());
            }
            if let Some(Value::Object(f)) = map.get("function") {
                if let Some(Value::String(n)) = f.get("name") {
                    out.insert(n.to_string());
                }
            }
            for val in map.values() {
                collect_tool_names(val, out);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                collect_tool_names(item, out);
            }
        }
        _ => {}
    }
}

/// TCB JsonValue 版递归收集（供 facts_log 指令遍历复用）
fn collect_tool_names_tcb(v: &evorule_tcb::JsonValue, out: &mut BTreeSet<String>) {
    if let Some(obj) = v.as_object() {
        if let Some(t) = obj.get("tool_name").and_then(|x| x.as_str()) {
            out.insert(t.to_string());
        }
        if let Some(n) = obj
            .get("function")
            .and_then(|f| f.get("name"))
            .and_then(|x| x.as_str())
        {
            out.insert(n.to_string());
        }
        for (_, val) in obj.iter() {
            collect_tool_names_tcb(val, out);
        }
    } else if let Some(arr) = v.as_array() {
        for item in arr {
            collect_tool_names_tcb(item, out);
        }
    }
}

/// 从会话事实链提取（装配面工具集，已调用工具兜底集）。
///
/// - 装配面：倒扫语义下最新一条 `Command{type=call_external}` 的
///   `params.tools[].function.name`（evo-agent 每次 LLM 调用以该指令经引擎，
///   即会话 manifest 快照）。实现为正序遍历覆盖，最终保留最新一条。
/// - 兜底集：全部 `Command{type=tool_trace}` 的 `params.value.tool_name`
///   （收尾时序上 io_response 先于 tool_trace 批量提交，挂点时刻通常为空；
///   非空值来自同会话跨 run 残留——兜底语义宁漏判不误伤）。
///
/// 使用零 clone 的 `for_each_fact_from`（长会话下全量 clone 构成 O(n²) 瓶颈）。
pub fn extract_assembled_and_traced(facts_log: &FactsLog) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut assembled: BTreeSet<String> = BTreeSet::new();
    let mut traced: BTreeSet<String> = BTreeSet::new();
    facts_log.for_each_fact_from(0, |_, fact| {
        if let Fact::Command { instruction, .. } = fact {
            match instruction.get("type").and_then(|t| t.as_str()) {
                Some("call_external") => {
                    let mut tools = BTreeSet::new();
                    if let Some(arr) = instruction
                        .get("params")
                        .and_then(|p| p.get("tools"))
                        .and_then(|t| t.as_array())
                    {
                        for t in arr {
                            if let Some(name) = t
                                .get("function")
                                .and_then(|f| f.get("name"))
                                .and_then(|n| n.as_str())
                            {
                                tools.insert(name.to_string());
                            }
                        }
                    }
                    assembled = tools;
                }
                Some("tool_trace") => {
                    if let Some(value) = instruction.get("params").and_then(|p| p.get("value")) {
                        collect_tool_names_tcb(value, &mut traced);
                    }
                }
                _ => {}
            }
        }
    });
    (assembled, traced)
}

/// 最终应答绕工具检测（session_io_response 挂点判定入口）。
///
/// 返回 Some(GuardHit) = 三条件判据命中（调用方按模式落账 / 拒绝）；
/// None = 不触发（错误应答 / 非最终应答 / 空收尾文本 / 特征表未部署 / 未命中）。
pub fn detect_final_response_violation(
    rules_dir: &Path,
    facts_log: &FactsLog,
    result: &Value,
    error: Option<&str>,
) -> Option<GuardHit> {
    // 错误应答不触门禁（含门禁超限后的 error 收敛应答——防止自锁循环）
    if error.is_some() {
        return None;
    }
    // 仅最终应答分支触发；工具结果回喂路径（无 is_finished 标记）不触
    if result.get("is_finished").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    let content = result.get("content").and_then(|v| v.as_str())?;
    if content.is_empty() {
        return None;
    }
    let table = load_feature_table(rules_dir)?;
    let (assembled, traced) = extract_assembled_and_traced(facts_log);
    let mut called = traced;
    if let Some(messages) = result.get("messages") {
        collect_tool_names(messages, &mut called);
    }
    table.inspect(content, &assembled, &called)
}

#[cfg(test)]
mod tests {
    use super::*;
    use evorule_reactor::FactId;

    /// 测试用特征表 fixture（经 parse_features 走真实解析路径）
    fn feature_table_fixture() -> FeatureTable {
        let json: Value = serde_json::json!({
            "kind": "io_guard_features",
            "features": [
                {"domain": "shell_exec", "tools": ["shell_exec"],
                 "phrases": ["已执行", "已运行", "执行结果如下", "运行结果如下", "命令输出如下"]},
                {"domain": "file_write", "tools": ["file_write"],
                 "phrases": ["已写入文件", "已创建文件", "写入完成", "文件内容如下"]},
                {"domain": "git_commit", "tools": ["git_commit"],
                 "phrases": ["已提交 commit", "已提交到仓库", "代码已提交"]},
                {"domain": "http_get", "tools": ["http_get"],
                 "phrases": ["已访问", "已请求", "接口返回如下", "已抓取"]},
                {"domain": "retrieve", "tools": ["retrieve", "search_memory"],
                 "phrases": ["已检索", "已查询", "查询结果如下", "搜索结果如下"]}
            ]
        });
        parse_features(&json).unwrap_or_default()
    }

    fn full_assembly() -> BTreeSet<String> {
        [
            "shell_exec",
            "file_write",
            "git_commit",
            "http_get",
            "retrieve",
            "search_memory",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect()
    }

    fn set(items: &[&str]) -> BTreeSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn tcb_call_external(tools: &[&str]) -> Fact {
        let names: Vec<evorule_tcb::JsonValue> = tools
            .iter()
            .map(|t| {
                evorule_tcb::JsonValue::object_from_pairs(&[(
                    "function",
                    evorule_tcb::JsonValue::object_from_pairs(&[(
                        "name",
                        evorule_tcb::JsonValue::string(*t),
                    )]),
                )])
            })
            .collect();
        Fact::Command {
            id: FactId(1),
            instruction: evorule_tcb::JsonValue::object_from_pairs(&[
                ("type", evorule_tcb::JsonValue::string("call_external")),
                (
                    "params",
                    evorule_tcb::JsonValue::object_from_pairs(&[
                        ("model", evorule_tcb::JsonValue::string("test-model")),
                        ("tools", evorule_tcb::JsonValue::array(names)),
                    ]),
                ),
            ]),
        }
    }

    fn tcb_tool_trace(tool: &str) -> Fact {
        Fact::Command {
            id: FactId(2),
            instruction: evorule_tcb::JsonValue::object_from_pairs(&[
                ("type", evorule_tcb::JsonValue::string("tool_trace")),
                (
                    "params",
                    evorule_tcb::JsonValue::object_from_pairs(&[
                        (
                            "attr",
                            evorule_tcb::JsonValue::string("meta_tool.tool_traces.0"),
                        ),
                        (
                            "value",
                            evorule_tcb::JsonValue::object_from_pairs(&[
                                ("tool_name", evorule_tcb::JsonValue::string(tool)),
                                ("status", evorule_tcb::JsonValue::string("ok")),
                            ]),
                        ),
                    ]),
                ),
            ]),
        }
    }

    #[test]
    fn test_parse_features_valid() {
        let table = feature_table_fixture();
        assert_eq!(table.features.len(), 5);
        assert_eq!(table.features[0].domain, "shell_exec");
        assert!(table.features[0].tools.contains(&"shell_exec".to_string()));
    }

    #[test]
    fn test_parse_features_missing_key() {
        let json: Value = serde_json::json!({"kind": "io_guard_features"});
        assert!(parse_features(&json).is_none());
    }

    #[test]
    fn test_parse_features_skip_incomplete_entry() {
        let json: Value = serde_json::json!({
            "features": [
                {"domain": "broken"},
                {"domain": "ok", "tools": ["t"], "phrases": ["p"]}
            ]
        });
        let table = parse_features(&json).unwrap_or_default();
        assert_eq!(table.features.len(), 1);
        assert_eq!(table.features[0].domain, "ok");
    }

    #[test]
    fn test_inspect_bypass_sample_hit() {
        let table = feature_table_fixture();
        let assembled = full_assembly();
        let called = BTreeSet::new();
        // 绕行样本：声称已执行命令 + 命令回显形态，装配面有 shell_exec、零调用
        let hit = table.inspect(
            "已执行命令 dir，输出如下：file1.txt file2.txt",
            &assembled,
            &called,
        );
        assert!(
            matches!(&hit, Some(h) if h.domain == "shell_exec"),
            "bypass sample should hit shell_exec"
        );
        let hit = table.inspect(
            "已运行 npm test，运行结果如下：3 passed",
            &assembled,
            &called,
        );
        assert!(
            matches!(&hit, Some(h) if h.domain == "shell_exec"),
            "bypass sample should hit shell_exec"
        );
        let hit = table.inspect(
            "已写入文件 /data/out.txt，文件内容如下",
            &assembled,
            &called,
        );
        assert!(
            matches!(&hit, Some(h) if h.domain == "file_write"),
            "bypass sample should hit file_write"
        );
        let hit = table.inspect("代码已提交 commit abc123", &assembled, &called);
        assert!(
            matches!(&hit, Some(h) if h.domain == "git_commit"),
            "bypass sample should hit git_commit"
        );
    }

    #[test]
    fn test_inspect_no_hit_when_tool_called() {
        let table = feature_table_fixture();
        let assembled = full_assembly();
        // 工具已调用 → 不命中（判据三：零调用）
        let called = set(&["shell_exec"]);
        assert!(table
            .inspect("已执行命令 dir，输出如下：file1.txt", &assembled, &called)
            .is_none());
    }

    #[test]
    fn test_inspect_no_hit_when_tool_not_in_assembly() {
        let table = feature_table_fixture();
        // 装配面无该域工具 → 不命中（判据二）
        let assembled = set(&["file_read", "memory_search"]);
        let called = BTreeSet::new();
        assert!(table
            .inspect("已执行命令 dir，输出如下：file1.txt", &assembled, &called)
            .is_none());
    }

    #[test]
    fn test_inspect_case_insensitive_english_phrase() {
        let json: Value = serde_json::json!({
            "features": [
                {"domain": "deploy", "tools": ["deploy_svc"],
                 "phrases": ["Deployed To Cluster"]}
            ]
        });
        let table = parse_features(&json).unwrap_or_default();
        let assembled = set(&["deploy_svc"]);
        let called = BTreeSet::new();
        assert!(table
            .inspect(
                "successfully deployed to cluster prod-1",
                &assembled,
                &called
            )
            .is_some());
    }

    #[test]
    fn golden_narrative_zero_false_positive() {
        let table = feature_table_fixture();
        let assembled = full_assembly();
        let called = BTreeSet::new();
        let narratives = [
            "今天天气不错。",
            "这个问题的答案取决于具体场景。",
            "根据规则引擎的设计，事实是原子通信单元。",
            "我将执行以下步骤：先分析，再验证。",
            "计划运行测试套件以验证修复。",
            "建议查询数据库获取更多信息。",
            "The answer is 42.",
            "This task has three subtasks.",
            "如需更多信息，请提供上下文。",
            "会话已结束。",
            "分析完成，结论如下。",
            "抱歉，我无法理解该请求。",
            "版本号已更新为 1.2.3。",
            "配置文件位于 D:/config.toml。",
            "该方案包含两个阶段。",
            "验收判据已列出。",
            "文档已归档。",
            "任务已排队等待处理。",
            "The repository contains 42 files.",
            "测试全部通过。",
            "请确认是否继续。",
            "数据集包含三个字段。",
        ];
        for (i, n) in narratives.iter().enumerate() {
            assert!(
                table.inspect(n, &assembled, &called).is_none(),
                "golden #{} 误伤: {}",
                i,
                n
            );
        }
    }

    #[test]
    fn test_collect_tool_names_both_shapes() {
        let messages: Value = serde_json::json!([
            {"role": "user", "content": "请处理 tool_name 字段说明"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"tool_name": "shell_exec", "args": {"command": "dir"}}
            ]},
            {"role": "tool", "content": "out", "tool_name": "shell_exec"},
            {"role": "assistant", "content": "", "tool_calls": [
                {"function": {"name": "http_get", "arguments": {}}}
            ]}
        ]);
        let mut called = BTreeSet::new();
        collect_tool_names(&messages, &mut called);
        assert!(called.contains("shell_exec"));
        assert!(called.contains("http_get"));
        assert_eq!(called.len(), 2, "字符串内容中的 tool_name 不应计入");
    }

    #[test]
    fn test_extract_assembled_and_traced() {
        let log = FactsLog::new();
        assert!(log
            .append(tcb_call_external(&["shell_exec", "http_get"]))
            .is_ok());
        assert!(log.append(tcb_tool_trace("shell_exec")).is_ok());
        let (assembled, traced) = extract_assembled_and_traced(&log);
        assert_eq!(assembled, set(&["shell_exec", "http_get"]));
        assert_eq!(traced, set(&["shell_exec"]));
    }

    #[test]
    fn test_extract_assembled_keeps_latest_snapshot() {
        let log = FactsLog::new();
        assert!(log.append(tcb_call_external(&["shell_exec"])).is_ok());
        assert!(log
            .append(tcb_call_external(&["file_write", "git_commit"]))
            .is_ok());
        let (assembled, _) = extract_assembled_and_traced(&log);
        assert_eq!(assembled, set(&["file_write", "git_commit"]));
    }

    #[test]
    fn test_detect_final_response_paths() {
        let log = FactsLog::new();
        assert!(log.append(tcb_call_external(&["shell_exec"])).is_ok());
        let dir = std::env::temp_dir().join("io_guard_test_rules");
        let _ = std::fs::create_dir_all(&dir);
        assert!(
            std::fs::write(
                dir.join(FEATURE_TABLE_FILE),
                serde_json::json!({
                    "kind": "io_guard_features",
                    "features": [
                        {"domain": "shell_exec", "tools": ["shell_exec"], "phrases": ["已执行"]}
                    ]
                })
                .to_string(),
            )
            .is_ok(),
            "write fixture failed"
        );

        // 绕行样本：最终应答 + 特征 + 装配面 + 零调用 → 命中
        let bypass = serde_json::json!({
            "content": "已执行命令 dir，输出如下：a.txt",
            "is_finished": true,
            "messages": [{"role": "user", "content": "列出文件"}]
        });
        let hit = detect_final_response_violation(&dir, &log, &bypass, None);
        assert!(hit.is_some(), "bypass final response should hit");

        // 工具结果回喂路径（无 is_finished）→ 不触
        let tool_feed = serde_json::json!({"content": "已执行", "output": "a.txt"});
        assert!(detect_final_response_violation(&dir, &log, &tool_feed, None).is_none());

        // 错误应答 → 不触
        let mut err_req = bypass.clone();
        assert!(detect_final_response_violation(&dir, &log, &err_req, Some("boom")).is_none());

        // 工具已调用（messages 含 tool 消息）→ 不命中
        err_req["messages"] = serde_json::json!([
            {"role": "tool", "content": "out", "tool_name": "shell_exec"}
        ]);
        assert!(detect_final_response_violation(&dir, &log, &err_req, None).is_none());

        // 叙述性应答 → 不命中
        let narrative = serde_json::json!({
            "content": "分析完成，结论如下：该问题需要更多信息。",
            "is_finished": true,
            "messages": [{"role": "user", "content": "?"}]
        });
        assert!(detect_final_response_violation(&dir, &log, &narrative, None).is_none());
    }
}
