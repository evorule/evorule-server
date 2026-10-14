// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 审计锚点签名（G-A1：真实性 / 防抵赖）
//!
//! # 背景
//!
//! 审计③发现：[`crate::hash`] 的 BLAKE3 哈希链是无密钥的，只能证明"完整性/被动篡改"，
//! **不能证明"真实性/防抵赖"**（恶意者改写 WAL 事实 + 哈希字段可重建一条"合法"链）。
//! G-A1 在此之上引入**密钥签名锚点**：定期对审计链尾签名，用公钥即可验证审计链确由私钥持有者生成。
//!
//! # 为何用 ed25519
//!
//! - ed25519（RFC 8032）签名是**确定性**的：同一私钥 + 同一消息 → 同一签名（nonce 由消息派生，无 RNG）。
//!   这与 evorule 的**确定性执行**纪律完全兼容——审计锚点签名不引入不确定性。
//! - 非对称：私钥签名、公钥验证；私钥可只在审计器端持有，公钥可随导出物分发供第三方验证。
//!
//! # 私钥供给
//!
//! 遵循"凭据不进审计资产/不进规则库"纪律：本模块不落盘任何密钥于审计资产，
//! 由调用方（文件路径 / 执行侧 secret 管理）注入 32 字节种子。
//! 未配置签名器时审计行为与旧版完全一致（可选的真实性锚定）。

use ed25519_dalek::{Signature, Signer, Verifier};

/// 签名相关错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SignError {
    /// 签名/验证输入非法（种子长度、验证密钥字节、签名长度、hex 解析）
    Invalid(String),
    /// 生成随机种子失败（OS 熵源不可用）
    Randomness(&'static str),
}

impl core::fmt::Display for SignError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SignError::Invalid(msg) => write!(f, "审计签名: 非法输入: {}", msg),
            SignError::Randomness(src) => write!(f, "审计签名: 随机种子失败: {}", src),
        }
    }
}

impl std::error::Error for SignError {}

/// ed25519 审计签名器（持有私钥）
///
/// 用私钥对审计链尾（锚点载荷）做**确定性签名**；公钥可导出供验证方使用。
/// 本类型只有 `sign` 能力，不含公钥信任管理（验证方自持公钥）。
#[derive(Debug, Clone)]
pub struct AuditSigner {
    signing: ed25519_dalek::SigningKey,
    verifying: ed25519_dalek::VerifyingKey,
}

impl AuditSigner {
    /// 从 32 字节私钥种子构造签名器
    ///
    /// # 确定性
    ///
    /// 只要种子相同，`sign` 对同一条消息的输出**完全相同**（RFC 8032 确定性 nonce）。
    pub fn from_bytes(seed: [u8; 32]) -> Self {
        let signing = ed25519_dalek::SigningKey::from_bytes(&seed);
        let verifying = signing.verifying_key();
        Self { signing, verifying }
    }

    /// 生成一对新密钥（一次性运维操作）
    ///
    /// 返回 `(sk_seed_hex, pk_hex)`：`sk_seed_hex` 为 32 字节私钥种子的十六进制（需私密保存），
    /// `pk_hex` 为 32 字节压缩公钥（可公开分发供验证）。
    pub fn generate_keys() -> Result<(String, String), SignError> {
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).map_err(|_| SignError::Randomness("OS 熵源不可用"))?;
        let signer = Self::from_bytes(seed);
        Ok((hex_encode(&seed), hex_encode(&signer.verifying_bytes())))
    }

    /// 从 64 位 hex 私钥种子字符串构造签名器
    pub fn from_hex(seed_hex: &str) -> Result<Self, SignError> {
        let bytes = hex_decode(seed_hex)?;
        if bytes.len() != 32 {
            return Err(SignError::Invalid(format!(
                "私钥种子长度 != 32: {}",
                bytes.len()
            )));
        }
        // len 已校验 == 32，无 panic-prone 拷贝（规避 F11-expect 门禁）
        let mut seed = [0u8; 32];
        for (dst, src) in seed.iter_mut().zip(bytes.iter()) {
            *dst = *src;
        }
        Ok(Self::from_bytes(seed))
    }

    /// 32 字节压缩公钥（供验证方使用）
    pub fn verifying_bytes(&self) -> [u8; 32] {
        self.verifying.to_bytes()
    }

    /// 公钥的 64 位 hex 字符串
    pub fn verifying_hex(&self) -> String {
        hex_encode(&self.verifying.to_bytes())
    }

    /// 对载荷做确定性签名，返回 64 字节签名
    ///
    /// 同一实例 + 同一载荷 bytes → 同一签名（确定性）。
    pub fn signature_bytes(&self, payload: &[u8]) -> [u8; 64] {
        // ed25519-dalek 的 Signer::sign 使用 RFC 8032 确定性 nonce，无 RNG，输出确定
        let sig: Signature = self.signing.sign(payload);
        sig.to_bytes()
    }
}

