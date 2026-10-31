// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! CNF 命题可满足性：确定性 DPLL（单元传播 + 变量首现序 + 正枝先行）。
//!
//! # 确定性
//! 纯整数/布尔域，无浮点、无时钟、无随机；搜索顺序由公式结构完全决定
//! （变量按首现序编号、分支恒先试「正枝」），同输入双跑判定与指派逐位一致。
//!
//! # 完备性诚实边界
//! - `UNSAT` 结果在变量/子句上限内是**完备判定**（DPLL 穷尽搜索空间）；
//! - `SAT` 结果附带一条满足指派（可直接机械复核：逐子句验真）；
//! - 超上限（变量 > 200 或子句 > 2000）→ fail-visible Err 拒解（资源守卫，
//!   不静默降级为启发式）；指数最坏复杂度如实声明——本服务不是 SAT 求解器
//!   竞赛件，定位是规则约束可满足性的**确定性机器断言**。
//!
//! # 算法
//! 1. 解析 DIMACS 风格 CNF：`{vars: [1..n], clauses: [[lit, ...], ...]}`；
//! 2. 单元传播至不动点（子句长度 1 → 强制指派）；
//! 3. 空子句 → 本枝失败；全满足 → SAT；
//! 4. 取未指派的最小变量号（首现序），先试 true（正枝），再试 false。
//!
//! 递归深度 ≤ 变量数 ≤ 200，无栈溢出面。

#![forbid(unsafe_code)]

use std::collections::BTreeMap;

pub const MAX_VARS: usize = 200;
pub const MAX_CLAUSES: usize = 2000;

/// 字面量：非零整数，负号表否定（DIMACS 惯例）。
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Lit(i32);

impl Lit {
    pub(crate) fn var(self) -> usize {
        self.0.unsigned_abs() as usize
    }
    pub(crate) fn is_pos(self) -> bool {
        self.0 > 0
    }
}

/// 公式解析：变量 1..=n 连续编号，子句非空，字面量在变量域内。
pub struct Formula {
    pub num_vars: usize,
    pub clauses: Vec<Vec<Lit>>,
}

pub fn parse_formula(args: &evorule_tcb::JsonValue) -> Result<Formula, String> {
    let num_vars = args
        .get("vars")
        .and_then(|v| v.as_i64())
        .ok_or_else(|| {
            "缺少参数 vars（正整数,变量数）— 自诊断指引: 见服务 parameters 契约".to_string()
        })?;
    if num_vars <= 0 {
        return Err(format!("参数 vars={num_vars} 须为正整数"));
    }
    let num_vars = usize::try_from(num_vars)
        .map_err(|_| format!("参数 vars={num_vars} 超出允许范围"))?;
    if num_vars > MAX_VARS {
        return Err(format!(
            "参数 vars={num_vars} 超上限 {MAX_VARS} — 资源守卫,如实拒解(不静默降级)"
        ));
    }

    let clauses_json = args
        .get("clauses")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            "缺少参数 clauses（二维整数数组,子句列表）— 自诊断指引: 见服务 parameters 契约"
                .to_string()
        })?;
    if clauses_json.len() > MAX_CLAUSES {
        return Err(format!(
            "子句数 {} 超上限 {MAX_CLAUSES} — 资源守卫,如实拒解",
            clauses_json.len()
        ));
    }

    let mut clauses = Vec::with_capacity(clauses_json.len());
    for (ci, cj) in clauses_json.iter().enumerate() {
        let arr = cj
            .as_array()
            .ok_or_else(|| format!("clauses[{ci}] 须为整数数组(子句)"))?;
        if arr.is_empty() {
            // 空子句 = 恒假：语法上允许（公式不可满足），保留语义而不是报错。
            clauses.push(Vec::new());
            continue;
        }
        let mut clause = Vec::with_capacity(arr.len());
        for (li, lj) in arr.iter().enumerate() {
            let raw = lj
                .as_i64()
                .ok_or_else(|| format!("clauses[{ci}][{li}] 须为非零整数(DIMACS 字面量)"))?;
            if raw == 0 {
                return Err(format!("clauses[{ci}][{li}]=0 非法——字面量须非零"));
            }
            let v = raw.unsigned_abs() as usize;
            if v > num_vars {
                return Err(format!(
                    "clauses[{ci}][{li}] 引用变量 {v} 超出 vars={num_vars} 域"
                ));
            }
            clause.push(Lit(raw as i32));
        }
        clauses.push(clause);
    }
    Ok(Formula { num_vars, clauses })
}

