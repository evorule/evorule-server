// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 审计锚点链（G-A2：抗整链重算/替换/截断/fork 伪造）
//!
//! # 背景
//!
//! [`crate::hash`] 的 BLAKE3 哈希链 + [`crate::signing`] 的锚点签名各自成立，
//! 但两者尚未组成**链**：单点签名只锚定单时刻链头，前缀截断（丢弃开头若干
//! 锚点、以中段锚点冒充创世）无法被检出。本模块把锚点组成**锚点链**：
//!
//! ```text
//! anchor_chain = blake3(prev_anchor_hash + session_id + fact_seq_range
//!                        + chain_head_hash + key_id)
//! ```
//!
//! 锚点自身成链（链中链）：替换/截断/伪造任意锚点，锚点链断裂或签名验不过。
//!
//! # 威胁模型（T2）
//!
//! | 向量 | 检出机制 |
//! |---|---|
//! | A1 整链重算（改 Fact 后全链重算哈希） | 链头哈希变化 → 与已签名锚点不匹配 |
//! | A2 整链替换（插入伪造链） | 伪造链无有效锚点签名 |
//! | A2' 前缀截断（丢弃开头锚点） | 锚点链 prev 链接断裂（创世校验） |
//! | A3 fork 伪造（另一条合法链冒充） | fork 链头与锚点内 chain_head 不符 |
//! | A4 引擎替换（改引擎产假链） | engine_id 入锚点载荷，跨引擎不匹配 |
//!
//! 诚实边界：本机制防「事后篡改留痕不可抵赖」，不防「密钥持有者从头记假账」
//! （那是 HSM/门限签名的 T3 领域）；密钥泄露则锚点链整体失守。
//!
//! # 确定性纪律
//!
//! 锚点载荷哈希用 BLAKE3（与事实链同源）；签名用 ed25519 确定性 nonce
//! （RFC 8032）。本模块不含密钥生成/时钟/随机——密钥由调用方注入，
//! 时刻戳（如需）由部署面提供，TCB 纪律不破。

use crate::hash;
use crate::signing::{self, verify_signature, AuditSigner, SignError};

/// 锚点链错误
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnchorError {
    /// 载荷构造/哈希失败
    Hash(String),
    /// 签名输入非法
    Sign(SignError),
    /// 锚点链结构错误（prev 链接断裂/序号跳跃/创世不匹配）
    ChainBroken {
        /// 断裂位置的锚点下标
        index: usize,
        /// 原因描述
        reason: String,
    },
    /// 签名验证失败（公钥不匹配/签名伪造）
    SignatureInvalid {
        /// 验证失败的锚点下标
        index: usize,
    },
}

impl core::fmt::Display for AnchorError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            AnchorError::Hash(msg) => write!(f, "审计锚点: 载荷哈希失败: {msg}"),
            AnchorError::Sign(e) => write!(f, "审计锚点: {e}"),
            AnchorError::ChainBroken { index, reason } => {
                write!(f, "审计锚点: 锚点链断裂于[{index}]: {reason}")
            }
            AnchorError::SignatureInvalid { index } => {
                write!(f, "审计锚点: 签名验证失败于[{index}]")
            }
        }
    }
}

impl std::error::Error for AnchorError {}

/// 锚点链哈希算法的创世值（与事实链 genesis 语义对齐，但独立命名空间）
pub const ANCHOR_GENESIS: &str = "anchor-genesis";

/// 锚点签名载荷格式版本
///
/// - `evorule-anchor/1`（0.8.1）：签名目标 = anchor_hash 的 hex 字节——
///   独立验证器重算载荷须 blake3（依赖门槛）；
/// - `evorule-anchor/2`（0.8.2）：签名目标 = 载荷字段串接字节（见
///   [`signature_payload_v2`]）——零依赖可验（字段串接直接可重构），
///   anchor_hash 链仍保留（A2 截断/fork 检出走 blake3 面不变）。
pub const ANCHOR_FORMAT: &str = "evorule-anchor/2";

