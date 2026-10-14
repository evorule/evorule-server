// 黄金向量生成器（人工运行的示例面产物工具）：确定性输入上的 unwrap 属示例惯用法。
#![allow(clippy::unwrap_used)]

use evorule_governance::anchor::AnchorChainBuilder;
use evorule_governance::signing::AuditSigner;
fn main() {
    let s = AuditSigner::from_bytes([0xabu8; 32]);
    let pk = s.verifying_hex();
    let mut b = AnchorChainBuilder::new(
        s.clone(),
        "anchor-key-1".into(),
        "vector-engine/0.1".into(),
        "vector-sess-1".into(),
    );
    let heads = ["aa".repeat(32), "bb".repeat(32)];
    let his = [5usize, 10];
    let mut out = Vec::new();
    for (&hi, h) in his.iter().zip(heads.iter()) {
        let a = b.seal(hi, h).unwrap();
        out.push(serde_json::json!({
            "seq": a.seq, "session_id": a.session_id,
            "fact_range": {"lo": a.fact_range.lo, "hi": a.fact_range.hi},
            "chain_head": a.chain_head, "key_id": a.key_id,
            "engine_id": a.engine_id, "logical_time": a.logical_time,
            "anchor_hash": a.anchor_hash, "signature": a.signature,
        }));
    }
    println!("__PK__{}", pk);
    println!("__ANCHORS__{}", serde_json::to_string(&out).unwrap());
}