/// 指派：变量号 → 布尔值（BTreeMap 保证输出按变量号升序,与遍历序无关）。
pub type Assignment = BTreeMap<usize, bool>;

/// DPLL 主判定。返回 Ok(Some(assignment)) = SAT；Ok(None) = UNSAT（完备）。
pub fn dpll(f: &Formula) -> Result<Option<Assignment>, String> {
    let n = f.num_vars;
    let mut trail: Vec<(usize, bool)> = Vec::with_capacity(n);
    let result = dpll_rec(f, &mut trail)?;
    Ok(result.map(|()| trail.iter().cloned().collect()))
}

/// trail 语义：递归期间记录当前枝的指派路径；失败回溯时弹出本层指派。
fn dpll_rec(f: &Formula, trail: &mut Vec<(usize, bool)>) -> Result<Option<()>, String> {
    // 单元传播至不动点
    loop {
        let mut unit: Option<(usize, bool)> = None;
        for clause in &f.clauses {
            let mut unassigned: Option<Lit> = None;
            let mut satisfied = false;
            let mut unassigned_count = 0usize;
            for lit in clause {
                let a = lookup(trail, lit.var());
                match a {
                    Some(b) => {
                        if b == lit.is_pos() {
                            satisfied = true;
                            break;
                        }
                        // 该字面量为假,子句还指望其他字面量
                    }
                    None => {
                        unassigned = Some(*lit);
                        unassigned_count += 1;
                    }
                }
            }
            if satisfied {
                continue;
            }
            if unassigned_count == 0 {
                // 冲突：本枝 UNSAT
                return Ok(None);
            }
            if unassigned_count == 1 {
                let lit = unassigned
                    .ok_or_else(|| "内部不变量破坏:count=1 但无未指派字面量".to_string())?;
                unit = Some((lit.var(), lit.is_pos()));
                break;
            }
        }
        let Some((var, val)) = unit else { break };
        // 已有同名指派则跳过（传播顺序决定,不重复入栈）
        if lookup(trail, var).is_some() {
            continue;
        }
        trail.push((var, val));
    }

    // 全部子句已满足 → SAT（无需指派所有变量,未指派变量可取任意值;
    // 输出时补 false,恒为合法满足指派,机械复核可证）
    if all_satisfied(f, trail) {
        return Ok(Some(()));
    }

    // 分支：未指派的最小变量号（首现序,确定性）;先试 true 后试 false
    let var = (1..=f.num_vars)
        .find(|v| lookup(trail, *v).is_none())
        .ok_or_else(|| "内部不变量破坏:仍有未满足子句但无未指派变量".to_string())?;
    for val in [true, false] {
        trail.push((var, val));
        let r = dpll_rec(f, trail);
        if r.is_err() {
            // 内部错误也须先弹出本层指派再上抛
            trail.pop();
            return r;
        }
        if matches!(r, Ok(Some(()))) {
            return Ok(Some(()));
        }
        trail.pop();
    }
    Ok(None)
}

fn lookup(trail: &[(usize, bool)], var: usize) -> Option<bool> {
    trail.iter().find(|(v, _)| *v == var).map(|(_, b)| *b)
}

fn all_satisfied(f: &Formula, trail: &[(usize, bool)]) -> bool {
    f.clauses.iter().all(|clause| {
        clause
            .iter()
            .any(|lit| lookup(trail, lit.var()) == Some(lit.is_pos()))
    })
}

/// 满足指派机械复核（自证,也供测试与调用方复算）。
pub fn satisfies(f: &Formula, a: &Assignment) -> bool {
    f.clauses.iter().all(|clause| {
        clause
            .iter()
            .any(|lit| a.get(&lit.var()) == Some(&lit.is_pos()))
    })
}

/// 指派 → JSON（变量升序,未指派补 false）。
pub fn assignment_json(f: &Formula, a: &Assignment) -> Vec<evorule_tcb::JsonValue> {
    (1..=f.num_vars)
        .map(|v| {
            evorule_tcb::JsonValue::Bool(a.get(&v).copied().unwrap_or(false))
        })
        .collect()
}
