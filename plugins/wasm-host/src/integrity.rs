// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 分发完整性：per-module 清单 + blake3 内容哈希 + ed25519 分离签名 + 信任锚 + 验签策略。
//!
//! # 分发物三件套（一期定案）
//! - `{name}.wasm`                —— 模块二进制（服务名 = 文件名去扩展名，既有口径不变）
//! - `{name}.plugin-module.json`  —— 分发清单（身份/版本/ABI/内容哈希/纯度声明）
//! - `{name}.wasm.sig`            —— 分离签名（ed25519 over **清单文件精确字节**）
//!
//! **签名链**：`sig → 清单字节 → blake3 → .wasm 字节`——一份签名锚定两份文件；
//! 任何一环被篡改（清单改字段 / .wasm 换内容）都会在对账或验签处 fail-visible。
//!
//! # 信任模型（双层，插件域扩展不替换）
//! - 治理动作：`trusted-keys.json` 公钥登记/轮换/禁用（运维显式操作，与
//!   plugin.json「运维显式登记即可信」同族——签名把「登记即可信」细化为
//!   「登记公钥 + 验签通过才可信」）；
//! - 工程动作：加载期前置验签（本模块），验签失败按策略处理。
//!
//! # 签名格式（与核心仓审计锚点同族）
//! ed25519（RFC 8032 确定性签名）over 清单文件精确字节。与
//! `evorule-governance::signing::AuditSigner` 的格式级对齐由测试向量锁定
//! （`test_cross_verify_with_governance_vector`：governance 原语签发的
//! (载荷, 公钥, 签名) 固定向量在本模块验签通过）——零跨仓代码依赖。
//!
//! # 策略（`WASM_HOST_SIGN_POLICY`）
//! - `enforce`（默认，fail-closed）：清单+签名必须存在且全量校验通过，否则拒载；
//!   信任锚缺失即拒启动。存量未签名部署须显式降档为 `warn`（降档动作留日志，
//!   可审计），完成签名分发后移除该降档。
//! - `warn`（显式迁移期降档）：清单/签名**缺失** → WARN 放行；
//!   **存在则全量校验**——清单或签名存在但校验失败（哈希不符/验签不过/
//!   信任锚未知/ABI 不符）→ 拒载（主动存在的完整性材料造假=篡改信号，
//!   不因 warn 而放行）。
//! - `off`：完全跳过（等价既有行为），启动日志明示。
//!
//! # 纯度声明对账（零 host function 模型下无可授权能力——原案权限声明让位于纯度对账）
//! 清单 `purity: "zero-import"` 与引擎实测 import 集合交叉：引擎「import 非空
//! 即拒」（`engine::load` 零能力防线①）不变，本模块在拒载消息上融合「清单
//! 纯度声明与实测不符」定性——声明×实载对账范式（declaration.rs 同族）。
//! **这不是权限授权**：零 host function 模型下无可授权能力，纯度对账是把
//! 「模块自称纯计算」升级为「宿主实测验证其自称」。

use serde::{Deserialize, Serialize};

/// 宿主 ABI 版本（三导出契约 memory/alloc/udf 的显式版本号）。
/// 破坏性 ABI 变更时 +1；清单 `abi` 与此不符 = 结构性不兼容，拒载。
pub const HOST_ABI_VERSION: u32 = 1;

/// 本进程版本（兼容性提示用，`engine_min` 比对基线）
pub fn host_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// 验签策略
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignPolicy {
    Warn,
    Enforce,
    Off,
}

impl SignPolicy {
    /// 从环境变量值解析；非法值 fail-visible（不静默回退——拼错策略名等于关掉防线）
    pub fn parse(s: &str) -> Result<Self, String> {
        match s.trim().to_ascii_lowercase().as_str() {
            "warn" => Ok(Self::Warn),
            "enforce" => Ok(Self::Enforce),
            "off" => Ok(Self::Off),
            other => Err(format!(
                "非法签名策略 `{other}`（WASM_HOST_SIGN_POLICY 合法值: warn | enforce | off）"
            )),
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Warn => "warn",
            Self::Enforce => "enforce",
            Self::Off => "off",
        }
    }
}