/// 用公钥验证签名（供验证方 / 导入校验使用）
///
/// `verifying_bytes`：32 字节压缩公钥；`payload`：被签名的载荷；`sig_bytes`：64 字节签名。
/// 返回 `Ok(true)` 表示签名有效且完整；`Ok(false)` 表示签名不匹配；`Err` 表示输入非法。
pub fn verify_signature(
    verifying_bytes: [u8; 32],
    payload: &[u8],
    sig_bytes: &[u8; 64],
) -> Result<bool, SignError> {
    let verifying = ed25519_dalek::VerifyingKey::from_bytes(&verifying_bytes)
        .map_err(|e| SignError::Invalid(format!("公钥非法: {e}")))?;
    let sig = Signature::from_bytes(sig_bytes);
    Ok(verifying.verify(payload, &sig).is_ok())
}

/// 十六进制编码
pub fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

/// 十六进制解码
pub fn hex_decode(hex: &str) -> Result<Vec<u8>, SignError> {
    if hex.len() % 2 != 0 {
        return Err(SignError::Invalid(format!("hex 长度为奇数: {}", hex.len())));
    }
    let mut out = Vec::with_capacity(hex.len() / 2);
    let bytes = hex.as_bytes();
    for i in (0..bytes.len()).step_by(2) {
        let hi = (bytes[i] as char).to_digit(16);
        let lo = (bytes[i + 1] as char).to_digit(16);
        match (hi, lo) {
            (Some(h), Some(l)) => out.push(((h << 4) | l) as u8),
            _ => return Err(SignError::Invalid("hex 含非十六进制字符".into())),
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn test_signature_is_deterministic() {
        let signer = AuditSigner::from_bytes([7u8; 32]);
        let payload = b"audit-anchor-payload";
        assert_eq!(
            signer.signature_bytes(payload),
            signer.signature_bytes(payload)
        );
    }

    #[test]
    fn test_sign_and_verify_ok() {
        let signer = AuditSigner::from_bytes([7u8; 32]);
        let pk = signer.verifying_bytes();
        let payload = b"hello-audit";
        let sig = signer.signature_bytes(payload);
        assert!(verify_signature(pk, payload, &sig).unwrap());
    }

    #[test]
    fn test_verify_rejects_tampered_payload_and_key() {
        let signer = AuditSigner::from_bytes([7u8; 32]);
        let pk = signer.verifying_bytes();
        let payload = b"hello-audit";
        let sig = signer.signature_bytes(payload);

        // 篡改载荷
        assert!(!verify_signature(pk, b"hello-audiT", &sig).unwrap());
        // 篡改签名
        let mut bad_sig = sig;
        bad_sig[0] ^= 0xff;
        assert!(!verify_signature(pk, payload, &bad_sig).unwrap());
        // 错误公钥
        let other = AuditSigner::from_bytes([8u8; 32]);
        assert!(!verify_signature(other.verifying_bytes(), payload, &sig).unwrap());
    }

    #[test]
    fn test_hex_roundtrip() {
        let bytes = [0u8, 1, 0xff, 0x10, 0xab, 0xcd];
        let hex = hex_encode(&bytes);
        assert_eq!(hex_decode(&hex).unwrap(), bytes);
        assert!(hex_decode("zz").is_err());
        assert!(hex_decode("abc").is_err());
    }
}