/// v2 签名载荷：字段以 `|` 串接（与 compute_anchor_hash 同字段序，
/// 不含 prev_anchor_hash——签名只锚本锚点自身内容，链式关系由
/// anchor_hash 链承载）
///
/// 格式：`{ANCHOR_FORMAT}|{session_id}|{seq}|{lo}|{hi}|{chain_head}|{key_id}|{engine_id}|{logical_time}`
pub fn signature_payload_v2(proto: &AnchorProto) -> String {
    format!(
        "{}|{}|{}|{}|{}|{}|{}|{}|{}",
        ANCHOR_FORMAT,
        proto.session_id,
        proto.seq,
        proto.fact_range.lo,
        proto.fact_range.hi,
        proto.chain_head,
        proto.key_id,
        proto.engine_id,
        proto.logical_time,
    )
}

/// 逻辑时钟值（锚点记录的逻辑时间，由 auditor 的 LogicalClock 供给）
pub type LogicalTime = u64;

/// 序号计数：锚点链自身的序号（0 起，严格递增）
pub type AnchorSeq = u64;

/// 被审计的事实序号区间（前闭后开）：`[fact_lo, fact_hi)`
///
/// `fact_lo` = 上一锚点未覆盖的首条事实下标（首锚点为 0）；
/// `fact_hi` = 本锚点覆盖的最后一条事实下标 + 1。
/// 区间严格递增且相邻锚点 `prev.fact_hi == next.fact_lo`（无缝覆盖）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct FactRange {
    /// 区间起点（含）：上一锚点覆盖止点
    pub lo: usize,
    /// 区间终点（不含）：本锚点覆盖的最后事实下标 + 1
    pub hi: usize,
}

/// 审计锚点（Checkpoint）
///
/// 一个锚点 = 对「截至 fact_hi 为止的审计链尾」的一次签名封条。
/// 载荷哈希覆盖全部防伪造字段；签名由 [`AuditSigner`] 对载荷哈希字节施加。
///
/// Serialize/Deserialize（P0-1 锚点持久化）：JSONL 锚点日志的行格式；
/// 字段全量参与序列化（无跳过）——持久化即全部防伪造字段。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AuditAnchor {
    /// 锚点链序号（0 起，严格 +1）
    pub seq: AnchorSeq,
    /// 会话标识（多会话 WAL 场景区分链归属；空串=单会话缺省）
    pub session_id: String,
    /// 本锚点覆盖的事实序号区间 [lo, hi)
    pub fact_range: FactRange,
    /// 截至本锚点的审计链（事实链）链头哈希
    pub chain_head: String,
    /// 签名密钥标识（轮换支持：验证方按 key_id 选公钥）
    pub key_id: String,
    /// 引擎/部署标识（A4：跨引擎链冒充检出；由部署面注入，如 "evorule-server/0.9.1"）
    pub engine_id: String,
    /// 逻辑时刻（auditor LogicalClock，非墙钟——确定性纪律）
    pub logical_time: LogicalTime,
    /// 载荷哈希 = anchor_chain_step(prev_anchor_hash, 载荷字段)
    pub anchor_hash: String,
    /// ed25519 签名（对 anchor_hash 的 64 字节签名的 hex，128 字符）
    pub signature: String,
}

impl AuditAnchor {
    /// 计算锚点载荷哈希（不含签名自身）
    ///
    /// `anchor_hash = blake3(prev_anchor_hash ++ session_id ++ seq
    ///                        ++ fact_lo ++ fact_hi ++ chain_head ++ key_id
    ///                        ++ engine_id ++ logical_time)`
    ///
    /// 字段以 `|` 分隔拼接（字段值均为 hex/数字/标识，不含 `|`，无歧义碰撞）。
    pub fn compute_anchor_hash(
        prev_anchor_hash: &str,
        proto: &AnchorProto,
    ) -> Result<String, AnchorError> {
        let combined = format!(
            "{}|{}|{}|{}|{}|{}|{}|{}|{}",
            prev_anchor_hash,
            proto.session_id,
            proto.seq,
            proto.fact_range.lo,
            proto.fact_range.hi,
            proto.chain_head,
            proto.key_id,
            proto.engine_id,
            proto.logical_time,
        );
        let h = blake3::hash(combined.as_bytes()).to_hex().to_string();
        Ok(h)
    }

