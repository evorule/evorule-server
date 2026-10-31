// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 原生服务实现：逻辑验证三服务（JSON 入 / JSON 出，统一信封 v1）。
//!
//! # 确定性
//! 纯整数/布尔/逻辑域，无浮点、无时钟、无随机；遍历序由编号与 BTree
//! 结构决定。同输入两次执行结果逐位一致。
//!
//! # 信封形态
//! 全部服务无舍入 → 契约**不含 scale/rounding**（按适用条件诚实省略,
//! checksum 家族先例）；成功出参含保留键 `status`(恒 determined) +
//! `determinism`(恒 bitwise_replayable)；error 走框架 `Err(String)`
//! 通道并带自诊断指引。

#![forbid(unsafe_code)]

use std::sync::Arc;

use evorule_plugin_kit::NativeService;
use evorule_reactor::IoResult;
use evorule_tcb::JsonValue;

use crate::fsm;
use crate::sat;

fn envelope(pairs: &[(&str, JsonValue)]) -> JsonValue {
    let mut all: Vec<(&str, JsonValue)> = vec![
        ("status", JsonValue::string("determined")),
        ("determinism", JsonValue::string("bitwise_replayable")),
    ];
    all.extend_from_slice(pairs);
    JsonValue::object_from_pairs(&all)
}

fn ints_array(args: &JsonValue, key: &str) -> Result<Vec<i64>, String> {
    let arr = args
        .get(key)
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("缺少参数 {key}（整数数组）— 自诊断指引: 见服务 parameters 契约"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (i, v) in arr.iter().enumerate() {
        let n = v
            .as_i64()
            .ok_or_else(|| format!("{key}[{i}] 须为整数"))?;
        out.push(n);
    }
    Ok(out)
}

fn usizes(json: &[i64], what: &str) -> Result<Vec<usize>, String> {
    json.iter()
        .enumerate()
        .map(|(i, &n)| {
            usize::try_from(n).map_err(|_| format!("{what}[{i}]={n} 须为非负整数"))
        })
        .collect()
}

// ============================================================================
// verification_sat_solve
// ============================================================================

struct SatSolveService;

impl NativeService for SatSolveService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let f = sat::parse_formula(args)?;
        match sat::dpll(&f)? {
            Some(a) => {
                if !sat::satisfies(&f, &a) {
                    return Err("内部自检失败:指派不满足公式(机械复核红线,不得返回)".into());
                }
                let mut clauses_true = 0usize;
                'outer: for clause in &f.clauses {
                    for lit in clause {
                        if a.get(&lit.var()) == Some(&lit.is_pos()) {
                            clauses_true += 1;
                            continue 'outer;
                        }
                    }
                }
                Ok(envelope(&[
                    ("satisfiable", JsonValue::Bool(true)),
                    ("assignment", JsonValue::array(sat::assignment_json(&f, &a))),
                    ("clauses_satisfied", JsonValue::Integer(clauses_true as i64)),
                    (
                        "note",
                        JsonValue::string(
                            "assignment[0..n] 按变量 1..n 升序;UNSAT 判定在本服务上限内为完备,超上限如实拒解",
                        ),
                    ),
                ]))
            }
            None => Ok(envelope(&[
                ("satisfiable", JsonValue::Bool(false)),
                (
                    "note",
                    JsonValue::string(
                        "UNSAT 为完备判定(DPLL 穷尽搜索空间);非超限拒解形态下不可反悔",
                    ),
                ),
            ])),
        }
    }
}

pub fn mk_sat_solve() -> Arc<dyn NativeService> {
    Arc::new(SatSolveService)
}

// ============================================================================
// verification_fsm_reachability
// ============================================================================

struct FsmReachabilityService;

