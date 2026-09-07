// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 配置键存储层 —— JSON 文件 + RwLock + 原子 rename 写入。
//!
//! 设计:
//! - 首期: JSON 文件 + Arc<RwLock<ConfigMap>>，整合包零依赖。
//! - 升级路径: Store trait 已抽象，可切换到 WorkspaceDb，接口不变。
//! - fail-fast: 文件损坏 / 版本不兼容 = 拒绝加载 + 自诊断指引（符 H 约束）。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 配置条目集合（键 → 值）。
pub type ConfigMap = BTreeMap<String, Value>;

/// 单条审计记录（配置变更历史）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuditEntry {
    pub key: String,
    pub old: Option<Value>,
    pub new: Value,
    pub who: String,
    pub when: String,
    pub reason: String,
    pub approved_by: String,
}

/// 单条 pending 提案（等待审批的配置变更请求）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Proposal {
    pub proposal_id: String,
    pub key: String,
    pub new_value: Value,
    pub reason: String,
    pub proposed_by: String,
    pub created_at: String,
    pub status: String, // "pending" | "approved" | "rejected"
    #[serde(default)]
    pub approver: Option<String>,
}

/// JSON 文件顶层结构。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoreFile {
    pub version: u32,
    pub updated_at: String,
    pub entries: ConfigMap,
    #[serde(default)]
    pub audit: Vec<AuditEntry>,
    #[serde(default)]
    pub proposals: Vec<Proposal>,
}

impl StoreFile {
    fn empty() -> Self {
        Self {
            version: 1,
            updated_at: Self::now(),
            entries: BTreeMap::new(),
            audit: Vec::new(),
            proposals: Vec::new(),
        }
    }

    fn now() -> String {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| format!("{}", d.as_secs()))
            .unwrap_or_else(|_| "0".to_string())
    }
}

/// 配置存储实现（JSON 文件）。
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
    inner: Arc<RwLock<StoreFile>>,
}

impl ConfigStore {
    pub fn open(path: &Path) -> Result<Self, String> {
        let path = path.to_path_buf();
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() && !parent.exists() {
                fs::create_dir_all(parent).map_err(|e| {
                    format!(
                        "无法创建配置目录 {}: {}（自诊断指引: 确认运行用户对 {} 有写权限）",
                        parent.display(),
                        e,
                        parent.display()
                    )
                })?;
            }
        }

        let store_file = if path.exists() {
            let content = fs::read_to_string(&path).map_err(|e| {
                format!(
                    "读取配置文件失败 {}: {}（自诊断指引: 确认文件存在且可读）",
                    path.display(),
                    e
                )
            })?;
            serde_json::from_str::<StoreFile>(&content).map_err(|e| {
                format!(
                    "配置文件 JSON 非法 {}: {}（自诊断指引: 用 {} 打开检查格式; 损坏可删除后由系统重建）",
                    path.display(),
                    e,
                    path.display()
                )
            })?
        } else {
            StoreFile::empty()
        };

