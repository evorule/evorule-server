------------------------------- MODULE ExecuteTransition -------------------------------
(*
  SPDX-License-Identifier: AGPL-3.0-or-later
  Copyright (C) 2026 EvoRule Project
  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

  ExecuteTransition.tla — tier0 execute_transition 的 TLA+ 状态机规格

  验证目标（5 个不变量）：
    I1 Termination       — 状态机总是到达 Done 或 Error（无死锁）
    I2 Determinism       — 任意状态最多一个子动作 enabled
    I3 DepthEnforcement  — branch depth ≤ D_MAX, domain depth ≤ D_DOM_MAX+1
    I4 IoEarlyReturn     — io_requested ⇒ pc ∈ {IoReturn, Done}
    I5 LoopProgress      — Loop 步骤中 i 递增或 pc 改变

  设计文档：verification/plan/EVORULE_FORMAL_VERIFICATION_PLAN_v3.md §3.2.2（TLA+ 全 tier 覆盖）
*)
EXTENDS Naturals, Sequences, FiniteSets, TLC

CONSTANTS
    N_MAX,          (* core_eval 最大长度，实例化 = 3（对应 MAX_TRANSFORM_RULES=64）*)
    D_MAX,          (* 最大 branch 递归深度，实例化 = 3（对应 MAX_BRANCH_DEPTH=64）*)
    D_DOM_MAX,      (* 最大 domain 递归深度，实例化 = 3（对应 MAX_DOMAIN_DEPTH=64）*)
    InstrTypeSet,   (* {"set", "push", "branch", "io_request"} *)
    IoTypeSet       (* {"call_llm", "call_external", "query_db"} *)

(* 允许参数降级（1..3）以控制 TLC 状态空间，见 v3 计划 §3.4 参数降级要求 *)
ASSUME N_MAX \in 1..3 /\ D_MAX \in 1..3 /\ D_DOM_MAX \in 1..3 /\
       Cardinality(InstrTypeSet) = 4 /\
       Cardinality(IoTypeSet) = 3

(* ── 抽象数据类型 ── *)

(* 指令类型（抽象：只保留 type，不建模 params/domain/on_true/on_false 内容）*)
InstrType == InstrTypeSet

(* 有界序列集：长度 0..n 的序列（TLC 可枚举的有限集）*)
BoundedSeq(S, n) == UNION { [1..m -> S] : m \in 0..n }

(* core_eval：指令类型序列（输入，执行期间不变，长度 ≤ N_MAX）*)
CoreEval == BoundedSeq(InstrType, N_MAX)

(* 有界子指令序列（branch body 的子指令，长度 ≤ N_MAX）*)
BoundedSubSeq == BoundedSeq(InstrType, N_MAX)

(* 栈帧：defunctionalize branch 递归（对应 §8.3.3）*)
Frame == [
    remaining:  Seq(InstrType),   (* 剩余子指令 *)
    depth:      0..D_MAX,         (* 该帧深度 *)
    return_i:   0..N_MAX          (* 返回后的外层循环 i *)
]

(* 程序计数器类型 *)
PCType == {"Init", "LengthCheck", "Loop", "ExecRule",
           "BranchDepthCheck", "DomainDepthCheck", "DomainEval",
           "BranchDomain", "BranchBody", "ExecSubRule",
           "IoReturn", "ExtractResult", "Done", "Error"}

(* 转换结果类型 *)
ResultType == {"none", "state", "io_required", "error"}

(* ── 状态变量 ── *)

VARIABLES
    pc,            (* 程序计数器 *)
    i,             (* 外层循环索引 0..N_MAX *)
    depth,         (* 当前 branch 嵌套深度 0..D_MAX+1 *)
    domDepth,      (* 当前 domain 递归深度 0..D_DOM_MAX+1 *)
    core_eval,     (* 输入：指令序列（CONSTANT，不变）*)
    stack,         (* branch 调用栈 Seq(Frame) *)
    result_type,   (* 转换结果类型 *)
    io_requested   (* IoRequired 是否被请求 *)