/// per-module 分发清单（`{name}.plugin-module.json`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModuleManifest {
    /// 模块名 = 服务名 = .wasm 文件名去扩展名（与 registry 口径一致，对账字段）
    pub module: String,
    /// 语义版本（分发管理用展示位，不参与兼容判定——包级 contract_version 才是兼容权威）
    pub version: String,
    /// 宿主 ABI 版本（与 [`HOST_ABI_VERSION`] 比对，不符=结构性不兼容）
    pub abi: u32,
    /// .wasm 内容哈希（`blake3:<64hex>`）
    pub blake3: String,
    /// 纯度声明：唯一合法值 `zero-import`（与引擎 import 实测对账，见模块头）
    pub purity: String,
    /// 最低宿主版本提示（如 "0.2.0"；提示性 WARN，不拒载）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine_min: Option<String>,
    /// 发布说明（自由文本，市场展示用）
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub release_notes: Option<String>,
}

/// 分离签名文件（`{name}.wasm.sig`）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DetachedSignature {
    /// 被签模块名（快速索引；真伪由验签本身保证）
    pub module: String,
    /// 信任锚索引（trusted-keys.json 的 key_id）
    pub key_id: String,
    /// ed25519 签名（128 hex = 64 字节），载荷 = 清单文件精确字节
    pub signature: String,
}

/// 信任锚条目（trusted-keys.json）
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TrustedKey {
    pub key_id: String,
    /// 32 字节压缩公钥（64 hex）
    pub public_key: String,
    #[serde(default)]
    pub note: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

/// 信任锚表（trusted-keys.json 根结构）
#[derive(Debug, Clone, Deserialize)]
pub struct TrustedKeys {
    #[serde(default)]
    pub keys: Vec<TrustedKey>,
}

/// 完整性校验结论（per-module）
///
/// `verified` = 清单+签名存在且全量校验通过；`accepted_unverified` = warn 策略下
/// 材料缺失放行；`off` = 策略关闭未检查。拒载不走本枚举（直接 Err fail-fast）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Integrity {
    Verified { key_id: String },
    AcceptedUnverified { reason: String },
    Unchecked,
}

impl Integrity {
    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Integrity::Verified { key_id } => {
                serde_json::json!({ "state": "verified", "key_id": key_id })
            }
            Integrity::AcceptedUnverified { reason } => {
                serde_json::json!({ "state": "accepted_unverified", "reason": reason })
            }
            Integrity::Unchecked => serde_json::json!({ "state": "unchecked" }),
        }
    }
}

/// blake3 内容哈希（`blake3:<64hex>` 形态，与 evorule-bundle audit.content_hash 同族）
pub fn blake3_hex(data: &[u8]) -> String {
    format!("blake3:{}", hex_encode(&blake3_hash(data)))
}

fn blake3_hash(data: &[u8]) -> [u8; 32] {
    blake3::hash(data).as_bytes().to_owned()
}

/// ed25519 确定性签名（与 governance::signing::AuditSigner 同族：RFC 8032，无 RNG nonce）
pub fn sign(seed: [u8; 32], payload: &[u8]) -> [u8; 64] {
    use ed25519_dalek::{Signer, SigningKey};
    let signing = SigningKey::from_bytes(&seed);
    signing.sign(payload).to_bytes()
}

/// ed25519 验签。`Ok(true)`=有效；`Ok(false)`=不匹配；`Err`=输入非法。
pub fn verify(verifying: [u8; 32], payload: &[u8], sig: &[u8; 64]) -> Result<bool, String> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let key = VerifyingKey::from_bytes(&verifying).map_err(|e| format!("公钥非法: {e}"))?;
    let signature = Signature::from_bytes(sig);
    Ok(key.verify(payload, &signature).is_ok())
}

/// 密钥对生成（一次性运维操作，OS 熵源）
///
/// 返回 `(seed_hex, pk_hex)`；种子需私密保存，公钥可入信任锚。
pub fn generate_keys() -> Result<(String, String), String> {
    let mut seed = [0u8; 32];
    getrandom::fill(&mut seed).map_err(|e| format!("OS 熵源不可用: {e}"))?;
    use ed25519_dalek::SigningKey;
    let signing = SigningKey::from_bytes(&seed);
    Ok((
        hex_encode(&seed),
        hex_encode(&signing.verifying_key().to_bytes()),
    ))
}

/// 十六进制编解码（与 governance::signing 同族小写形态）
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