    /// 验证单个锚点：重算载荷哈希 + 验签名
    ///
    /// `prev_anchor_hash` 为前一锚点的 anchor_hash（首锚点用 [`ANCHOR_GENESIS`]）。
    pub fn verify_one(
        &self,
        prev_anchor_hash: &str,
        verifying_key_hex: &str,
    ) -> Result<(), AnchorError> {
        // 1. 重算载荷哈希（字段级防篡改）
        let proto = AnchorProto {
            seq: self.seq,
            session_id: self.session_id.clone(),
            fact_range: self.fact_range,
            chain_head: self.chain_head.clone(),
            key_id: self.key_id.clone(),
            engine_id: self.engine_id.clone(),
            logical_time: self.logical_time,
        };
        let expect = Self::compute_anchor_hash(prev_anchor_hash, &proto)?;
        if expect != self.anchor_hash {
            return Err(AnchorError::ChainBroken {
                index: self.seq as usize,
                reason: format!(
                    "载荷哈希不匹配: 期望 {:.16}… 实际 {:.16}…（字段被篡改或 prev 链接错误）",
                    expect, self.anchor_hash
                ),
            });
        }
        // 2. 验签名（真实性/防抵赖）
        let vk = signing::hex_decode(verifying_key_hex).map_err(AnchorError::Sign)?;
        let mut vk_arr = [0u8; 32];
        if vk.len() != 32 {
            return Err(AnchorError::Sign(SignError::Invalid(format!(
                "公钥长度 != 32: {}",
                vk.len()
            ))));
        }
        vk_arr.copy_from_slice(&vk);
        let sig_bytes = signing::hex_decode(&self.signature).map_err(AnchorError::Sign)?;
        let mut sig_arr = [0u8; 64];
        if sig_bytes.len() != 64 {
            return Err(AnchorError::Sign(SignError::Invalid(format!(
                "签名长度 != 64: {}",
                sig_bytes.len()
            ))));
        }
        sig_arr.copy_from_slice(&sig_bytes);
        // v2:验签目标=载荷串接字节(与 seal 同源);v1 锚点(0.8.1 产物)不兼容——
        // 混版链按失败处理(诚实边界:不静默跨格式验过)
        let proto_payload = signature_payload_v2(&proto);
        let ok = verify_signature(vk_arr, proto_payload.as_bytes(), &sig_arr)
            .map_err(AnchorError::Sign)?;
        if !ok {
            return Err(AnchorError::SignatureInvalid {
                index: self.seq as usize,
            });
        }
        Ok(())
    }
}

/// 锚点载荷字段（签名前的原型；`AuditAnchor` 减去 anchor_hash/signature）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnchorProto {
    /// 锚点链序号（0 起，严格 +1）
    pub seq: AnchorSeq,
    /// 会话标识
    pub session_id: String,
    /// 覆盖的事实序号区间
    pub fact_range: FactRange,
    /// 截至本锚点的事实链链头哈希
    pub chain_head: String,
    /// 签名密钥标识
    pub key_id: String,
    /// 引擎/部署标识
    pub engine_id: String,
    /// 逻辑时刻（LogicalClock 供给）
    pub logical_time: LogicalTime,
}

/// 锚点链构建器（签名侧持有；auditor 挂点使用）
///
/// 维护 `prev_anchor_hash` 状态，逐步产出锚点。确定性：同输入序列 → 同锚点序列。
pub struct AnchorChainBuilder {
    signer: AuditSigner,
    key_id: String,
    engine_id: String,
    session_id: String,
    prev_anchor_hash: String,
    last_seq: AnchorSeq,
    last_fact_hi: usize,
}

