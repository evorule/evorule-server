//! G-A2 审计锚点链部署面装配（server 启动注入）
//!
//! 职责边界（对齐设计决策三原则）：
//! - **TCB 零污染**：签名密钥只存在于本部署面（server 进程内存），
//!   evorule-tcb / reactor 不感知密钥；
//! - **默认关**：未配置环境变量时返回 None，行为与旧版逐位一致（J-T6）；
//! - **密钥不落审计资产**：seed 只经环境变量注入，不写 WAL/日志/审计条目。
//!
//! 环境变量：
//! - `EVORULE_ANCHOR_SEED`：ed25519 私钥种子（64 位 hex，32 字节）。必填项，
//!   未配置时锚点链关闭。
//! - `EVORULE_ANCHOR_KEY_ID`：密钥标识（默认 "anchor-key-1"）。
//! - `EVORULE_ANCHOR_ENGINE_ID`：引擎/部署标识（默认含 server 版本）。
//! - `EVORULE_ANCHOR_INTERVAL`：自动封条间隔，每 N 条新事实封一个锚点
//!   （默认 64；0 = 仅手动 seal）。
//!
//! 生产密钥管理（HSM/门限签名）属 T3 领域，本模块只提供最小部署面。

use std::collections::BTreeMap;

use evorule_governance::SessionAnchorConfig;

/// 从环境变量解析锚点配置；未配置 seed 时返回 None（锚点关）
pub fn anchor_config_from_env() -> Option<SessionAnchorConfig> {
    let seed_hex = std::env::var("EVORULE_ANCHOR_SEED").ok()?;
    let seed = decode_seed(&seed_hex)?;

    let key_id = std::env::var("EVORULE_ANCHOR_KEY_ID")
        .unwrap_or_else(|_| "anchor-key-1".to_string());
    let engine_id = std::env::var("EVORULE_ANCHOR_ENGINE_ID").unwrap_or_else(|_| {
        format!("evorule-server/{}", env!("CARGO_PKG_VERSION"))
    });
    let interval = std::env::var("EVORULE_ANCHOR_INTERVAL")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(64);

    tracing::info!(
        key_id = %key_id,
        engine_id = %engine_id,
        interval,
        "审计锚点链已启用（G-A2）：后续会话将封 ed25519 签名锚点"
    );

    Some(SessionAnchorConfig {
        signer_seed: seed,
        key_id,
        engine_id,
        interval,
    })
}

/// 解析 64 位 hex 种子（严格 32 字节；解析失败=启动失败优于静默降级）
fn decode_seed(hex: &str) -> Option<[u8; 32]> {
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        tracing::error!(
            len = hex.len(),
            "EVORULE_ANCHOR_SEED 格式非法（须 64 位 hex/32 字节），锚点链不启用"
        );
        return None;
    }
    let mut seed = [0u8; 32];
    for (i, chunk) in hex.as_bytes().chunks(2).enumerate() {
        let hi = (chunk[0] as char).to_digit(16)?;
        let lo = (chunk[1] as char).to_digit(16)?;
        seed[i] = ((hi << 4) | lo) as u8;
    }
    Some(seed)
}

/// 部署面公钥表（key_id → hex 公钥），供锚点验证默认密钥集
///
/// 从与 `anchor_config_from_env` 同源的环境变量派生（同 seed → 同公钥）。
/// 未配置 seed 时返回 None（调用方退回空表=只验结构不验签）。
pub fn deployment_public_keys() -> Option<BTreeMap<String, String>> {
    let seed_hex = std::env::var("EVORULE_ANCHOR_SEED").ok()?;
    let seed = decode_seed(&seed_hex)?;
    let key_id = std::env::var("EVORULE_ANCHOR_KEY_ID")
        .unwrap_or_else(|_| "anchor-key-1".to_string());
    let signer = evorule_governance::signing::AuditSigner::from_bytes(seed);
    let mut keys = BTreeMap::new();
    keys.insert(key_id, signer.verifying_hex());
    Some(keys)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_decode_accepts_valid_hex() {
        let s = decode_seed(&"ab".repeat(32)).unwrap();
        assert_eq!(s[0], 0xab);
        assert_eq!(s[31], 0xab);
    }

    #[test]
    fn seed_decode_rejects_bad_input() {
        assert!(decode_seed("short").is_none());
        assert!(decode_seed(&"zz".repeat(32)).is_none());
        assert!(decode_seed(&"ab".repeat(31)).is_none());
    }
}