pub fn hex_decode(hex: &str) -> Result<Vec<u8>, String> {
    if !hex.len().is_multiple_of(2) {
        return Err(format!("hex 长度为奇数: {}", hex.len()));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16);
        let lo = (pair[1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out.push(((h << 4) | l) as u8),
            _ => return Err("hex 含非十六进制字符".to_string()),
        }
    }
    Ok(out)
}

/// 三段语义版本比较（`a >= b` 则 true；解析失败返回 None 由调用方降级为 WARN）
pub fn version_at_least(current: &str, min: &str) -> Option<bool> {
    let parse = |v: &str| -> Option<Vec<u64>> {
        v.split('.')
            .map(|p| p.parse::<u64>().ok())
            .collect::<Option<Vec<_>>>()
    };
    let (c, m) = (parse(current)?, parse(min)?);
    for i in 0..3 {
        let cv = c.get(i).copied().unwrap_or(0);
        let mv = m.get(i).copied().unwrap_or(0);
        if cv != mv {
            return Some(cv > mv);
        }
    }
    Some(true)
}

/// 加载期完整性校验（registry 调用；单模块维度）
///
/// `wasm_bytes` 已读入内存的模块字节；`dir` 为模块目录（找清单/签名/信任锚）。
/// 返回 `Ok(Integrity)` = 放行（含 warn 放行）；`Err` = 拒载（fail-fast）。
pub struct Verifier<'a> {
    pub policy: SignPolicy,
    pub trusted: Option<&'a TrustedKeys>,
}

impl<'a> Verifier<'a> {
    pub fn new(policy: SignPolicy, trusted: Option<&'a TrustedKeys>) -> Self {
        Self { policy, trusted }
    }

    pub fn verify_module(
        &self,
        name: &str,
        wasm_bytes: &[u8],
        dir: &std::path::Path,
    ) -> Result<Integrity, String> {
        if self.policy == SignPolicy::Off {
            return Ok(Integrity::Unchecked);
        }
        let manifest_path = dir.join(format!("{name}.plugin-module.json"));
        let sig_path = dir.join(format!("{name}.wasm.sig"));

        let manifest_bytes = read_opt(&manifest_path)?;
        let sig_bytes = read_opt(&sig_path)?;
        if manifest_bytes.is_none() && sig_bytes.is_none() {
            return match self.policy {
                SignPolicy::Enforce => Err(format!(
                    "模块 `{name}` 无分发清单与签名（enforce 策略拒绝未签名模块）——\
                     用 `wasm-host sign` 生成分发三件套，或将 WASM_HOST_SIGN_POLICY 降为 warn/off"
                )),
                _ => Ok(Integrity::AcceptedUnverified {
                    reason: "no manifest/signature (warn policy)".to_string(),
                }),
            };
        }

        // 材料存在（任一）即全量校验：篡改信号不因 warn 放行
        let manifest_bytes = manifest_bytes.ok_or_else(|| {
            format!("模块 `{name}` 有签名无清单——分发三件套不完整，拒载（请用 sign 重新生成）")
        })?;
        let manifest: ModuleManifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|e| format!("模块 `{name}` 清单解析失败: {e}"))?;

        // ① 身份对账（清单.module × 文件名）
        if manifest.module != name {
            return Err(format!(
                "清单身份不符: module=`{}` 文件名=`{name}`（服务名口径以文件名为准）",
                manifest.module
            ));
        }
        // ② 内容哈希（清单 blake3 × .wasm 实测）——防模块被换
        let actual = blake3_hex(wasm_bytes);
        if manifest.blake3 != actual {
            return Err(format!(
                "模块 `{name}` 内容哈希不符: 清单={} 实测={actual}（.wasm 与清单不同源/被篡改）",
                manifest.blake3
            ));
        }
        // ③ 纯度声明合法性
        if manifest.purity != "zero-import" {
            return Err(format!(
                "模块 `{name}` 纯度声明非法: `{}`（唯一合法值 zero-import；零能力沙箱只接受纯计算 UDF）",
                manifest.purity
            ));
        }
        // ④ ABI 结构兼容
        if manifest.abi != HOST_ABI_VERSION {
            return Err(format!(
                "模块 `{name}` ABI 版本不兼容: 清单={} 宿主={HOST_ABI_VERSION}",
                manifest.abi
            ));
        }
        // ⑤ engine_min 提示性检查（不拒载）
        if let Some(min) = &manifest.engine_min {
            match version_at_least(host_version(), min) {
                Some(false) => tracing::warn!(
                    "模块 `{name}` 要求宿主 >= {min}，当前 {}/集成可能不完整",
                    host_version()
                ),
                None => tracing::warn!(
                    "模块 `{name}` engine_min=`{min}` 无法解析（提示性字段，跳过比对）"
                ),
                Some(true) => {}
            }
        }

