#!/usr/bin/env bash
# =============================================================================
# collect_kani_artifacts.sh - Kani 超时/失败 proof 中间产物收集器
#
# 用途：当 Kani proof 因 CBMC 状态爆炸超时或失败时，自动收集所有中间产物
#       （symtab、type_map、goto binary、stdout/stderr 等）到独立目录，
#       便于后续分析（如 unwind bound 调整、状态爆炸原因定位）。
#
# 收集的产物：
#   1. stdout.log / stderr.log     - Kani 完整输出
#   2. *.out                       - CBMC Goto 二进制（已编译的模型）
#   3. *.symtab.out                - 符号表（变量、函数名映射）
#   4. *.type_map.json             - 类型映射（Rust 类型 → CBMC 类型）
#   5. *.pretty_name_map.json      - 美化名称映射
#   6. kani-metadata.json          - Kani 编译元数据
#   7. summary.txt                 - 运行摘要（状态、耗时、失败原因）
#   8. counterexample/             - 反例文件（如验证失败，存在则收集）
#   9. witness/                    - witness 文件（如存在）
#
# 用法：
#   # 收集单个 proof 的产物（300s 超时）
#   ./evorule-reactor/collect_kani_artifacts.sh --harness invariant_io_count_register_complete --timeout 300
#
#   # 收集所有超时/失败 proof 的产物
#   ./evorule-reactor/collect_kani_artifacts.sh --all --timeout 600
#
#   # 指定输出目录
#   ./evorule-reactor/collect_kani_artifacts.sh --harness X --output-dir ./kani-artifacts
#
#   # 列出所有可用 proof
#   ./evorule-reactor/collect_kani_artifacts.sh --list
#
#   # 分析已收集的产物（生成状态爆炸分析报告）
#   ./evorule-reactor/collect_kani_artifacts.sh --analyze ./kani-artifacts/invariant_io_count_register_complete
# =============================================================================

set -euo pipefail

# === 配置 ===
WORKSPACE_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
CRATE="evorule-reactor"
DEFAULT_TIMEOUT=300  # 秒
DEFAULT_OUTPUT_DIR="${WORKSPACE_ROOT}/kani-artifacts"

# Kani deps 目录（target 下的 deps）
KANI_DEPS_DIR=""  # 运行时自动探测

# === 颜色 ===
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m' # No Color

# === 辅助函数 ===
log_info()  { echo -e "${BLUE}[INFO]${NC}  $*" >&2; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC}  $*" >&2; }
log_error() { echo -e "${RED}[ERROR]${NC} $*" >&2; }
log_ok()    { echo -e "${GREEN}[OK]${NC}    $*" >&2; }

# 探测 Kani deps 目录
detect_kani_deps_dir() {
    local build_dir="${WORKSPACE_ROOT}/.build/rust/kani"
    if [ -d "$build_dir" ]; then
        # 找最新的 target 目录
        local latest
        latest=$(find "$build_dir" -maxdepth 2 -type d -name "debug" | head -1)
        if [ -n "$latest" ] && [ -d "${latest}/deps" ]; then
            KANI_DEPS_DIR="${latest}/deps"
            return 0
        fi
    fi
    # fallback: cargo target 目录
    local target_dir="${WORKSPACE_ROOT}/target/kani/debug/deps"
    if [ -d "$target_dir" ]; then
        KANI_DEPS_DIR="$target_dir"
        return 0
    fi
    return 1
}

# 列出所有可用的 proof harness
list_proofs() {
    log_info "可用的 Kani proof harness（${CRATE}）："
    local proof_file="${WORKSPACE_ROOT}/${CRATE}/verification/kani_proofs.rs"
    if [ -f "$proof_file" ]; then
        grep -A1 '#\[kani::proof' "$proof_file" \
            | grep -E '^(pub )?fn ' \
            | sed 's/.*fn /  - /; s/().*//' \
            | sort
    else
        log_error "找不到 verification/kani_proofs.rs，请确认 crate 路径"
        return 1
    fi
}

# 提取 proof 的完整符号名（用于匹配 deps 中的文件）
# 输入：short name（如 invariant_io_count_register_complete）
# 输出：完整符号名前缀（用于 glob 匹配）
find_proof_file_prefix() {
    local short_name="$1"
    if [ -z "$KANI_DEPS_DIR" ]; then
        detect_kani_deps_dir || return 1
    fi
    # 在 deps 目录中查找匹配的 .out 文件
    local found
    found=$(find "$KANI_DEPS_DIR" -name "*${short_name}*.out" -type f 2>/dev/null | head -1)
    if [ -z "$found" ]; then
        return 1
    fi
    # 返回去掉 .out 后缀的 basename
    basename "$found" .out
}

