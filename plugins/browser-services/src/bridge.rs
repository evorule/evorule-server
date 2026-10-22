// SPDX-License-Identifier: AGPL-3.0-or-later
// Copyright (C) 2026 EvoRule Project
// This file is part of EvoRule, licensed under GNU Affero General Public License v3 or later.
//! BlockingBridge：同步 NativeService trait × async zendriver 的桥接。
//!
//! evorule-server 的 `NativeService::execute` 是同步签名，而 zendriver 全是 tokio
//! Future。本桥接自持一个**独立** tokio Runtime（禁止在 server 主 runtime 内
//! block_on，否则嵌套死锁），每次同步调用 spawn 过去并阻塞等结果。
//!
//! Browser 句柄为进程级单例（OnceLock）：跨调用复用同一浏览器进程。
//! 浏览器路径/profile 目录缺省 Edge+data/edge-profile，均可经
//! `EVORULE_BROWSER_PATH` / `EVORULE_BROWSER_PROFILE_DIR` 覆盖；profile
//! 目录一律绝对路径化（相对路径触发浏览器默认目录回退+调试口安全拒绝，见
//! [`browser_profile_dir`] 注释）。

use std::sync::{Mutex, OnceLock};

use zendriver::Browser;

/// monitor 事件收集 buffer（进程级，后台 task 写，service 调用读快照）。
pub static MONITOR_BUF: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

pub fn monitor_buf() -> &'static Mutex<Vec<String>> {
    MONITOR_BUF.get_or_init(|| Mutex::new(Vec::new()))
}

/// Edge 可执行路径缺省（用户指定：本机 Edge，Chrome 网络受限）。
/// 部署覆盖：环境变量 `EVORULE_BROWSER_PATH` 指定其他 Chromium 系浏览器绝对路径。
const EDGE_PATH: &str = r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe";

/// 浏览器可执行路径：env 覆盖 → 缺省 Edge。
fn browser_executable() -> String {
    match std::env::var("EVORULE_BROWSER_PATH") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => EDGE_PATH.to_string(),
    }
}

/// 持久化 profile 目录：env 覆盖 → 缺省 `data/edge-profile`（相对 server 工作目录）。
///
/// **必须解析为绝对路径再传给浏览器**（2026-10-22 实测）：相对 `--user-data-dir`
/// 在部分进程环境下浏览器写入失败后回退默认数据目录，进而触发"默认目录禁用
/// 远程调试"的安全拒绝（Edge stderr：DevTools remote debugging requires a
/// non-default data directory）——调试端口不立、真实弹窗报"无法写入数据"。
/// 绝对路径在全部实测组合下稳定（同机对照：绝对路径多次成功/相对路径在
/// server 进程内三次复现超时）。
fn browser_profile_dir() -> Result<std::path::PathBuf, String> {
    let raw = match std::env::var("EVORULE_BROWSER_PROFILE_DIR") {
        Ok(p) if !p.trim().is_empty() => p,
        _ => r"data\edge-profile".to_string(),
    };
    let dir = std::path::Path::new(&raw);
    let abs = if dir.is_absolute() {
        dir.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|e| format!("解析浏览器 profile 目录失败（取进程工作目录）: {e}"))?
            .join(dir)
    };
    std::fs::create_dir_all(&abs)
        .map_err(|e| format!("创建浏览器 profile 目录失败 {}: {e}", abs.display()))?;
    Ok(abs)
}

pub struct Bridge {
    rt: tokio::runtime::Runtime,
}

impl Bridge {
    fn new() -> Result<Self, String> {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .thread_keep_alive(std::time::Duration::from_secs(60))
            .enable_all()
            .build()
            .map_err(|e| format!("browser bridge tokio runtime 初始化失败: {e}"))?;
        Ok(Self { rt })
    }

    pub fn get() -> Result<&'static Bridge, String> {
        static BRIDGE: OnceLock<Result<Bridge, String>> = OnceLock::new();
        BRIDGE
            .get_or_init(Bridge::new)
            .as_ref()
            .map_err(|e| e.clone())
    }

    pub fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        // server HTTP handler 在 tokio worker 线程上调用本同步 execute；
        // 直接 self.rt.block_on 会触发 tokio "Cannot start a runtime from within a runtime" panic。
        // block_in_place 告知 tokio 本 worker 将阻塞，tokio 把其他任务挪到别的 worker，
        // 随后在本线程 block 独立 runtime 的 future——两个 runtime 不嵌套。
        tokio::task::block_in_place(|| self.rt.block_on(fut))
    }

    /// 在自有 runtime 上 detach 一个后台 task（monitor 常驻消费用）。
    pub fn spawn<F: std::future::Future + Send + 'static>(&self, fut: F)
    where
        F::Output: Send + 'static,
    {
        self.rt.spawn(fut);
    }

    /// 取（必要时启动）进程级 Edge 浏览器。已启动则复用。
    pub async fn browser() -> Result<&'static Browser, String> {
        static BROWSER: OnceLock<Result<&'static Browser, String>> = OnceLock::new();
        if BROWSER.get().is_none() {
            // 持久化 profile：保留 cookies/session/登录态（ALE 登录态题需要）。
            // 路径一律解析为绝对（相对路径的浏览器侧回退链见 browser_profile_dir 注释）。
            let profile_dir = browser_profile_dir()?;
            let exe = browser_executable();
            let b = Browser::builder()
                .executable(&exe)
                .user_data_dir(&profile_dir)
                .headless(true)
                .launch()
                .await
                .map_err(|e| {
                    format!(
                        "启动浏览器失败: {e}（自诊断: ①浏览器路径={exe}，可用 EVORULE_BROWSER_PATH 覆盖                          ②profile={}，可用 EVORULE_BROWSER_PROFILE_DIR 覆盖 ③残留浏览器进程锁 profile 时先结束同名进程再试）",
                        profile_dir.display()
                    )
                })?;
            let leaked: &'static Browser = Box::leak(Box::new(b));
            // 竞争时以先 set 者为准
            let _ = BROWSER.set(Ok(leaked));
        }
        match BROWSER.get() {
            Some(r) => r.clone(),
            None => Err("browser 未初始化".to_string()),
        }
    }
}

pub fn not_implemented(name: &str) -> evorule_reactor::IoResult {
    Err(format!("{name}: v0.1 已声明能力，尚未接 zendriver 实现"))
}
