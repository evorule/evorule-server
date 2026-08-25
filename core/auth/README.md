<!--
  Copyright 2026 EvoRule Project
  SPDX-License-Identifier: AGPL-3.0-or-later
-->

# evorule-auth

**Bearer Token 认证 —— 速率限制 + 恒定时间比较 + 空 token 过滤**

> **crate 类型**: 内部 lib（`publish = false`）
> **引入版本**: v0.1.0

---

## 定位

提供 HTTP Bearer token 认证中间件，用于 evorule-server 的 API 端点保护。

## 公开类型

- `AuthService` — 认证服务（token 验证/添加/移除/列表/生成）
- `TokenInfo` — token 信息（创建时间、最后使用时间、使用次数）
- `TokenInfoMasked` — 脱敏后的 token 信息（用于 API 返回）
- `AuthResponse` — 认证响应（成功/失败）

## 安全特性

- **恒定时间比较**（N1）：使用 `subtle::ConstantTimeEq` 防止时序攻击
- **空 token 过滤**（N1）：`AuthConfig::new()` 过滤空字符串 token，防止 `ct_eq("", "")` 返回 true 的空 token 通过认证
- **速率限制**：可配置的请求速率限制，防止暴力破解
- **token 生成**：使用 `rand` crate 生成加密安全的随机 token

## 相关文档

- [PITFALLS.md](../../docs/PITFALLS.md) — 认证相关踩坑
- [GATE_REFERENCE.md](../../GATE_REFERENCE.md) — 门控参考