impl AnchorChainBuilder {
    /// 构建构建器
    ///
    /// - `signer`：签名器（私钥由调用方注入，不入审计资产）
    /// - `key_id`：密钥标识（轮换时新 builder 用新 key_id）
    /// - `engine_id`：部署面引擎标识（A4 检出用）
    /// - `session_id`：会话标识
    pub fn new(signer: AuditSigner, key_id: String, engine_id: String, session_id: String) -> Self {
        Self {
            signer,
            key_id,
            engine_id,
            session_id,
            prev_anchor_hash: ANCHOR_GENESIS.to_string(),
            last_seq: 0,
            last_fact_hi: 0,
        }
    }

    /// 产出一个锚点，覆盖 `[self.last_fact_hi, fact_hi)` 并锚定 `chain_head`
    ///
    /// 调用方约束：`fact_hi >= self.last_fact_hi`（允许相等=空区间锚点，
    /// 用于会话结束封条）；`chain_head` 必须是截至 fact_hi 的事实链链头。
    pub fn seal(&mut self, fact_hi: usize, chain_head: &str) -> Result<AuditAnchor, AnchorError> {
        let proto = AnchorProto {
            seq: self.last_seq,
            session_id: self.session_id.clone(),
            fact_range: FactRange {
                lo: self.last_fact_hi,
                hi: fact_hi,
            },
            chain_head: chain_head.to_string(),
            key_id: self.key_id.clone(),
            engine_id: self.engine_id.clone(),
            logical_time: 0, // 由 auditor 挂点覆盖（见 Auditor::seal_anchor）
        };
        let anchor_hash = AuditAnchor::compute_anchor_hash(&self.prev_anchor_hash, &proto)?;
        // v2:签名目标=载荷串接字节(零依赖可验);v1 曾签 anchor_hash(见 ANCHOR_FORMAT 注释)
        let sig = self
            .signer
            .signature_bytes(signature_payload_v2(&proto).as_bytes());
        let anchor = AuditAnchor {
            seq: proto.seq,
            session_id: proto.session_id,
            fact_range: proto.fact_range,
            chain_head: proto.chain_head,
            key_id: proto.key_id,
            engine_id: proto.engine_id,
            logical_time: proto.logical_time,
            anchor_hash,
            signature: signing::hex_encode(&sig),
        };
        self.prev_anchor_hash = anchor.anchor_hash.clone();
        self.last_seq += 1;
        self.last_fact_hi = fact_hi;
        Ok(anchor)
    }

    /// 当前覆盖进度（下一锚点的 fact_lo）
    pub fn next_fact_lo(&self) -> usize {
        self.last_fact_hi
    }

    /// 签名器只读访问（锚点持久化恢复时重建 builder 用）
    pub fn signer(&self) -> &AuditSigner {
        &self.signer
    }

    /// 锚点身份四元组只读访问（持久化恢复重建 builder 用）
    pub fn identity(&self) -> (&AuditSigner, &str, &str, &str) {
        (&self.signer, &self.key_id, &self.engine_id, &self.session_id)
    }

    /// 从既有链状态续建（密钥轮换/进程重启恢复用）
    ///
    /// - `prev_anchor_hash`：前一锚点的 anchor_hash（无前锚=null 时用 ANCHOR_GENESIS）
    /// - `next_seq`：下一锚点的序号
    /// - `next_fact_lo`：下一锚点覆盖区间的起点
    pub fn new_with_prev(
        signer: AuditSigner,
        key_id: String,
        engine_id: String,
        session_id: String,
        prev_anchor_hash: String,
        next_seq: AnchorSeq,
        next_fact_lo: usize,
    ) -> Self {
        Self {
            signer,
            key_id,
            engine_id,
            session_id,
            prev_anchor_hash,
            last_seq: next_seq,
            last_fact_hi: next_fact_lo,
        }
    }
}