# 收集单个 proof 的产物
# 参数：
#   $1 - harness 名称（短名）
#   $2 - 超时时间（秒）
#   $3 - 输出目录
collect_single_proof() {
    local harness="$1"
    local timeout_sec="$2"
    local output_root="$3"
    local output_dir="${output_root}/${harness}"
    local start_time
    start_time=$(date +%s)

    log_info "收集 proof 产物：${harness}"
    log_info "  超时：${timeout_sec}s"
    log_info "  输出目录：${output_dir}"

    mkdir -p "$output_dir"

    # 运行 Kani，捕获输出，超时处理
    local exit_code=0
    local stdout_file="${output_dir}/stdout.log"
    local stderr_file="${output_dir}/stderr.log"

    log_info "  运行 cargo kani --harness ${harness} ..."

    # 使用 timeout 命令限制运行时间
    # 注意：timeout 在超时后返回 124
    cd "$WORKSPACE_ROOT"
    timeout "${timeout_sec}" cargo kani -p "$CRATE" --harness "$harness" --output-format=terse \
        >"$stdout_file" 2>"$stderr_file" || exit_code=$?

    local end_time
    end_time=$(date +%s)
    local duration=$((end_time - start_time))

    # 判定结果
    local result="unknown"
    if [ $exit_code -eq 124 ]; then
        result="TIMEOUT"
        log_warn "  Proof 超时（${timeout_sec}s）"
    elif [ $exit_code -eq 0 ]; then
        if grep -q "SUCCESSFUL" "$stdout_file"; then
            result="PASSED"
            log_ok "  Proof 通过（${duration}s）"
        else
            result="FAILED"
            log_warn "  Proof 失败（${duration}s）"
        fi
    else
        result="ERROR"
        log_error "  Kani 运行错误（exit=${exit_code}）"
    fi

    # 探测 deps 目录
    detect_kani_deps_dir || {
        log_warn "  无法定位 Kani deps 目录，跳过产物收集"
        write_summary "$output_dir" "$harness" "$result" "$duration" "$exit_code"
        return 0
    }

    log_info "  收集中间产物..."

    # 查找 proof 对应的文件前缀
    local file_prefix
    file_prefix=$(find_proof_file_prefix "$harness") || {
        log_warn "  未找到 proof 对应的编译产物（可能未编译成功）"
        write_summary "$output_dir" "$harness" "$result" "$duration" "$exit_code"
        return 0
    }

    log_info "  文件前缀：${file_prefix}"

    # 收集各类产物
    local artifacts_collected=0

    # Goto 二进制（.out）
    if [ -f "${KANI_DEPS_DIR}/${file_prefix}.out" ]; then
        cp "${KANI_DEPS_DIR}/${file_prefix}.out" "$output_dir/"
        artifacts_collected=$((artifacts_collected + 1))
    fi

    # 符号表（.symtab.out）
    if [ -f "${KANI_DEPS_DIR}/${file_prefix}.symtab.out" ]; then
        cp "${KANI_DEPS_DIR}/${file_prefix}.symtab.out" "$output_dir/"
        artifacts_collected=$((artifacts_collected + 1))
    fi

    # 类型映射（.type_map.json）
    if [ -f "${KANI_DEPS_DIR}/${file_prefix}.type_map.json" ]; then
        cp "${KANI_DEPS_DIR}/${file_prefix}.type_map.json" "$output_dir/"
        artifacts_collected=$((artifacts_collected + 1))
    fi

    # 美化名称映射（.pretty_name_map.json）
    if [ -f "${KANI_DEPS_DIR}/${file_prefix}.pretty_name_map.json" ]; then
        cp "${KANI_DEPS_DIR}/${file_prefix}.pretty_name_map.json" "$output_dir/"
        artifacts_collected=$((artifacts_collected + 1))
    fi

    # Kani 元数据
    if [ -f "${KANI_DEPS_DIR}/evorule_reactor.kani-metadata.json" ]; then
        cp "${KANI_DEPS_DIR}/evorule_reactor.kani-metadata.json" "$output_dir/"
        artifacts_collected=$((artifacts_collected + 1))
    fi

    # 收集反例（counterexample）- Kani 验证失败时生成
    local cex_dir="${output_dir}/counterexample"
    mkdir -p "$cex_dir"
    # Kani 默认会在 target 目录生成反例文件
    local cex_found=0
    if [ -d "${WORKSPACE_ROOT}/target/kani" ]; then
        find "${WORKSPACE_ROOT}/target/kani" -name "*counterexample*" -type f 2>/dev/null | while read -r f; do
            cp "$f" "$cex_dir/"
            cex_found=$((cex_found + 1))
        done
    fi
    # 也检查 .build 目录
    if [ -d "${WORKSPACE_ROOT}/.build/rust/kani" ]; then
        find "${WORKSPACE_ROOT}/.build/rust/kani" -name "*counterexample*" -type f 2>/dev/null | while read -r f; do
            cp "$f" "$cex_dir/"
            cex_found=$((cex_found + 1))
        done
    fi
    # 如果没找到反例文件，删除空目录
    if [ "$(ls -A "$cex_dir" 2>/dev/null | wc -l)" -eq 0 ]; then
        rmdir "$cex_dir" 2>/dev/null || true
    else
        artifacts_collected=$((artifacts_collected + 1))
    fi

    # 收集 witness 文件
    local witness_dir="${output_dir}/witness"
    mkdir -p "$witness_dir"
    local witness_found=0
    if [ -d "${WORKSPACE_ROOT}/target/kani" ]; then
        find "${WORKSPACE_ROOT}/target/kani" -name "*witness*" -type f 2>/dev/null | while read -r f; do
            cp "$f" "$witness_dir/"
            witness_found=$((witness_found + 1))
        done
    fi
    if [ -d "${WORKSPACE_ROOT}/.build/rust/kani" ]; then
        find "${WORKSPACE_ROOT}/.build/rust/kani" -name "*witness*" -type f 2>/dev/null | while read -r f; do
            cp "$f" "$witness_dir/"
            witness_found=$((witness_found + 1))
        done
    fi
    if [ "$(ls -A "$witness_dir" 2>/dev/null | wc -l)" -eq 0 ]; then
        rmdir "$witness_dir" 2>/dev/null || true
    else
        artifacts_collected=$((artifacts_collected + 1))
    fi

    log_ok "  收集完成：${artifacts_collected} 类产物"

    # 写摘要
    write_summary "$output_dir" "$harness" "$result" "$duration" "$exit_code" "$artifacts_collected"

    # 打印文件大小
    echo ""
    log_info "产物清单："
    find "$output_dir" -type f -exec ls -lh {} \; | awk '{print "  " $5, $NF}' | sort -k2

    echo ""
    if [ "$result" = "TIMEOUT" ]; then
        log_warn "建议的下一步分析："
        echo "  1. 查看 stdout.log 最后 50 行了解超时位置"
        echo "  2. 运行 --analyze 生成状态爆炸分析报告"
        echo "  3. 尝试调整 unwind bound: --default-unwind 200"
        echo "  4. 尝试缩小 proof 范围（减少 BTreeSet 元素数）"
    fi
}

