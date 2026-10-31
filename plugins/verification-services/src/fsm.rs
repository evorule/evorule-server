// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 显式有限状态机：可达性分析、等价判定、死锁判定（活性语义）与语言包含判定。
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
//! - 死锁判定（活性语义）：可达（自初始状态集）但从此无法到达任何接受态
//!   的状态——「卡死在非接受吸收域」，零出边为其特例；不可达态不判定
//!   （诚实边界）；实现=前向可达 BFS + 自接受态反向 BFS 共达闭包，出参附
//!   到首个（升序最小）死锁态的最短 witness；
//! - 语言包含：L(A) ⊆ L(B)——完全 DFA 的补机=接受集翻转（恒可构造），
//!   遍历 L(A)∩补(L(B)) 的乘积可达闭包，混合态（A 接受且补 B 接受）回溯
//!   最短反例词（A 接受而 B 拒绝,可机械复核）；闭包穷尽无混合态=包含成立
//!   的完备判定；字母表独立上限 16（资源守卫）；
//! - 规模守卫：状态 ≤ 500、符号 ≤ 64（语言包含字母表 ≤ 16）、乘积态
//!   ≤ 250000，超限 fail-visible 拒解。
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
/// 语言包含的字母表独立上限:字母表层资源守卫,超限如实拒解
/// (完全 DFA 的补机=接受集翻转,恒线性可构造;此上限为本判定的扫描预算)。
pub const MAX_ALPHABET: usize = 16;

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

/// 死锁判定报告：升序死锁态列表 + 前驱边表 + 前向距离表（后两者供 witness 回溯）。
pub type DeadlockResult = (Vec<usize>, Vec<Option<(usize, usize)>>, Vec<Option<u64>>);

/// 死锁态判定（活性语义）：可达、但从此无法到达任何接受态的状态
/// ——「卡死在非接受吸收域」,零出边是其特例。返回
/// (升序死锁态列表, 前驱边表, 前向距离表)——后两者供 witness 回溯。
/// 不可达态不判定(诚实边界);机器无接受态时全部可达态均为死锁(语言恒空)。
pub fn deadlock_states(fsm: &Fsm, initials: &[usize]) -> DeadlockResult {
    let (fwd_dist, fwd_pred) = bfs_reach(fsm, initials, None);
    // 反向边邻接:rev_edges[t] = [(p, s), ...] 表示 p --s--> t
    let mut rev_edges: Vec<Vec<(usize, usize)>> = vec![Vec::new(); fsm.state_count];
    for q in 0..fsm.state_count {
        for (s, &t) in fsm.transitions[q].iter().enumerate() {
            rev_edges[t].push((q, s));
        }
    }
    // 从接受态反向 BFS:co_dist[q] 有值 ⇔ q 可达某接受态(含自身接受,0 步)
    let mut co_dist: Vec<Option<u64>> = vec![None; fsm.state_count];
    let mut queue = VecDeque::new();
    for &q in &fsm.accepting {
        co_dist[q] = Some(0);
        queue.push_back(q);
    }
    while let Some(q) = queue.pop_front() {
        // 内部不变量:队列内状态必有距离记录(违例即结构错误,判别式如实暴露)
        let Some(d) = co_dist[q] else {
            return (
                Vec::new(),
                vec![None; fsm.state_count],
                vec![None; fsm.state_count],
            );
        };
        for &(p, _s) in &rev_edges[q] {
            if co_dist[p].is_none() {
                co_dist[p] = Some(d + 1);
                queue.push_back(p);
            }
        }
    }
    let mut dead = Vec::new();
    for q in 0..fsm.state_count {
        if fwd_dist[q].is_some() && co_dist[q].is_none() {
            dead.push(q);
        }
    }
    (dead, fwd_pred, fwd_dist)
}

/// 语言包含判定：L(A) ⊆ L(B)。
/// 返回 Ok(None) = 包含成立(乘积可达闭包无混合态,完备判定);
/// Ok(Some(反例数据)) = 不包含,certificate 携带「A 接受而补 B 亦接受」
/// 的反例词与两机轨迹(反例可机械复核);Err = 不可判定形态如实拒解
/// (字母表超上限/乘积超上限——本实现要求 B 为完全 DFA,其补机恒可构造)。
pub struct InclusionCounterExample {
    pub word: Vec<usize>,
    pub path_a: Vec<usize>,
    pub path_b: Vec<usize>,
}

pub fn language_inclusion(a: &Fsm, b: &Fsm) -> Result<Option<InclusionCounterExample>, String> {
    if a.symbol_count != b.symbol_count {
        return Err(format!(
            "两机符号数不同(a={} b={})——语言包含要求同一字母表",
            a.symbol_count, b.symbol_count
        ));
    }
    if b.symbol_count > MAX_ALPHABET {
        return Err(format!(
            "符号数 {} 超语言包含上限 {MAX_ALPHABET} — 字母表资源守卫,如实拒解",
            b.symbol_count
        ));
    }
    if b.accepting.len() == b.state_count {
        // B 接受一切词,包含恒成立(乘积遍历必无反例,直接短路,语义等价)
        return Ok(None);
    }
    let complement_b = Fsm {
        state_count: b.state_count,
        symbol_count: b.symbol_count,
        transitions: b.transitions.clone(),
        accepting: (0..b.state_count)
            .filter(|q| !b.accepting.contains(q))
            .collect(),
    };
    // L(A) ⊆ L(B)  ⇔  L(A) ∩ complement(L(B)) = ∅
    // 乘积可达闭包上任何 (接受于 A, 接受于补 B) 混合态给出反例词。
    let product = a.state_count * b.state_count;
    if product > MAX_PRODUCT {
        return Err(format!(
            "乘积态数 {product} 超上限 {MAX_PRODUCT} — 资源守卫,如实拒解"
        ));
    }
    let mut seen = vec![false; product];
    let mut pred: Vec<Option<(usize, usize)>> = vec![None; product];
    let mut queue = VecDeque::new();
    let start = 0usize; // 约定:两机初始状态恒为 0(见 lib.rs 契约)
    seen[start] = true;
    queue.push_back(start);
    while let Some(pair) = queue.pop_front() {
        let q1 = pair / b.state_count;
        let q2 = pair % b.state_count;
        let in_a = a.accepting.contains(&q1);
        let in_cb = complement_b.accepting.contains(&q2);
        if in_a && in_cb {
            let mut word_rev = Vec::new();
            let mut cur = pair;
            while let Some((p, s)) = pred[cur] {
                word_rev.push(s);
                cur = p;
            }
            word_rev.reverse();
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
            return Ok(Some(InclusionCounterExample {
                path_a: trace(a, &word_rev),
                path_b: trace(b, &word_rev),
                word,
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