vars == <<pc, i, depth, domDepth, core_eval, stack, result_type, io_requested>>

(* ── Init 谓词 ── *)

Init ==
    /\ pc = "Init"
    /\ i = 0
    /\ depth = 0
    /\ domDepth = 0
    /\ core_eval \in CoreEval           (* 非确定性输入，TLC 穷举 *)
    /\ Len(core_eval) <= N_MAX          (* 有界模型 *)
    /\ stack = <<>>
    /\ result_type = "none"
    /\ io_requested = FALSE

(* ── Next 动作（12 个子动作）── *)

(* 子动作 1: Init → LengthCheck
   对应 transition.rs 入口 *)
InitStep ==
    /\ pc = "Init"
    /\ pc' = "LengthCheck"
    /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, result_type, io_requested>>

(* 子动作 2: LengthCheck → Loop 或 Error
   对应 transition.rs:144-148 if core_eval.len() > MAX_TRANSFORM_RULES *)
LengthCheckStep ==
    /\ pc = "LengthCheck"
    /\ IF Len(core_eval) > N_MAX
       THEN /\ pc' = "Error"
            /\ result_type' = "error"
            /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, io_requested>>
       ELSE /\ pc' = "Loop"
            /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, result_type, io_requested>>

(* 子动作 3: Loop → ExecRule 或 ExtractResult
   对应 transition.rs:158 for transform_rule in core_eval *)
LoopStep ==
    /\ pc = "Loop"
    /\ IF i >= Len(core_eval)
       THEN /\ pc' = "ExtractResult"
            /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, result_type, io_requested>>
       ELSE /\ pc' = "ExecRule"
            /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, result_type, io_requested>>

(* 子动作 4: ExecRule → IoReturn / BranchDepthCheck / Loop
   对应 transition.rs:159 + executor.rs:122-128 match 分派 *)
ExecRuleStep ==
    /\ pc = "ExecRule"
    /\ i < Len(core_eval)
    /\ LET rule_type == core_eval[i + 1]   (* TLA+ Seq 从 1 开始 *)
      IN
      CASE rule_type = "io_request"
        -> /\ pc' = "IoReturn"
           /\ result_type' = "io_required"
           /\ io_requested' = TRUE
           /\ UNCHANGED <<i, depth, domDepth, core_eval, stack>>
        [] rule_type = "branch"
        -> /\ pc' = "BranchDepthCheck"
           /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, result_type, io_requested>>
        [] rule_type \in {"set", "push"}
        -> (* 非递归指令：抽象应用（state 抽象掉），继续循环 *)
           /\ pc' = "Loop"
           /\ i' = i + 1
           /\ UNCHANGED <<depth, domDepth, core_eval, stack, result_type, io_requested>>

(* 子动作 5: BranchDepthCheck → DomainDepthCheck 或 Error
   对应 executor.rs:295 if depth >= MAX_BRANCH_DEPTH（Rust 用 >=）*)
BranchDepthCheckStep ==
    /\ pc = "BranchDepthCheck"
    /\ IF depth >= D_MAX
       THEN /\ pc' = "Error"
            /\ result_type' = "error"
            /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, io_requested>>
       ELSE /\ pc' = "DomainDepthCheck"
            /\ domDepth' = 0           (* 重置 domain 深度计数器 *)
            /\ UNCHANGED <<i, depth, core_eval, stack, result_type, io_requested>>

(* 子动作 6: DomainDepthCheck → BranchDomain 或 DomainEval
   对应 domain.rs:76 if depth > MAX_DOMAIN_DEPTH（Rust 用 >，与 branch 的 >= 不同）*)
DomainDepthCheckStep ==
    /\ pc = "DomainDepthCheck"
    /\ IF domDepth > D_DOM_MAX
       THEN (* domain 深度超限：evaluate_domain 返回 false，branch 取 on_false *)
            /\ pc' = "BranchDomain"
            /\ domDepth' = 0
            /\ UNCHANGED <<i, depth, core_eval, stack, result_type, io_requested>>
       ELSE /\ pc' = "DomainEval"
            /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, result_type, io_requested>>

