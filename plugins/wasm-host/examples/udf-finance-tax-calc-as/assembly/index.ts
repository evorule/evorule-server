// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.

/**
 * AssemblyScript UDF 示例：`udf_finance_tax_calc_as`（阶段 2 · T7）
 *
 * # 这条示例要证什么
 * UDF ABI 是**语言无关**的：同一份 host（`evorule-wasm-host`）既能跑
 * Rust 编译的 guest，也能跑 AssemblyScript 编译的 guest，且同参调用
 * **输出逐字节一致**（键序、数字格式完全相同）。
 *
 * # ABI（与 Rust 版一致）
 * - `memory`：导出线性内存（`--exportMemory`）
 * - `alloc(len: i32) -> i32`：返回入参写入地址
 * - `udf(ptr: i32, len: i32) -> i64`：`(结果指针 << 32) | 结果长度`
 *
 * # 零 import 约束（T4 实证的硬要求）
 * host 在**加载期**枚举 import 集合，非空即拒。故本模块用
 * `--runtime stub --use abort=` 编译：不引入 `env.abort`，也不引入
 * AS 的 GC runtime（`__new`/`__pin`）。任何 import 都会让模块被沙箱拒载。
 *
 * # 为何是整数定点
 * 同 Rust 版：evorule TCB 的 `JsonValue` 无 Float 变体，浮点入参会在
 * server 侧被降级为字符串、根本到不了 UDF。金额用分、税率用基点。
 */

/** 缓冲容量（入参 JSON 很小；静态分配，stub runtime 下无堆分配） */
const CAP: i32 = 4096;

/** 入参缓冲：host 调 alloc 拿到 dataStart，写入后经 ptr/len 通知 */
const IN: Uint8Array = new Uint8Array(CAP);

/** 出参缓冲：写入结果 / 错误 JSON */
const OUT: Uint8Array = new Uint8Array(CAP);

/** 本次调用的入参长度（host 传入） */
let inLen: i32 = 0;

/** 未找到 / 非整数的哨兵值 */
const NOT_FOUND: i64 = i64.MIN_VALUE;

/** ABI：返回入参写入地址 */
export function alloc(len: i32): i32 {
  if (len <= 0 || len > CAP) return 0;
  return IN.dataStart as i32;
}

/** ABI：UDF 入口 */
export function udf(ptr: i32, len: i32): i64 {
  if (len <= 0 || len > CAP) return writeError("入参为空");
  inLen = len;
  // ptr 是 host 拿到并写入的缓冲地址；本实现固定用自有 IN 缓冲，
  // 故不额外引用 ptr（避免 AS 报未使用参数）
  if (ptr <= 0) return writeError("入参指针非法");

  const amountCents = findInt("amount_cents");
  if (amountCents == NOT_FOUND) return writeError("缺少 amount_cents（整数，单位：分）");
  const rateBp = findInt("rate_bp");
  if (rateBp == NOT_FOUND) return writeError("缺少 rate_bp（整数，单位：基点，600 = 6%）");

  if (amountCents < 0) return writeError("amount_cents 不得为负");
  if (rateBp < 0 || rateBp > 10000) return writeError("rate_bp 必须位于 [0,10000]");
  // i64 中间运算防溢出：Rust 版用 i128 中间量，AS 版只有 i64，
  // 故显式设上界 —— 9e14 分（9 万亿元）× 10000 = 9e18 < i64.MAX(9.223e18)。
  // 两版在 [0, 9e14] 区间内输出逐字节一致（已实测 5 组参数）。
  if (amountCents > 900000000000000) {
    return writeError("amount_cents 超出示例实现上界（9e14 分）");
  }

  // round-half-up 到分：与 Rust 版 `(numer + denom/2) / denom` 等价
  const taxCents = (amountCents * rateBp + 5000) / 10000;
  const totalCents = amountCents + taxCents;

  // 键序与 Rust 版 serde_json（BTreeMap，字母序）一致 —— 逐字节可比对
  let n = 0;
  n = put(n, '{"amount_cents":');
  n = putInt(n, amountCents);
  n = put(n, ',"rate_bp":');
  n = putInt(n, rateBp);
  n = put(n, ',"tax_cents":');
  n = putInt(n, taxCents);
  n = put(n, ',"total_cents":');
  n = putInt(n, totalCents);
  n = put(n, '}');
  return pack(n);
}