# 写入摘要文件
write_summary() {
    local output_dir="$1"
    local harness="$2"
    local result="$3"
    local duration="$4"
    local exit_code="${5:-0}"
    local artifacts_count="${6:-0}"

    local summary_file="${output_dir}/summary.txt"
    cat > "$summary_file" <<EOF
Kani Proof 产物收集摘要
========================

Proof:     ${harness}
Crate:     ${CRATE}
Result:    ${result}
Duration:  ${duration}s
Exit Code: ${exit_code}
Artifacts: ${artifacts_count} 类
Timestamp: $(date -Iseconds)

收集的产物：
EOF

    find "$output_dir" -type f ! -name "summary.txt" | sort | while read -r f; do
        local size
        size=$(ls -lh "$f" | awk '{print $5}')
        local rel_path="${f#${output_dir}/}"
        echo "  - ${rel_path} (${size})" >> "$summary_file"
    done

    echo "" >> "$summary_file"
    echo "stdout.log 最后 20 行：" >> "$summary_file"
    echo "----------------------------------------" >> "$summary_file"
    tail -20 "${output_dir}/stdout.log" 2>/dev/null >> "$summary_file" || echo "  (无 stdout)" >> "$summary_file"
    echo "----------------------------------------" >> "$summary_file"

    if [ -f "${output_dir}/stderr.log" ]; then
        echo "" >> "$summary_file"
        echo "stderr.log 最后 20 行：" >> "$summary_file"
        echo "----------------------------------------" >> "$summary_file"
        tail -20 "${output_dir}/stderr.log" 2>/dev/null >> "$summary_file" || echo "  (无 stderr)" >> "$summary_file"
        echo "----------------------------------------" >> "$summary_file"
    fi
}