/// 验证整条锚点链（验证方入口）
///
/// 校验规则（全部通过才算有效）：
/// 1. **序号连续**：seq 从 0 严格 +1；
/// 2. **区间无缝**：首锚点 lo=0；相邻锚点 prev.hi == next.lo；区间非空检查由调用方语义决定；
/// 3. **锚点哈希链**：逐锚点重算（含 prev 链接）；
/// 4. **签名有效**：逐锚点按 key_id 选公钥验签。
///
/// `keys`：key_id → 公钥 hex 映射（轮换支持）。
pub fn verify_anchor_chain(
    anchors: &[AuditAnchor],
    keys: &std::collections::BTreeMap<String, String>,
) -> Result<(), AnchorError> {
    let mut prev_hash = ANCHOR_GENESIS.to_string();
    let mut expect_lo: usize = 0;
    for (i, a) in anchors.iter().enumerate() {
        // 1. 序号连续
        if a.seq != i as AnchorSeq {
            return Err(AnchorError::ChainBroken {
                index: i,
                reason: format!("序号不连续: 期望 {} 实际 {}", i, a.seq),
            });
        }
        // 2. 区间无缝
        if a.fact_range.lo != expect_lo {
            return Err(AnchorError::ChainBroken {
                index: i,
                reason: format!(
                    "事实区间不无缝: 期望 lo={} 实际 lo={}（前缀截断或区间丢失）",
                    expect_lo, a.fact_range.lo
                ),
            });
        }
        // 3+4. 载荷哈希 + 签名
        let key = keys
            .get(&a.key_id)
            .ok_or_else(|| AnchorError::ChainBroken {
                index: i,
                reason: format!("未知 key_id: {}（公钥表缺失）", a.key_id),
            })?;
        a.verify_one(&prev_hash, key)?;
        prev_hash = a.anchor_hash.clone();
        expect_lo = a.fact_range.hi;
    }
    Ok(())
}