        // ⑥ 签名（清单文件精确字节）
        let sig_bytes = sig_bytes
            .ok_or_else(|| format!("模块 `{name}` 有清单无签名（分发三件套不完整，拒载）"))?;
        let detached: DetachedSignature = serde_json::from_slice(&sig_bytes)
            .map_err(|e| format!("模块 `{name}` 签名文件解析失败: {e}"))?;
        if detached.module != name {
            return Err(format!(
                "签名归属不符: sig.module=`{}` 文件=`{name}`",
                detached.module
            ));
        }
        let trusted = self.trusted.ok_or_else(|| {
            format!(
                "模块 `{name}` 有签名但信任锚不可用（trusted-keys.json 缺失/解析失败）\
                 —— enforce/warn 策略下已存在的签名必须可验证"
            )
        })?;
        let key = trusted
            .keys
            .iter()
            .find(|k| k.key_id == detached.key_id && k.enabled)
            .ok_or_else(|| {
                format!(
                    "模块 `{name}` 签名 key_id=`{}` 不在信任锚或已禁用（拒绝未知来源签名）",
                    detached.key_id
                )
            })?;
        let pk_bytes = hex_decode(&key.public_key)
            .map_err(|e| format!("信任锚公钥 `{}` 非法: {e}", key.key_id))?;
        let pk: [u8; 32] = pk_bytes
            .try_into()
            .map_err(|_| format!("信任锚公钥 `{}` 长度 != 32 字节", key.key_id))?;
        let sig_arr: [u8; 64] = hex_decode(&detached.signature)
            .map_err(|e| format!("模块 `{name}` 签名 hex 非法: {e}"))?
            .try_into()
            .map_err(|_| format!("模块 `{name}` 签名长度 != 64 字节"))?;
        let ok = verify(pk, &manifest_bytes, &sig_arr)
            .map_err(|e| format!("模块 `{name}` 验签输入非法: {e}"))?;
        if !ok {
            return Err(format!(
                "模块 `{name}` 验签失败（清单被篡改或签名非 `{}` 私钥所签）",
                key.key_id
            ));
        }
        Ok(Integrity::Verified {
            key_id: detached.key_id,
        })
    }
}

fn read_opt(path: &std::path::Path) -> Result<Option<Vec<u8>>, String> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("读取 {} 失败: {e}", path.display())),
    }
}