impl NativeService for FsmReachabilityService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        let m = fsm::parse_fsm(args, "")?;
        let initials = usizes(&ints_array(args, "initial")?, "initial")?;
        if initials.is_empty() {
            return Err("initial 为空——至少须给一个初始状态".into());
        }
        for (i, &q) in initials.iter().enumerate() {
            if q >= m.state_count {
                return Err(format!("initial[{i}]={q} 超出状态域 [0,{})", m.state_count));
            }
        }
        let targets = usizes(&ints_array(args, "targets")?, "targets")?;
        if targets.is_empty() {
            return Err("targets 为空——至少须给一个目标状态".into());
        }
        for (i, &q) in targets.iter().enumerate() {
            if q >= m.state_count {
                return Err(format!("targets[{i}]={q} 超出状态域 [0,{})", m.state_count));
            }
        }
        let bounded = args.get("max_steps").is_some();
        let max_steps = match args.get("max_steps") {
            Some(JsonValue::Integer(n)) if *n >= 0 => Some(*n as u64),
            Some(JsonValue::Integer(n)) => {
                return Err(format!("max_steps={n} 须为非负整数"))
            }
            Some(_) => return Err("max_steps 须为整数".into()),
            None => None,
        };

        let (dist, pred) = fsm::bfs_reach(&m, &initials, max_steps);
        let mut reached: Vec<(usize, u64)> = targets
            .iter()
            .filter_map(|&t| dist[t].map(|d| (t, d)))
            .collect();
        reached.sort_by_key(|&(t, d)| (d, t)); // 按步数升序,步数同按状态号

        let witness = match reached.first() {
            Some(&(t, d)) => {
                let (start, word) = fsm::witness_path(&pred, t);
                let word_json: Vec<JsonValue> =
                    word.iter().map(|&s| JsonValue::Integer(s as i64)).collect();
                let path_json: Vec<JsonValue> = {
                    let mut path = vec![JsonValue::Integer(start as i64)];
                    let mut cur = start;
                    for &s in &word {
                        cur = m.transitions[cur][s];
                        path.push(JsonValue::Integer(cur as i64));
                    }
                    path
                };
                Some(JsonValue::object_from_pairs(&[
                    ("target", JsonValue::Integer(t as i64)),
                    ("steps", JsonValue::Integer(d as i64)),
                    ("start", JsonValue::Integer(start as i64)),
                    ("word", JsonValue::array(word_json)),
                    ("path", JsonValue::array(path_json)),
                ]))
            }
            None => None,
        };

        // 语言空性:是否存在某初始态出发可达某接受态(不受 max_steps 限,全空间)
        let (full_dist, _full_pred) = fsm::bfs_reach(&m, &initials, None);
        let mut accepts_any = false;
        let mut min_accept_steps: Option<u64> = None;
        for &q in &m.accepting {
            if let Some(d) = full_dist[q] {
                accepts_any = true;
                min_accept_steps = Some(min_accept_steps.map_or(d, |prev: u64| prev.min(d)));
            }
        }

        let mut pairs: Vec<(&str, JsonValue)> =
            vec![("targets_reachable", JsonValue::Bool(!reached.is_empty()))];
        pairs.push(("bounded", JsonValue::Bool(bounded)));
        pairs.push((
            "language_empty",
            JsonValue::Bool(!accepts_any),
        ));
        if let Some(steps) = min_accept_steps {
            pairs.push((
                "min_accept_steps",
                JsonValue::Integer(steps as i64),
            ));
        }
        pairs.push((
            "note",
            JsonValue::string(if bounded {
                "max_steps 语义=仅探查前 N 步:targets_reachable=false 不排除更深处可达(受限探查,非完备);language_empty 不受该上界约束,恒为全空间完备判定"
            } else {
                "无 max_steps:全空间穷尽,targets_reachable 与 language_empty 均为完备判定"
            }),
        ));
        if let Some(w) = witness {
            pairs.push(("witness", w));
        }
        Ok(envelope(&pairs))
    }
}

pub fn mk_fsm_reachability() -> Arc<dyn NativeService> {
    Arc::new(FsmReachabilityService)
}

// ============================================================================
// verification_fsm_equivalence
// ============================================================================

struct FsmEquivalenceService;

impl NativeService for FsmEquivalenceService {
    fn execute(&self, args: &JsonValue) -> IoResult {
        // 两台机器:前缀 a_/b_;各自初始状态恒为 0(契约,如实声明)
        let a = fsm::parse_fsm(args, "a_")
            .map_err(|e| format!("机器 A: {e}"))?;
        let b = fsm::parse_fsm(args, "b_")
            .map_err(|e| format!("机器 B: {e}"))?;
        match fsm::dfa_equivalent(&a, &b)? {
            None => Ok(envelope(&[
                ("equivalent", JsonValue::Bool(true)),
                (
                    "note",
                    JsonValue::string(
                        "乘积机全空间遍历无区分态=完全 DFA 等价的完备判定;两机初始状态约定均为 0",
                    ),
                ),
            ])),
            Some(cx) => {
                let word_json: Vec<JsonValue> = cx
                    .word
                    .iter()
                    .map(|&s| JsonValue::Integer(s as i64))
                    .collect();
                let p1: Vec<JsonValue> = cx
                    .path1
                    .iter()
                    .map(|&q| JsonValue::Integer(q as i64))
                    .collect();
                let p2: Vec<JsonValue> = cx
                    .path2
                    .iter()
                    .map(|&q| JsonValue::Integer(q as i64))
                    .collect();
                Ok(envelope(&[
                    ("equivalent", JsonValue::Bool(false)),
                    ("counterexample_word", JsonValue::array(word_json)),
                    ("path_a", JsonValue::array(p1)),
                    ("path_b", JsonValue::array(p2)),
                    ("end_a_accepting", JsonValue::Bool(cx.end1_accept)),
                    ("end_b_accepting", JsonValue::Bool(cx.end2_accept)),
                    (
                        "note",
                        JsonValue::string(
                            "counterexample_word 为最短区分词(长度升序,长度内符号号升序);反例可机械复核:沿词两机终态接受性必不同",
                        ),
                    ),
                ]))
            }
        }
    }
}

pub fn mk_fsm_equivalence() -> Arc<dyn NativeService> {
    Arc::new(FsmEquivalenceService)
}