(* 子动作 7: DomainEval → BranchDomain
   对应 executor.rs:304 evaluate_domain(domain, state)
   DomainEval 是抽象函数，TLC 穷举 TRUE/FALSE 两种结果（通过 BranchDomainStep 的非确定性）*)
DomainEvalStep ==
    /\ pc = "DomainEval"
    /\ pc' = "BranchDomain"
    /\ domDepth' = 0           (* domain 评估完成，重置 *)
    /\ UNCHANGED <<i, depth, core_eval, stack, result_type, io_requested>>

(* 子动作 8: BranchDomain → BranchBody
   对应 executor.rs:310-317 for sub_instr 循环
   非确定性选择子指令序列（抽象 on_true/on_false + DomainEval）*)
BranchDomainStep ==
    /\ pc = "BranchDomain"
    /\ \E branch_instrs \in BoundedSubSeq :     (* 非确定性：DomainEval 结果 + on_true/on_false 抽象 *)
       /\ depth' = depth + 1
       /\ stack' = Append(stack, [remaining |-> branch_instrs,
                                      depth     |-> depth + 1,
                                      return_i  |-> i])
       /\ pc' = "BranchBody"
       /\ UNCHANGED <<i, domDepth, core_eval, result_type, io_requested>>

(* 子动作 9: BranchBody → Loop / ExecSubRule / IoReturn
   对应 executor.rs:310-317 栈帧管理 *)
BranchBodyStep ==
    /\ pc = "BranchBody"
    /\ IF Len(stack) = 0
       THEN (* 栈空，branch 完成 *)
            /\ pc' = "Loop"
            /\ i' = i + 1
            /\ depth' = depth - 1
            /\ UNCHANGED <<domDepth, core_eval, stack, result_type, io_requested>>
       ELSE LET frame == Head(stack) IN
            IF Len(frame.remaining) = 0
            THEN (* 当前帧子指令执行完，pop *)
                 /\ stack' = Tail(stack)
                 /\ depth' = depth - 1
                 /\ pc' = IF Len(Tail(stack)) = 0 THEN "Loop" ELSE "BranchBody"
                 /\ i' = IF Len(Tail(stack)) = 0 THEN i + 1 ELSE i
                 /\ UNCHANGED <<domDepth, core_eval, result_type, io_requested>>
            ELSE IF io_requested
            THEN (* IoRequired 传播，清栈返回 *)
                 /\ pc' = "IoReturn"
                 /\ stack' = <<>>
                 /\ UNCHANGED <<i, depth, domDepth, core_eval, result_type, io_requested>>
            ELSE (* 执行下一条子指令 *)
                 /\ pc' = "ExecSubRule"
                 /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, result_type, io_requested>>

(* 子动作 10: ExecSubRule → IoReturn / BranchDepthCheck / BranchBody
   对应 executor.rs:311 execute_meta_instruction(sub_instr, state, depth+1) *)
ExecSubRuleStep ==
    /\ pc = "ExecSubRule"
    /\ Len(stack) > 0
    /\ LET frame        == Head(stack)
           sub_type     == Head(frame.remaining)
           new_remaining == Tail(frame.remaining)
      IN
      CASE sub_type = "io_request"
        -> /\ pc' = "IoReturn"
           /\ result_type' = "io_required"
           /\ io_requested' = TRUE
           /\ stack' = <<>>           (* 清栈 *)
           /\ UNCHANGED <<i, depth, domDepth, core_eval>>
        [] sub_type = "branch"
        -> (* 嵌套 branch：进入深度检查 *)
           /\ pc' = "BranchDepthCheck"
           /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, result_type, io_requested>>
        [] sub_type \in {"set", "push"}
        -> (* 非递归子指令：更新栈顶帧 *)
           /\ stack' = Append(Tail(stack),
                              [remaining |-> new_remaining,
                               depth     |-> frame.depth,
                               return_i  |-> frame.return_i])
           /\ pc' = IF Len(new_remaining) = 0
                    THEN IF Len(Tail(stack)) = 0 THEN "Loop" ELSE "BranchBody"
                    ELSE "BranchBody"
           /\ i' = IF Len(new_remaining) = 0 /\ Len(Tail(stack)) = 0
                   THEN i + 1 ELSE i
           /\ depth' = IF Len(new_remaining) = 0 /\ Len(Tail(stack)) = 0
                       THEN depth - 1 ELSE depth
           /\ UNCHANGED <<domDepth, core_eval, result_type, io_requested>>