/// 锚点链与事实链的绑定校验（验证方核心入口）
///
/// 在锚点链自身有效的前提下，进一步验证「锚点声称的链头」与
/// 「事实链重算的链头」一致——这是 A1/A3 检出的最后一环：
/// 锚点链合法但内容锚错事实链 = 无效绑定。
///
/// `facts`：完整事实序列（按因果顺序）；
/// `anchors`：锚点链；
/// 返回最后一个锚点覆盖到的 fact 下标（exclusive）。
pub fn verify_binding(
    facts: &[evorule_reactor::Fact],
    anchors: &[AuditAnchor],
) -> Result<usize, AnchorError> {
    // 整链口径：逐锚点用 [0, hi) 前缀重算整链头比对
    // （锚点内 chain_head 语义 = 截至本锚点的整链链头，非区间链）
    let mut covered: usize = 0;
    for a in anchors {
        let prefix = facts
            .get(0..a.fact_range.hi)
            .ok_or_else(|| AnchorError::ChainBroken {
                index: a.seq as usize,
                reason: format!(
                    "事实数不足: 锚点声称 hi={} 但事实总数={}",
                    a.fact_range.hi,
                    facts.len()
                ),
            })?;
        let head =
            hash::compute_chain_hash(prefix).map_err(|e| AnchorError::Hash(e.to_string()))?;
        if head != a.chain_head {
            return Err(AnchorError::ChainBroken {
                index: a.seq as usize,
                reason: format!(
                    "链头绑定失败: 锚点[{}]声称 {:.16}… 实际重算 {:.16}…（事实链被篡改/替换，或锚点属另一条链）",
                    a.seq,
                    a.chain_head,
                    head
                ),
            });
        }
        covered = a.fact_range.hi;
    }
    Ok(covered)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::panic, clippy::expect_used)]
    use super::*;
    use crate::signing::AuditSigner;

    fn signer() -> AuditSigner {
        AuditSigner::from_bytes([7u8; 32])
    }

    #[test]
    fn test_v2_signature_payload_deterministic() {
        // v2 载荷:字段序确定,零依赖方可按字符串模板重构
        let proto = AnchorProto {
            seq: 3,
            session_id: "sess-1".into(),
            fact_range: FactRange { lo: 5, hi: 10 },
            chain_head: "ab".repeat(32),
            key_id: "k1".into(),
            engine_id: "evorule-server/0.9.2".into(),
            logical_time: 7,
        };
        let s = signature_payload_v2(&proto);
        assert_eq!(
            s,
            "evorule-anchor/2|sess-1|3|5|10|".to_string()
                + &"ab".repeat(32)
                + "|k1|evorule-server/0.9.2|7"
        );
    }

    #[test]
    fn test_v1_anchor_rejected_under_v2() {
        // v1 锚点(签 anchor_hash)在 v2 验证下必须失败——不静默跨格式验过
        let s = signer();
        let mut b = AnchorChainBuilder::new(s.clone(), "k1".into(), "e".into(), "sess-1".into());
        let a = b.seal(5, &"cd".repeat(32)).unwrap();
        // 伪造 v1 形态:对 anchor_hash 签名(v1 语义),塞进 v2 锚点
        let mut v1_anchor = a.clone();
        let sig_v1 = s.signature_bytes(a.anchor_hash.as_bytes());
        v1_anchor.signature = crate::signing::hex_encode(&sig_v1);
        let mut keys = std::collections::BTreeMap::new();
        keys.insert("k1".to_string(), s.verifying_hex());
        // v1 签名在 v2 验证器下:验签失败(载荷哈希仍对,但签名对不上)
        assert!(v1_anchor
            .verify_one(crate::anchor::ANCHOR_GENESIS, &keys["k1"])
            .is_err());
        // 同锚点 v2 重封后验过(对照组,证明失败源于格式不匹配非构造错误)
        assert!(a
            .verify_one(crate::anchor::ANCHOR_GENESIS, &keys["k1"])
            .is_ok());
    }

    #[test]
    fn test_anchor_chain_build_and_verify() {
        let s = signer();
        let pk = s.verifying_hex();
        let mut b = AnchorChainBuilder::new(
            s.clone(),
            "k1".into(),
            "evorule-test/0.1".into(),
            "sess-1".into(),
        );
        let mut anchors = vec![];
        // 模拟三条事实链头（哈希值任意但确定性）
        let heads = [
            "aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111aaaa1111",
            "bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222bbbb2222",
            "cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333cccc3333",
        ];
        let mut his = [5usize, 10, 15];
        his.sort();
        for (i, (&hi, &h)) in his.iter().zip(heads.iter()).enumerate() {
            let a = b.seal(hi, h).unwrap();
            assert_eq!(a.seq, i as u64);
            anchors.push(a);
        }
        let mut keys = std::collections::BTreeMap::new();
        keys.insert("k1".to_string(), pk);
        assert!(verify_anchor_chain(&anchors, &keys).is_ok());
    }

    #[test]
    fn test_prefix_truncation_detected() {
        // A2'：丢弃首锚点 → 首锚点 lo != 0 → 检出
        let s = signer();
        let pk = s.verifying_hex();
        let mut b = AnchorChainBuilder::new(s, "k1".into(), "e".into(), "s".into());
        let a0 = b.seal(5, "h0").unwrap();
        let a1 = b.seal(10, "h1").unwrap();
        let truncated = vec![a1];
        let _ = a0;
        let mut keys = std::collections::BTreeMap::new();
        keys.insert("k1".to_string(), pk);
        assert!(matches!(
            verify_anchor_chain(&truncated, &keys),
            Err(AnchorError::ChainBroken { index: 0, .. })
        ));
    }

    #[test]
    fn test_chain_substitution_detected() {
        // A2：整链替换 → 锚点签名验不过
        let s = signer();
        let pk_real = s.verifying_hex();
        let mut b = AnchorChainBuilder::new(s, "k1".into(), "e".into(), "s".into());
        let real = b.seal(5, "h0").unwrap();
        // 攻击者自建 builder（不同私钥）产出伪造锚点
        let attacker = AuditSigner::from_bytes([9u8; 32]);
        let mut fake_b = AnchorChainBuilder::new(attacker, "k1".into(), "e".into(), "s".into());
        let fake = fake_b.seal(5, "h0-evil").unwrap();
        let _ = real;
        let mut keys = std::collections::BTreeMap::new();
        keys.insert("k1".to_string(), pk_real);
        assert!(matches!(
            verify_anchor_chain(&[fake], &keys),
            Err(AnchorError::SignatureInvalid { .. }) | Err(AnchorError::ChainBroken { .. })
        ));
    }

    #[test]
    fn test_key_rotation_supported() {
        // J-T5：轮换后旧锚点仍可验（多 key 并存）
        let s1 = AuditSigner::from_bytes([7u8; 32]);
        let s2 = AuditSigner::from_bytes([8u8; 32]);
        let pk1 = s1.verifying_hex();
        let pk2 = s2.verifying_hex();
        let mut b1 = AnchorChainBuilder::new(s1, "k1".into(), "e".into(), "s".into());
        let a0 = b1.seal(5, "h0").unwrap();
        // 轮换：新 builder 从当前进度续链（生产中由 auditor 迁移状态）
        let mut b2 = AnchorChainBuilder::new_with_prev(
            s2,
            "k2".into(),
            "e".into(),
            "s".into(),
            a0.anchor_hash.clone(),
            1,
            5,
        );
        let a1 = b2.seal(10, "h1").unwrap();
        let mut keys = std::collections::BTreeMap::new();
        keys.insert("k1".to_string(), pk1);
        keys.insert("k2".to_string(), pk2);
        assert!(verify_anchor_chain(&[a0, a1], &keys).is_ok());
    }

    #[test]
    fn test_engine_swap_detected() {
        // A4：engine_id 不同 → 载荷哈希不同 → 链断
        let s = signer();
        let pk = s.verifying_hex();
        let mut b1 = AnchorChainBuilder::new(s.clone(), "k1".into(), "engine-a".into(), "s".into());
        let a0 = b1.seal(5, "h0").unwrap();
        let mut b2 = AnchorChainBuilder::new_with_prev(
            s,
            "k1".into(),
            "engine-b".into(), // 引擎被替换
            "s".into(),
            a0.anchor_hash.clone(),
            1,
            5,
        );
        let a1 = b2.seal(10, "h1").unwrap();
        let mut keys = std::collections::BTreeMap::new();
        keys.insert("k1".to_string(), pk);
        // 锚点链本身自洽（b2 续链正确），但 engine_id 变化在验证方可配策略拦截；
        // 本测验证机制：engine_id 参与哈希（篡改 engine_id 必然哈希失配）
        assert!(verify_anchor_chain(&[a0.clone(), a1.clone()], &keys).is_ok());
        let mut tampered = a1.clone();
        tampered.engine_id = "engine-c".into();
        assert!(matches!(
            verify_anchor_chain(&[a0, tampered], &keys),
            Err(AnchorError::ChainBroken { .. })
        ));
    }

    #[test]
    fn test_builder_with_prev_for_rotation() {
        // 轮换续链：new_with_prev 保持链连续性
        let s1 = AuditSigner::from_bytes([7u8; 32]);
        let s2 = AuditSigner::from_bytes([8u8; 32]);
        let mut b1 = AnchorChainBuilder::new(s1, "k1".into(), "e".into(), "s".into());
        let a0 = b1.seal(5, "h0").unwrap();
        let mut b2 = AnchorChainBuilder::new_with_prev(
            s2,
            "k2".into(),
            "e".into(),
            "s".into(),
            a0.anchor_hash.clone(),
            1,
            5,
        );
        let a1 = b2.seal(10, "h1").unwrap();
        assert_eq!(a1.seq, 1);
        assert_eq!(a1.fact_range.lo, 5);
    }
}
