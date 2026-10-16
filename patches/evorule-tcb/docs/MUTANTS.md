<!--
  Copyright 2026 EvoRule Project

  SPDX-License-Identifier: AGPL-3.0-or-later

  This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
-->

# Mutation Testing 指南 (cargo-mutants)

[evorule-tcb](../) 的测试**真的能抓 bug**吗？mutation testing 给答案。

## 🎯 原理

cargo-mutants 自动在源代码中注入微小改动 ("mutant")：

- `>=` → `>`
- `+` → `-`
- `true` → `false`
- 删一行、删一个分支、改一个常量...

每个 mutant 都跑完整测试套件：

- **CAUGHT** (抓到): 测试失败 → 测试有效 ✓
- **MISSED** (漏过): 测试通过 → 测试有盲区 ⚠
- **TIMEOUT**: mutant 死循环/超时

**Mutation score** = caught / total。分数越低，测试越"摆设"。

## 📋 现状

- evorule-tcb 测试数: 227 (lib unit + integration + panic_free + proptest + tcb_error_variants + doctest)
- 估算 mutants: ~1500 (基于 5636 lines src / ~4 lines per mutant)
- 实测耗时: 24h+ (单 mutant ~30s - 2min, 含编译 + 227 tests)
- 建议起步: 先 `--quick` 跑 30min 采样建立 baseline

## 🛠️ 安装

```bash
cargo install --locked cargo-mutants
cargo mutants --version
```

## 🚀 运行

### Baseline (30min 采样)

```bash
cd evorule-tcb
./../scripts/run-mutants.sh --quick
# 或直接:
timeout 30m cargo mutants --timeout 60 --output mutants.out/
```

### 全部 (24h+)

```bash
cd evorule-tcb
cargo mutants --timeout 120 --output mutants.out/
```

### 单文件 (快速验证)

```bash
./../scripts/run-mutants.sh --file src/error.rs  # ~50 mutants, 1-2h
./../scripts/run-mutants.sh --file src/domain.rs # ~150 mutants, 3-6h
./../scripts/run-mutants.sh --file src/path.rs   # ~200 mutants, 4-8h
```

### 使用 wrapper

```bash
# 从项目根
./scripts/run-mutants.sh --help     # 显示所有选项
./scripts/run-mutants.sh --install  # 装 cargo-mutants
./scripts/run-mutants.sh --list     # 列出所有 mutants 不跑
./scripts/run-mutants.sh --quick    # 30min baseline
./scripts/run-mutants.sh --file src/executor.rs
./scripts/run-mutants.sh            # 跑全部
```

## 📊 解读结果

`mutants.out/mutants.json` 含完整数据。HTML 报告 (`mutants.out/mutants.html`) 直接打开：

```bash
# Linux
xdg-open evorule-tcb/mutants.out/mutants.html
# macOS
open evorule-tcb/mutants.out/mutants.html
# Windows
start evorule-tcb/mutants.out/mutants.html
```

### Mutation Score 标准

| 分数 | 评估 | 行动 |
|---|---|---|
| > 80% | 优秀 | 维护即可 |
| 60-80% | 良好 | 关注 MISSED mutants |
| 40-60% | 中等 | **优先补 MISSED mutants 的测试** |
| < 40% | 薄弱 | 重构测试 |

## 🔧 处理 MISSED mutants

每个 MISSED mutant 是测试盲区。处理步骤：

1. **理解 mutant**: mutants.html 显示源码差异
2. **判定**:
   - 等价 mutant (mutant 行为不变): 接受，标 EQUIVALENT
   - 真盲区: 加测试
3. **加测试**:

   ```rust
   #[test]
   fn regression_for_<function>_<mutation>() {
       // 构造触发特定分支的输入
       // 验证期望行为
   }
   ```

4. **重跑**: 验证 mutant 现在被 CAUGHT

## 📊 CI

`.github/workflows/mutants.yml`:

- **Schedule**: 每周日 00:00 UTC (夜间跑)
- **Manual**: workflow_dispatch
- **Timeout**: 12h/job (GitHub Actions max)
- **Output**: mutants.out/ artifact + 摘要

CI 不阻断 PR，仅生成报告。merge 决策权在 USER。

## 📖 延伸阅读

- [cargo-mutants 文档](https://github.com/sourcefrog/cargo-mutants)
- [Mutation Testing 原理](https://en.wikipedia.org/wiki/Mutation_testing)
- [Kani 形式化验证](./KANI.md) (互补技术: 形式化证明 vs 启发式 mutant)