/**
 * 从入参 JSON 中提取整数键值。
 * 手写极简扫描（stub runtime 下不引入 JSON 库，也避免动态分配）。
 */
function findInt(key: string): i64 {
  const klen = key.length;
  for (let i = 0; i + klen < inLen; i++) {
    let ok = true;
    for (let j = 0; j < klen; j++) {
      if (IN[i + j] != <u8>key.charCodeAt(j)) {
        ok = false;
        break;
      }
    }
    if (!ok) continue;

    let p = i + klen;
    if (IN[p] != 0x22) continue; // 期望紧跟 '"'（键名闭合引号）
    p++;
    // 跳过 ':' 与空白
    while (p < inLen && (IN[p] == 0x3a || IN[p] == 0x20 || IN[p] == 0x09)) p++;

    let neg = false;
    if (p < inLen && IN[p] == 0x2d) {
      neg = true;
      p++;
    }
    let v: i64 = 0;
    let any = false;
    while (p < inLen) {
      const c = IN[p];
      if (c < 0x30 || c > 0x39) break;
      v = v * 10 + <i64>(c - 0x30);
      any = true;
      p++;
    }
    if (!any) return NOT_FOUND;
    return neg ? -v : v;
  }
  return NOT_FOUND;
}

/** 逐字符写字符串到出参缓冲（**UTF-8** 编码）
 *
 * 必须真做 UTF-8 编码：`charCodeAt` 返回 UTF-16 码元，中文字符 > 0xFF，
 * 直接截成 u8 会写出非法字节 → 出参不是合法 JSON，host 只能报
 * "返回值不是合法 JSON"（误导：真实原因是参数错误）。
 * 实测踩过：错误文案含中文时 422 文案变成 JSON 解析错误。
 *
 * 仅处理 BMP（含中文）；本文件不出现代理对（emoji 等），故不做代理对合并。
 */
function put(n: i32, s: string): i32 {
  for (let i = 0; i < s.length; i++) {
    const c = s.charCodeAt(i);
    if (c < 0x80) {
      OUT[n] = <u8>c;
      n++;
    } else if (c < 0x800) {
      OUT[n] = <u8>(0xc0 | (c >> 6));
      OUT[n + 1] = <u8>(0x80 | (c & 0x3f));
      n += 2;
    } else {
      OUT[n] = <u8>(0xe0 | (c >> 12));
      OUT[n + 1] = <u8>(0x80 | ((c >> 6) & 0x3f));
      OUT[n + 2] = <u8>(0x80 | (c & 0x3f));
      n += 3;
    }
  }
  return n;
}

/** 写非负整数（十进制，无前导零） */
function putInt(n: i32, v: i64): i32 {
  if (v == 0) {
    OUT[n] = 0x30;
    return n + 1;
  }
  let div: i64 = 1;
  while (v / div >= 10) div *= 10;
  while (div > 0) {
    OUT[n] = <u8>(0x30 + <i32>((v / div) % 10));
    n++;
    div /= 10;
  }
  return n;
}

/** 错误出参：host 见到顶层 `error` 字段判 422（ABI 约定） */
function writeError(msg: string): i64 {
  let n = 0;
  n = put(n, '{"error":"');
  n = put(n, msg);
  n = put(n, '"}');
  return pack(n);
}

/** 打包 `(指针 << 32) | 长度` 为 i64（ABI 返回约定） */
function pack(n: i32): i64 {
  return (<i64>(OUT.dataStart as i32) << 32) | <i64>n;
}