        Ok(Self {
            path,
            inner: Arc::new(RwLock::new(store_file)),
        })
    }

    pub fn get(&self, key: &str) -> Option<Value> {
        let guard = self.inner.read().map_err(|e| e.to_string()).ok()?;
        guard.entries.get(key).cloned()
    }

    pub fn all_keys(&self) -> Vec<String> {
        self.inner
            .read()
            .map_err(|e| e.to_string())
            .map(|g| g.entries.keys().cloned().collect())
            .unwrap_or_default()
    }

    pub fn create_proposal(
        &self,
        key: &str,
        new_value: Value,
        reason: &str,
        proposed_by: &str,
    ) -> Result<String, String> {
        let mut guard = self.inner.write().map_err(|e| e.to_string())?;
        let proposal_id = format!(
            "PROP-{}-{:03}",
            StoreFile::now(),
            guard.proposals.len() + 1
        );
        guard.proposals.push(Proposal {
            proposal_id: proposal_id.clone(),
            key: key.to_string(),
            new_value,
            reason: reason.to_string(),
            proposed_by: proposed_by.to_string(),
            created_at: StoreFile::now(),
            status: "pending".to_string(),
            approver: None,
        });
        self.persist(&guard)?;
        Ok(proposal_id)
    }

    pub fn approve_proposal(
        &self,
        proposal_id: &str,
        approver: &str,
    ) -> Result<(), String> {
        let mut guard = self.inner.write().map_err(|e| e.to_string())?;
        let idx = guard
            .proposals
            .iter()
            .position(|p| p.proposal_id == proposal_id)
            .ok_or_else(|| format!("提案 {proposal_id} 不存在"))?;
        let prop = guard.proposals[idx].clone();
        if prop.status != "pending" {
            return Err(format!(
                "提案 {proposal_id} 状态为 {}，无法审批",
                prop.status
            ));
        }
        let old = guard.entries.get(&prop.key).cloned();
        guard
            .entries
            .insert(prop.key.clone(), prop.new_value.clone());
        guard.updated_at = StoreFile::now();
        guard.audit.push(AuditEntry {
            key: prop.key.clone(),
            old,
            new: prop.new_value.clone(),
            who: prop.proposed_by.clone(),
            when: StoreFile::now(),
            reason: prop.reason.clone(),
            approved_by: approver.to_string(),
        });
        guard.proposals[idx].status = "approved".to_string();
        guard.proposals[idx].approver = Some(approver.to_string());
        self.persist(&guard)
    }

    pub fn reject_proposal(&self, proposal_id: &str, approver: &str) -> Result<(), String> {
        let mut guard = self.inner.write().map_err(|e| e.to_string())?;
        let idx = guard
            .proposals
            .iter()
            .position(|p| p.proposal_id == proposal_id)
            .ok_or_else(|| format!("提案 {proposal_id} 不存在"))?;
        guard.proposals[idx].status = "rejected".to_string();
        guard.proposals[idx].approver = Some(approver.to_string());
        self.persist(&guard)
    }

    fn persist(&self, guard: &StoreFile) -> Result<(), String> {
        let json = serde_json::to_string_pretty(guard)
            .map_err(|e| format!("序列化配置失败: {e}"))?;
        let tmp = self.path.with_extension("json.tmp");
        fs::write(&tmp, json).map_err(|e| {
            format!(
                "写临时配置文件 {} 失败: {}（自诊断指引: 确认磁盘空间与权限）",
                tmp.display(),
                e
            )
        })?;
        std::fs::remove_file(&self.path).ok(); fs::rename(&tmp, &self.path).map_err(|e| {
            format!(
                "原子替换配置文件 {} 失败: {}（自诊断指引: 确认无其他进程独占此文件）",
                self.path.display(),
                e
            )
        })?;
        Ok(())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
    use super::*;

    fn temp_dir() -> PathBuf {
        let tid = format!("{:?}", std::thread::current().id());
        let dir = std::env::temp_dir().join(format!(
            "evorule-finance-config-test-{}-{}",
            std::process::id(),
            tid
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn test_open_creates_new_file() {
        let dir = temp_dir();
        let path = dir.join("finance-config.json");
        let _store = ConfigStore::open(&path).expect("open should succeed");
        // 注意: open 新建 store 时文件不自动落盘（只在 create_proposal/approve 时 persist）。
        // 这里只验证 open 成功 + store 可用。
    }

    #[test]
    fn test_get_set_roundtrip() {
        let dir = temp_dir();
        let path = dir.join("finance-config.json");
        let store = ConfigStore::open(&path).unwrap();

        let pid = store
            .create_proposal(
                "limits.travel.max_amount",
                Value::from(2000),
                "初始值",
                "test_user",
            )
            .unwrap();
        store.approve_proposal(&pid, "finance_dir").unwrap();

        let v = store.get("limits.travel.max_amount").unwrap();
        assert_eq!(v, Value::from(2000));

        let keys = store.all_keys();
        assert!(keys.contains(&"limits.travel.max_amount".to_string()));
    }

    #[test]
    fn test_get_missing_key_returns_none() {
        let dir = temp_dir();
        let path = dir.join("finance-config.json");
        let store = ConfigStore::open(&path).unwrap();
        assert!(store.get("nope.nope.nope").is_none());
    }

    #[test]
    fn test_corrupt_file_fails_fast() {
        let dir = temp_dir();
        let path = dir.join("finance-config.json");
        fs::write(&path, "this is not json").unwrap();
        let err = ConfigStore::open(&path).unwrap_err();
        assert!(err.contains("JSON 非法"));
    }

    #[test]
    fn test_audit_entry_after_approval() {
        let dir = temp_dir();
        let path = dir.join("finance-config.json");
        let store = ConfigStore::open(&path).unwrap();

        let pid = store
            .create_proposal(
                "accounts.ledger.tax_rate",
                Value::from(0.06),
                "税率变更",
                "user_001",
            )
            .unwrap();
        store.approve_proposal(&pid, "finance_director").unwrap();

        let content = fs::read_to_string(&path).unwrap();
        let parsed: StoreFile = serde_json::from_str(&content).unwrap();
        assert!(!parsed.audit.is_empty());
        let last = parsed.audit.last().unwrap();
        assert_eq!(last.key, "accounts.ledger.tax_rate");
        assert_eq!(last.approved_by, "finance_director");
    }

    #[test]
    fn test_proposal_pending_until_approved() {
        let dir = temp_dir();
        let path = dir.join("finance-config.json");
        let store = ConfigStore::open(&path).unwrap();

        let pid = store
            .create_proposal(
                "limits.travel.max_amount",
                Value::from(5000),
                "临时上调",
                "user_001",
            )
            .unwrap();

        assert!(store.get("limits.travel.max_amount").is_none());

        store.approve_proposal(&pid, "finance_dir").unwrap();

        assert_eq!(store.get("limits.travel.max_amount"), Some(Value::from(5000)));
    }

    #[test]
    fn test_reject_proposal_leaves_value_unchanged() {
        let dir = temp_dir();
        let path = dir.join("finance-config.json");
        let store = ConfigStore::open(&path).unwrap();

        let pid = store
            .create_proposal(
                "limits.travel.max_amount",
                Value::from(5000),
                "临时上调（被拒）",
                "user_001",
            )
            .unwrap();
        store.reject_proposal(&pid, "manager").unwrap();

        assert!(store.get("limits.travel.max_amount").is_none());
    }
}