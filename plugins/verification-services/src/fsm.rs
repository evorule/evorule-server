// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 显式有限状态机：可达性分析 + 两台完全 DFA 等价判定。
//!
//! # 确定性
//! 纯整数/逻辑域，无浮点、无时钟、无随机；BTreeMap/BTreeSet 保证遍历序
//! 由状态/符号编号决定，同输入双跑判定与 witness 路径逐位一致。
//!
//! # 语义与诚实边界
//! - FSM 采用**显式转移表**形态：状态 0..state_count，符号 0..symbol_count，
//!   每行恰 `symbol_count` 个转移（不全者 fail-visible 拒解——本族只分析
//!   显式给出的机器，不做隐式补全，拒绝"看起来在跑"的假验证）；
//! - 可达性：BFS 最短步数 + 一条 witness 路径（BFS 首达边序,确定性）；
//!   `max_steps` 可选上界（缺省 = 全空间穷尽,上界语义 = 仅探查前 N 步,
//!   未达 ≠ 不可达,出参 `bounded` 字段如实区分完备/受限两态）；
//! - 等价判定：**两台都必须是完全 DFA**（转移函数全定义）——乘积机同步
//!   遍历,任一乘积态恰一方是接受态 → 不等价；此时回溯 BFS 首个区分词
//!   （按长度升序、长度内按符号编号升序,确定性最小反例）；
//! - 规模守卫：状态 ≤ 500、符号 ≤ 64、等价乘积态 ≤ 250000，超限
//!   fail-visible 拒解。
//!
//! # 互补定位
//! inference_cycle_detect 查**规则集**的环；本服务查**显式 FSM**——
//! 形态不同、层次不同，互补不重叠。

#![forbid(unsafe_code)]

use std::collections::{BTreeSet, VecDeque};

use evorule_tcb::JsonValue;

pub const MAX_STATES: usize = 500;
pub const MAX_SYMBOLS: usize = 64;
pub const MAX_PRODUCT: usize = 250_000;

/// 显式转移表 FSM：`transitions[q][s] = q'`（行=状态,列=符号）。
pub struct Fsm {
    pub state_count: usize,
    pub symbol_count: usize,
    pub transitions: Vec<Vec<usize>>,
    pub accepting: BTreeSet<usize>,
}

pub fn parse_fsm(args: &JsonValue, prefix: &str) -> Result<Fsm, String> {
    let state_count = args
        .get(&format!("{prefix}states"))
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("缺少参数 {prefix}states（正整数,状态数）"))?;
    let symbol_count = args
        .get(&format!("{prefix}symbols"))
        .and_then(|v| v.as_i64())
        .ok_or_else(|| format!("缺少参数 {prefix}symbols（正整数,符号数）"))?;
    if state_count <= 0 || state_count as usize > MAX_STATES {
        return Err(format!(
            "{prefix}states={state_count} 须为正整数且 ≤ {MAX_STATES} — 资源守卫"
        ));
    }
    if symbol_count <= 0 || symbol_count as usize > MAX_SYMBOLS {
        return Err(format!(
            "{prefix}symbols={symbol_count} 须为正整数且 ≤ {MAX_SYMBOLS} — 资源守卫"
        ));
    }
    let state_count = state_count as usize;
    let symbol_count = symbol_count as usize;

    let trans_json = args
        .get(&format!("{prefix}transitions"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!("缺少参数 {prefix}transitions（二维整数数组,转移表[state][symbol]）")
        })?;
    if trans_json.len() != state_count {
        return Err(format!(
            "{prefix}transitions 行数 {} ≠ {prefix}states={state_count}",
            trans_json.len()
        ));
    }
    let mut transitions = Vec::with_capacity(state_count);
    for (q, row) in trans_json.iter().enumerate() {
        let arr = row
            .as_array()
            .ok_or_else(|| format!("{prefix}transitions[{q}] 须为整数数组"))?;
        if arr.len() != symbol_count {
            return Err(format!(
                "{}transitions[{q}] 列数 {} ≠ {}symbols={symbol_count} — 不完全转移表须 fail-visible 拒解(不做隐式补全)",
                prefix,
                arr.len(),
                prefix
            ));
        }
        let mut trow = Vec::with_capacity(symbol_count);
        for (s, cell) in arr.iter().enumerate() {
            let t = cell
                .as_i64()
                .ok_or_else(|| format!("{prefix}transitions[{q}][{s}] 须为整数(目标状态)"))?;
            if t < 0 || t as usize >= state_count {
                return Err(format!(
                    "{prefix}transitions[{q}][{s}]={t} 超出状态域 [0,{state_count})"
                ));
            }
            trow.push(t as usize);
        }
        transitions.push(trow);
    }

    let acc_json = args
        .get(&format!("{prefix}accepting"))
        .and_then(|v| v.as_array())
        .ok_or_else(|| format!("缺少参数 {prefix}accepting（整数数组,接受状态列表）"))?;
    let mut accepting = BTreeSet::new();
    for (i, aj) in acc_json.iter().enumerate() {
        let q = aj
            .as_i64()
            .ok_or_else(|| format!("{prefix}accepting[{i}] 须为整数"))?;
        if q < 0 || q as usize >= state_count {
            return Err(format!(
                "{prefix}accepting[{i}]={q} 超出状态域 [0,{state_count})"
            ));
        }
        accepting.insert(q as usize);
    }

    Ok(Fsm {
        state_count,
        symbol_count,
        transitions,
        accepting,
    })
}