/// 生成清单骨架（sign CLI 用）：module/version/abi/blake3/purity，其余留空
pub fn build_manifest(module: &str, version: &str, wasm_bytes: &[u8]) -> ModuleManifest {
    ModuleManifest {
        module: module.to_string(),
        version: version.to_string(),
        abi: HOST_ABI_VERSION,
        blake3: blake3_hex(wasm_bytes),
        purity: "zero-import".to_string(),
        engine_min: None,
        release_notes: None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    // 跨实现互验固定向量：由 evorule-governance::signing::AuditSigner 实际签发
    // （种子 [7u8;32]，载荷 b"cross-verify-vector-payload"；RFC 8032 确定性 → 向量恒真）。
    // 写死在本仓锁定「wasm-host 验签实现 × governance 签名原语」格式兼容，零跨仓依赖。
    const GOVERNANCE_SEED: [u8; 32] = [7u8; 32];
    const GOVERNANCE_PAYLOAD: &[u8] = b"cross-verify-vector-payload";
    const GOVERNANCE_PK_HEX: &str =
        "ea4a6c63e29c520abef5507b132ec5f9954776aebebe7b92421eea691446d22c";
    const GOVERNANCE_SIG_HEX: &str =
        "fc5eae11e851e9eefe061901264f4becb3bc11269f177764167ff12eec38bba8554d4aa2d060ffccbc3ea3cce9ace5ea06bdee7e25904425d658ff0641573503";

    #[test]
    fn test_cross_verify_with_governance_vector() {
        // ① governance 公钥 × governance 签名 × 本模块 verify → 有效
        let pk: [u8; 32] = hex_decode(GOVERNANCE_PK_HEX).unwrap().try_into().unwrap();
        let sig: [u8; 64] = hex_decode(GOVERNANCE_SIG_HEX).unwrap().try_into().unwrap();
        assert!(verify(pk, GOVERNANCE_PAYLOAD, &sig).unwrap());
        // ② 本模块 sign（同种子同载荷）复算 == governance 签名（确定性同族锁定）
        assert_eq!(sign(GOVERNANCE_SEED, GOVERNANCE_PAYLOAD), sig);
        // ③ 篡改即失效
        assert!(!verify(pk, b"tampered", &sig).unwrap());
    }

    #[test]
    fn test_sign_deterministic() {
        let a = sign(GOVERNANCE_SEED, GOVERNANCE_PAYLOAD);
        let b = sign(GOVERNANCE_SEED, GOVERNANCE_PAYLOAD);
        assert_eq!(a, b);
    }

    #[test]
    fn test_verify_ok_and_tamper_rejected() {
        let pk = {
            use ed25519_dalek::SigningKey;
            SigningKey::from_bytes(&GOVERNANCE_SEED)
                .verifying_key()
                .to_bytes()
        };
        let sig = sign(GOVERNANCE_SEED, GOVERNANCE_PAYLOAD);
        assert!(verify(pk, GOVERNANCE_PAYLOAD, &sig).unwrap());
        assert!(!verify(pk, b"tampered", &sig).unwrap());
        let mut bad = sig;
        bad[0] ^= 0xff;
        assert!(!verify(pk, GOVERNANCE_PAYLOAD, &bad).unwrap());
        let other_pk = {
            use ed25519_dalek::SigningKey;
            SigningKey::from_bytes(&[8u8; 32])
                .verifying_key()
                .to_bytes()
        };
        assert!(!verify(other_pk, GOVERNANCE_PAYLOAD, &sig).unwrap());
    }

    #[test]
    fn test_blake3_stable() {
        let h1 = blake3_hex(b"hello");
        let h2 = blake3_hex(b"hello");
        assert_eq!(h1, h2);
        assert!(h1.starts_with("blake3:"));
        assert_eq!(h1.len(), "blake3:".len() + 64);
        assert_ne!(blake3_hex(b"hello"), blake3_hex(b"hellp"));
    }

    #[test]
    fn test_policy_parse_strict() {
        assert_eq!(SignPolicy::parse("warn").unwrap(), SignPolicy::Warn);
        assert_eq!(SignPolicy::parse("ENFORCE").unwrap(), SignPolicy::Enforce);
        assert_eq!(SignPolicy::parse(" off ").unwrap(), SignPolicy::Off);
        assert!(SignPolicy::parse("yes").is_err());
    }

    #[test]
    fn test_version_compare() {
        assert_eq!(version_at_least("0.2.0", "0.1.9"), Some(true));
        assert_eq!(version_at_least("0.1.0", "0.2.0"), Some(false));
        assert_eq!(version_at_least("1.0.0", "1.0.0"), Some(true));
        assert_eq!(version_at_least("0.1", "0.1.0"), Some(true));
        assert_eq!(version_at_least("abc", "0.1.0"), None);
    }

    #[test]
    fn test_hex_roundtrip() {
        let bytes = [0u8, 1, 0xff, 0x10, 0xab, 0xcd];
        let hex = hex_encode(&bytes);
        assert_eq!(hex_decode(&hex).unwrap(), bytes);
        assert!(hex_decode("zz").is_err());
        assert!(hex_decode("abc").is_err());
    }

    #[test]
    fn test_manifest_roundtrip() {
        let wasm = b"fake-wasm-bytes";
        let m = build_manifest("demo", "1.0.0", wasm);
        let json = serde_json::to_vec(&m).unwrap();
        let back: ModuleManifest = serde_json::from_slice(&json).unwrap();
        assert_eq!(m, back);
        assert_eq!(back.blake3, blake3_hex(wasm));
        assert_eq!(back.abi, HOST_ABI_VERSION);
    }
}