# 分析已收集的产物（生成状态爆炸分析报告）
analyze_artifacts() {
    local artifact_dir="$1"

    if [ ! -d "$artifact_dir" ]; then
        log_error "目录不存在：${artifact_dir}"
        return 1
    fi

    log_info "分析产物目录：${artifact_dir}"
    echo ""

    # 读取摘要
    if [ -f "${artifact_dir}/summary.txt" ]; then
        echo "=== 摘要 ==="
        head -10 "${artifact_dir}/summary.txt"
        echo ""
    fi

    # 分析 stdout 中的关键信息
    if [ -f "${artifact_dir}/stdout.log" ]; then
        echo "=== Kani 输出分析 ==="

        # 检查是否有 CBMC 相关错误
        if grep -q "CBMC out of memory" "${artifact_dir}/stdout.log" 2>/dev/null; then
            echo "  ❌ CBMC 内存溢出（OOM）"
            echo "     → 建议：拆分 proof、减少数据结构规模、使用 --default-unwind"
        fi

        if grep -q "VERIFICATION.*FAILED" "${artifact_dir}/stdout.log" 2>/dev/null; then
            echo "  ❌ 验证失败（存在反例）"
            local failed_checks
            failed_checks=$(grep -c "Failed Checks:" "${artifact_dir}/stdout.log" 2>/dev/null || echo "0")
            echo "     失败检查数：${failed_checks}"
            grep "Failed Checks:" "${artifact_dir}/stdout.log" 2>/dev/null | head -5 | sed 's/^/       /'
        fi

        if grep -q "VERIFICATION.*SUCCESSFUL" "${artifact_dir}/stdout.log" 2>/dev/null; then
            echo "  ✅ 验证成功"
            local passed
            passed=$(grep "VERIFICATION RESULT:" -A1 "${artifact_dir}/stdout.log" 2>/dev/null | tail -1)
            echo "     ${passed}"
        fi

        # 提取验证结果行
        echo ""
        echo "  验证结果："
        grep -E "VERIFICATION RESULT|Failed Checks|SUCCESSFUL|FAILED" "${artifact_dir}/stdout.log" 2>/dev/null | head -5 | sed 's/^/    /'

        echo ""
    fi

    # 分析产物大小
    echo "=== 产物大小分析 ==="
    local total_size
    total_size=$(du -sh "$artifact_dir" 2>/dev/null | cut -f1 || echo "unknown")
    echo "  总大小：${total_size}"

    local out_file
    out_file=$(find "$artifact_dir" -name "*.out" -type f 2>/dev/null | head -1)
    if [ -n "$out_file" ]; then
        local out_size
        out_size=$(ls -lh "$out_file" | awk '{print $5}')
        echo "  Goto 二进制：${out_size}"
    fi

    local symtab_file
    symtab_file=$(find "$artifact_dir" -name "*.symtab.out" -type f 2>/dev/null | head -1)
    if [ -n "$symtab_file" ]; then
        local symtab_size
        symtab_size=$(ls -lh "$symtab_file" | awk '{print $5}')
        echo "  符号表：${symtab_size}"
        # 粗略估算符号数量
        local approx_symbols
        approx_symbols=$(wc -l < "$symtab_file" 2>/dev/null || echo "?")
        echo "  符号数（估算）：约 ${approx_symbols} 行"
    fi

    # 类型映射分析
    local type_map_file
    type_map_file=$(find "$artifact_dir" -name "*.type_map.json" -type f 2>/dev/null | head -1)
    if [ -n "$type_map_file" ]; then
        local type_count
        type_count=$(grep -c '"' "$type_map_file" 2>/dev/null || echo "?")
        echo "  类型映射条目：约 ${type_count} 个"
    fi

    echo ""

    # 状态爆炸诊断
    echo "=== 状态爆炸诊断建议 ==="
    if [ -f "${artifact_dir}/stdout.log" ]; then
        local last_lines
        last_lines=$(tail -30 "${artifact_dir}/stdout.log" 2>/dev/null || true)

        if echo "$last_lines" | grep -q "out of memory\|OOM\|Cannot allocate"; then
            echo "  🔴 内存溢出型状态爆炸"
            echo "     → 症状：CBMC 直接因内存不足退出"
            echo "     → 原因：BTreeSet/BTreeMap 红黑树节点过多，或 unwind 深度过大"
            echo "     → 对策："
            echo "        1. 减少 BTreeSet/BTreeMap 元素数量（当前 proof 的设计）"
            echo "        2. 拆分 proof 为更小的单元"
            echo "        3. 使用 --default-unwind 限制循环展开深度"
            echo "        4. 尝试用 kani::assume 缩小状态空间"
        elif echo "$last_lines" | grep -q "Unwinding loop|unwinding"; then
            echo "  🟡 循环展开型超时"
            echo "     → 症状：卡在循环展开阶段"
            echo "     → 原因：循环迭代次数过多，或递归深度大"
            echo "     → 对策："
            echo "        1. 使用 --default-unwind 设置合理的展开上限"
            echo "        2. 用 kani::assume 限制循环次数"
            echo "        3. 重构 proof 减少循环嵌套"
        else
            echo "  🟠 其他类型超时"
            echo "     → 可能原因：SAT 求解器耗时过长，或命题公式过大"
            echo "     → 对策："
            echo "        1. 尝试不同的 SAT 求解器（--sat-solver cadical / minisat）"
            echo "        2. 减少 proof 中的变量数量"
            echo "        3. 拆分 proof 为更小的验证单元"
        fi
    else
        echo "  ⚪ 无 stdout 日志，无法诊断"
    fi

    echo ""
}