/// 可达性 BFS。返回 (每状态最短步数,witness 前驱边)。
/// `max_steps`: None=穷尽(完备); Some(k)=仅探查 ≤k 步(受限)。
pub type ReachResult = (Vec<Option<u64>>, Vec<Option<(usize, usize)>>);

pub fn bfs_reach(fsm: &Fsm, initials: &[usize], max_steps: Option<u64>) -> ReachResult {
    let mut dist: Vec<Option<u64>> = vec![None; fsm.state_count];
    let mut pred: Vec<Option<(usize, usize)>> = vec![None; fsm.state_count];
    let mut queue = VecDeque::new();
    for &q in initials {
        if dist[q].is_none() {
            dist[q] = Some(0);
            queue.push_back(q);
        }
    }
    while let Some(q) = queue.pop_front() {
        // 内部不变量:队列内状态必有距离记录(违例即结构错误,判别式如实暴露)
        let Some(d) = dist[q] else {
            return (vec![None; fsm.state_count], vec![None; fsm.state_count]);
        };
        if let Some(cap) = max_steps {
            if d >= cap {
                continue; // 不再展开 — 受限探查
            }
        }
        for (s, &t) in fsm.transitions[q].iter().enumerate() {
            if dist[t].is_none() {
                dist[t] = Some(d + 1);
                pred[t] = Some((q, s));
                queue.push_back(t);
            }
        }
    }
    (dist, pred)
}

/// witness 路径回溯：目标态 → (起点, 符号序列)。起点为初始态之一
/// （pred 链止于无前驱的初始态,无需再传 initials 判定）。
pub fn witness_path(pred: &[Option<(usize, usize)>], target: usize) -> (usize, Vec<usize>) {
    let mut symbols_rev = Vec::new();
    let mut cur = target;
    while let Some((p, s)) = pred[cur] {
        symbols_rev.push(s);
        cur = p;
    }
    symbols_rev.reverse();
    (cur, symbols_rev)
}

/// DFA 等价：乘积机 BFS,首个区分态回溯最短区分词。
/// 返回 Ok(true) = 等价; Ok(None 区分数据) = 不等价。
pub struct CounterExample {
    pub word: Vec<usize>,
    pub path1: Vec<usize>,
    pub path2: Vec<usize>,
    pub end1_accept: bool,
    pub end2_accept: bool,
}

pub fn dfa_equivalent(a: &Fsm, b: &Fsm) -> Result<Option<CounterExample>, String> {
    if a.symbol_count != b.symbol_count {
        return Err(format!(
            "两机符号数不同(a={} b={})——等价判定要求同一字母表",
            a.symbol_count, b.symbol_count
        ));
    }
    let product = a.state_count * b.state_count;
    if product > MAX_PRODUCT {
        return Err(format!(
            "乘积态数 {product} 超上限 {MAX_PRODUCT} — 资源守卫,如实拒解"
        ));
    }
    let mut seen = vec![false; product];
    let mut pred: Vec<Option<(usize, usize)>> = vec![None; product]; // (prev_pair, symbol)
    let mut queue = VecDeque::new();
    let start = 0usize; // 约定:两机初始状态恒为 0(见 lib.rs 契约)
    seen[start] = true;
    queue.push_back(start);
    while let Some(pair) = queue.pop_front() {
        let q1 = pair / b.state_count;
        let q2 = pair % b.state_count;
        let acc1 = a.accepting.contains(&q1);
        let acc2 = b.accepting.contains(&q2);
        if acc1 != acc2 {
            // 回溯最短区分词（BFS 首达 = 最短;长度内按符号升序由遍历序保证）
            let mut word_rev = Vec::new();
            let mut cur = pair;
            while let Some((p, s)) = pred[cur] {
                word_rev.push(s);
                cur = p;
            }
            word_rev.reverse();
            // 两机轨迹:从各自初始态沿 word 走
            let trace = |fsm: &Fsm, word: &[usize]| -> Vec<usize> {
                let mut path = vec![0usize];
                let mut q = 0usize;
                for &s in word {
                    q = fsm.transitions[q][s];
                    path.push(q);
                }
                path
            };
            let word = word_rev.clone();
            return Ok(Some(CounterExample {
                word,
                path1: trace(a, &word_rev),
                path2: trace(b, &word_rev),
                end1_accept: acc1,
                end2_accept: acc2,
            }));
        }
        for s in 0..a.symbol_count {
            let np = a.transitions[q1][s] * b.state_count + b.transitions[q2][s];
            if !seen[np] {
                seen[np] = true;
                pred[np] = Some((pair, s));
                queue.push_back(np);
            }
        }
    }
    Ok(None)
}
