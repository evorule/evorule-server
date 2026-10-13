#!/bin/bash
# =============================================================================
# run_kani_proofs.sh - evorule-reactor Kani 验证脚本
#
# 运行全部 11 个 reactor proof。注意：
# - 3 个简单 proof (version_monotonic / max_rounds_termination / cause_queue_sync) 可在 CI 预算内完成
# - 8 个复杂 proof 实测 7 PASS + 1 TIMEOUT (invariant_io_count_force_remove, BTreeSet 状态爆炸)
#   涉及堆分配数据结构 (Vec/VecDeque/BTreeSet), 跨 Kani 版本可能不稳定, 建议本地运行
#
# 用法: bash run_kani_proofs.sh [proof_name]
#   不带参数: 跑全部 11 个 proof
#   带参数:   跑指定 proof
# =============================================================================
cd "$(dirname "$0")/.." || exit 1
source ~/.cargo/env 2>/dev/null

PROOFS=(
  # === CI 子集 (3 个,简单状态机验证) ===
  invariant_version_monotonic
  max_rounds_termination
  invariant_cause_queue_sync
  # === 本地运行 (8 个,实测 7 PASS + 1 TIMEOUT) ===
  invariant_io_count_register_complete
  invariant_io_count_force_remove
  invariant_io_recovery_iff_result
  command_does_not_decrease_queue
  proof_fact_log_append_monotonic
  proof_hash_chain_back_link
  proof_reactor_invariants_preserved_after_pure_ops
  proof_phase_state_machine_cannot_jump
)

# 如果传了参数,只跑指定的 proof
if [ -n "$1" ]; then
  PROOFS=("$1")
fi

PASS=0
FAIL=0
TIMEOUT=0

for proof in "${PROOFS[@]}"; do
  echo "======== $proof ========"
  start=$(date +%s)
  output=$(timeout 600 cargo kani -p evorule-reactor --harness "$proof" --output-format=terse 2>&1)
  exit_code=$?
  end=$(date +%s)
  dur=$((end - start))
  echo "$output" | grep -E "VERIFICATION|Checking|FAILED|SUCCESSFUL|failed|Summary" | head -8
  if [ $exit_code -eq 0 ]; then
    echo ">>> PASS (${dur}s)"
    PASS=$((PASS + 1))
  elif [ $exit_code -eq 124 ]; then
    echo ">>> TIMEOUT (${dur}s, 600s limit)"
    TIMEOUT=$((TIMEOUT + 1))
  else
    echo ">>> FAIL (${dur}s, exit=$exit_code)"
    FAIL=$((FAIL + 1))
  fi
  echo ""
done

echo "======== SUMMARY ========"
echo "PASS=$PASS FAIL=$FAIL TIMEOUT=$TIMEOUT TOTAL=$((PASS + FAIL + TIMEOUT))"