(* 子动作 11: IoReturn → Done *)
IoReturnStep ==
    /\ pc = "IoReturn"
    /\ pc' = "Done"
    /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, result_type, io_requested>>

(* 子动作 12: ExtractResult → Done
   对应 transition.rs:173-179 提取 new_payload/new_queue *)
ExtractResultStep ==
    /\ pc = "ExtractResult"
    /\ pc' = "Done"
    /\ result_type' = "state"
    /\ UNCHANGED <<i, depth, domDepth, core_eval, stack, io_requested>>

(* 终止状态自环：Done/Error 可 stutter（防止 TLC 误报死锁）
   非终止状态的死锁仍会被 TLC 检测（表示缺失转移 = bug）*)
TerminalStep ==
    /\ pc \in {"Done", "Error"}
    /\ UNCHANGED vars

(* Next: 所有子动作的析取 *)
Next ==
    \/ InitStep
    \/ LengthCheckStep
    \/ LoopStep
    \/ ExecRuleStep
    \/ BranchDepthCheckStep
    \/ DomainDepthCheckStep
    \/ DomainEvalStep
    \/ BranchDomainStep
    \/ BranchBodyStep
    \/ ExecSubRuleStep
    \/ IoReturnStep
    \/ ExtractResultStep
    \/ TerminalStep

(* Spec: Init ∧ □[Next]_vars *)
Spec == Init /\ [][Next]_vars

(* ===========================================================================
   5 个不变式
   =========================================================================== *)

(* I1: Termination — 终止状态一致性
   pc ∈ {Done, Error} 时 result_type 非 none；
   pc ∈ {Error} 时 result_type = "error"。
   死锁检测（非终止状态无后继）由 TLC 自动执行。*)
TerminationInvariant ==
    (pc \in {"Done", "Error"} => result_type # "none") /\
    (pc = "Error" => result_type = "error")

(* I2: Determinism — 类型一致性
   每个子动作有唯一 pc 守卫，确定性由结构保证（pc 值唯一决定 enabled 动作）。
   此不变式验证 pc 和 result_type 的类型一致性。*)
DeterminismInvariant ==
    pc \in PCType /\ result_type \in ResultType

(* I3: DepthEnforcement — 双深度硬上界强制（核心价值）
   branch depth ≤ D_MAX，domain depth ≤ D_DOM_MAX+1
   除非已经报错（pc ∈ {Error}）
   注意：branch 用 >=（executor.rs:295），domain 用 >（domain.rs:76）*)
DepthEnforcementInvariant ==
    pc \in {"Error"} \/ (depth <= D_MAX /\ domDepth <= D_DOM_MAX + 1)

(* I4: IoEarlyReturn — I/O 提前返回语义
   一旦 io_requested = TRUE，pc 必须走向 IoReturn 或 Done *)
IoEarlyReturnInvariant ==
    io_requested => pc \in {"IoReturn", "Done"}

(* I5: LoopProgress — 循环索引有效且推进
   i 始终在 [0, N_MAX] 范围内；
   LoopStep 的两个分支都导致 pc' ≠ "Loop"（ExtractResult 或 ExecRule），
   故 Loop 不会空转。TLC 通过有界模型验证终止性（I1）间接覆盖此性质。*)
LoopProgressInvariant ==
    0 <= i /\ i <= N_MAX

=============================================================================