# === 主入口 ===
usage() {
    sed -n '2,40p' "$0" | sed 's/^# \?//'
}

main() {
    local mode="single"
    local harness=""
    local timeout_sec="$DEFAULT_TIMEOUT"
    local output_dir="$DEFAULT_OUTPUT_DIR"
    local analyze_dir=""

    # 解析参数
    while [ $# -gt 0 ]; do
        case "$1" in
            --harness|-H)
                harness="$2"
                shift 2
                ;;
            --all|-a)
                mode="all"
                shift
                ;;
            --list|-l)
                list_proofs
                exit 0
                ;;
            --timeout|-t)
                timeout_sec="$2"
                shift 2
                ;;
            --output-dir|-o)
                output_dir="$2"
                shift 2
                ;;
            --analyze)
                analyze_dir="$2"
                shift 2
                ;;
            --help|-h)
                usage
                exit 0
                ;;
            *)
                log_error "未知参数：$1"
                usage
                exit 1
                ;;
        esac
    done

    # 分析模式
    if [ -n "$analyze_dir" ]; then
        analyze_artifacts "$analyze_dir"
        exit 0
    fi

    # 验证模式
    if [ "$mode" = "single" ] && [ -z "$harness" ]; then
        log_error "请指定 --harness <name> 或使用 --all"
        usage
        exit 1
    fi

    mkdir -p "$output_dir"

    if [ "$mode" = "all" ]; then
        log_info "收集所有 proof 的产物（超时：${timeout_sec}s）"
        echo ""

        # 获取所有 proof 列表
        local proofs
        proofs=$(grep -A1 '#\[kani::proof' "${WORKSPACE_ROOT}/${CRATE}/verification/kani_proofs.rs" \
            | grep -E '^(pub )?fn ' \
            | sed 's/.*fn //; s/().*//' \
            | sort)

        local total=0
        local passed=0
        local failed=0
        local timeout=0

        for proof in $proofs; do
            echo ""
            echo "========================================"
            collect_single_proof "$proof" "$timeout_sec" "$output_dir" || true

            # 统计结果
            local summary_file="${output_dir}/${proof}/summary.txt"
            if [ -f "$summary_file" ]; then
                total=$((total + 1))
                local result
                result=$(grep "^Result:" "$summary_file" | awk '{print $2}')
                case "$result" in
                    PASSED) passed=$((passed + 1)) ;;
                    TIMEOUT) timeout=$((timeout + 1)) ;;
                    FAILED|ERROR) failed=$((failed + 1)) ;;
                esac
            fi
        done

        echo ""
        echo "========================================"
        log_info "全部完成！"
        echo "  总计：${total}"
        echo "  通过：${passed}"
        echo "  超时：${timeout}"
        echo "  失败：${failed}"
        echo ""
        log_info "产物保存在：${output_dir}/"
    else
        collect_single_proof "$harness" "$timeout_sec" "$output_dir"
    fi
}

main "$@"
