// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! 文件系统监听器

use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::path::Path;
use std::sync::mpsc::{channel, Receiver};
use tracing::info;

/// 文件变更事件
#[derive(Debug)]
pub struct FileChange {
    pub path: String,
    pub event_type: ChangeType,
}

/// 文件变更类型
#[derive(Debug)]
pub enum ChangeType {
    Create,
    Modify,
    Remove,
}

impl From<Event> for FileChange {
    fn from(event: Event) -> Self {
        let path = event
            .paths
            .first()
            .map(|p| p.display().to_string())
            .unwrap_or_default();
        let event_type = if event.kind.is_create() {
            ChangeType::Create
        } else if event.kind.is_modify() {
            ChangeType::Modify
        } else if event.kind.is_remove() {
            ChangeType::Remove
        } else {
            ChangeType::Modify
        };
        Self { path, event_type }
    }
}

/// 创建文件监听器
pub fn create_watcher(path: &Path) -> Result<Receiver<FileChange>, notify::Error> {
    let (tx, rx) = channel();

    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| match res {
            Ok(event) => {
                let change: FileChange = event.into();
                if let Err(e) = tx.send(change) {
                    tracing::error!(error = %e, "发送文件变更事件失败");
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "文件监听错误");
            }
        },
        notify::Config::default(),
    )?;

    watcher.watch(path, RecursiveMode::Recursive)?;
    info!(path = %path.display(), "已开始监听规则目录");

    Ok(rx)
}
