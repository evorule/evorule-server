//! evorule 体验版一键安装器（Windows，单文件，免管理员权限）。
//!
//! 分发包全部内容由 build.rs 从 `payload/` 内嵌为单个 .exe：
//! 双击 → 确认 → 释放到 `%LOCALAPPDATA%\Evorule` → 建桌面/开始菜单快捷方式
//! → 注册 HKCU 卸载信息（可在「设置-应用」中卸载）→ 可选立即启动。
//!
//! 命令行（内部/测试用）：
//! - `--uninstall`          卸载模式
//! - `--silent`             无 UI 静默安装（写 %TEMP%\evorule-setup.log，不自动启动）
//! - `--dir <path>`         自定义安装目录（此时不写快捷方式/注册表，供 E2E 测试）

#![windows_subsystem = "windows"]

mod ffi;

// build.rs 生成的内嵌载荷表：`&[(&str 相对路径, &[u8] 内容)]`
include!(concat!(env!("OUT_DIR"), "/payload.rs"));

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const VERSION: &str = env!("CARGO_PKG_VERSION");
const APP_DIR_NAME: &str = "Evorule";
const REG_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Evorule";
const SHORTCUT_NAME: &str = "evorule 体验版";

fn main() {
    std::process::exit(dispatch());
}

fn dispatch() -> i32 {
    let mut silent = false;
    let mut uninstall_mode = false;
    let mut dir: Option<String> = None;
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut iter = args.iter().peekable();
    while let Some(a) = iter.next() {
        match a.as_str() {
            "--silent" => silent = true,
            "--uninstall" => uninstall_mode = true,
            "--dir" => {
                dir = iter.next().cloned();
            }
            other => {
                if let Some(v) = other.strip_prefix("--dir=") {
                    dir = Some(v.to_string());
                }
            }
        }
    }
    if uninstall_mode {
        return uninstall(silent);
    }
    install(silent, dir)
}

// ---------- 安装 ----------

fn install(silent: bool, dir_override: Option<String>) -> i32 {
    let is_test_mode = dir_override.is_some();
    let target = match dir_override {
        Some(d) => std::path::PathBuf::from(d),
        None => default_install_dir(),
    };
    let target_str = target.to_string_lossy().to_string();
    let mut log: Vec<String> = Vec::new();
    log.push(format!("evorule-setup v{VERSION}"));

    if !silent {
        let confirm = format!(
            "evorule 体验版 v{VERSION} 安装向导\n\n\
             将安装到：\n{target_str}\n\n\
             · 无需管理员权限\n\
             · 升级安装会保留你的规则与数据\n\n\
             点击「确定」开始安装。"
        );
        if !ffi::ask_ok_cancel(&confirm) {
            return 0;
        }
    }

    // 1. 释放文件（升级时 data\ 不在载荷中，天然保留）
    let file_count = match extract(&target, &mut log) {
        Ok(n) => n,
        Err(e) => return fail(silent, &target, &e),
    };

    // 2. 复制自身作为卸载器
    if let Err(e) = copy_self(&target) {
        return fail(silent, &target, &e);
    }
    log.push("self copied as uninstaller".to_string());

    // 3. 快捷方式 + 卸载注册表（仅默认安装路径；--dir 测试模式跳过）
    if !is_test_mode {
        if let Err(e) = write_shortcuts(&target) {
            return fail(silent, &target, &e);
        }
        if let Err(e) = register_uninstall(&target) {
            return fail(silent, &target, &e);
        }
        log.push("shortcuts + uninstall registry written".to_string());
    }

    if silent {
        write_log(&log);
        return 0;
    }

    let done = format!(
        "安装完成！已释放 {file_count} 个文件。\n\n\
         桌面与开始菜单已创建「{SHORTCUT_NAME}」快捷方式，\n\
         以后双击快捷方式即可启动。\n\n\
         是否立即启动 evorule？"
    );
    if ffi::ask_yes_no(&done) {
        launch(&target);
    }
    0
}

fn extract(target: &std::path::Path, log: &mut Vec<String>) -> Result<usize, String> {
    let mut count = 0usize;
    for (rel, bytes) in PAYLOAD {
        let dest = target.join(rel.replace('/', "\\"));
        let parent = match dest.parent() {
            Some(p) => p,
            None => return Err(format!("无效的载荷路径: {rel}")),
        };
        std::fs::create_dir_all(parent).map_err(|e| format!("创建目录失败 {}: {e}", parent.display()))?;
        std::fs::write(&dest, bytes).map_err(|e| friendly_io(&dest, &e))?;
        count += 1;
    }
    log.push(format!("extracted {count} files"));
    Ok(count)
}

fn friendly_io(path: &std::path::Path, e: &std::io::Error) -> String {
    let locked = matches!(e.raw_os_error(), Some(5) | Some(32));
    if locked {
        format!(
            "文件被占用：{}\n\n可能 evorule 正在运行。请先关闭任务栏上的两个最小化窗口（evorule-server 与 evorule-rule），再重新运行安装。",
            path.display()
        )
    } else {
        format!("写入文件失败 {}: {e}", path.display())
    }
}

fn copy_self(target: &std::path::Path) -> Result<(), String> {
    let self_path = std::env::current_exe().map_err(|e| format!("定位自身失败: {e}"))?;
    let dest = target.join("evorule-setup.exe");
    if self_path != dest {
        std::fs::copy(&self_path, &dest).map_err(|e| format!("复制卸载器失败: {e}"))?;
    }
    Ok(())
}

fn default_install_dir() -> std::path::PathBuf {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| "C:\\Program Files".to_string());
    std::path::Path::new(&base).join(APP_DIR_NAME)
}

// ---------- 快捷方式 / 注册表 ----------

fn write_shortcuts(target: &std::path::Path) -> Result<(), String> {
    let desktop = ffi::desktop_dir()?;
    let appdata = std::env::var("APPDATA").map_err(|_| "APPDATA 环境变量缺失".to_string())?;
    let start_menu = std::path::Path::new(&appdata)
        .join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs");
    let dir = target.to_string_lossy().replace('\'', "''");
    let bat = target.join("start-evorule.bat").to_string_lossy().replace('\'', "''");
    let icon = target.join("evorule-server.exe").to_string_lossy().replace('\'', "''");
    let mut ps = String::from("$ws = New-Object -ComObject WScript.Shell; ");
    for lnk_dir in [desktop, start_menu.to_string_lossy().to_string()] {
        ps.push_str(&format!(
            "$s = $ws.CreateShortcut('{lnk_dir}\\{SHORTCUT_NAME}.lnk'); \
             $s.TargetPath = '{bat}'; $s.WorkingDirectory = '{dir}'; \
             $s.IconLocation = '{icon},0'; $s.Save(); "
        ));
    }
    run_quiet("powershell", &["-NoProfile", "-WindowStyle", "Hidden", "-Command", &ps])
        .map_err(|e| format!("创建快捷方式失败: {e}"))
}

fn register_uninstall(target: &std::path::Path) -> Result<(), String> {
    let setup = target.join("evorule-setup.exe");
    let values: Vec<(&str, String)> = vec![
        ("DisplayName", SHORTCUT_NAME.to_string()),
        ("DisplayVersion", VERSION.to_string()),
        ("Publisher", "EvoRule Project".to_string()),
        ("InstallLocation", target.to_string_lossy().to_string()),
        ("UninstallString", format!("\"{}\" --uninstall", setup.display())),
        ("DisplayIcon", format!("{},0", setup.display())),
        ("NoModify", "1".to_string()),
        ("NoRepair", "1".to_string()),
    ];
    for (name, value) in values {
        run_quiet(
            "reg",
            &["add", REG_KEY, "/v", name, "/d", &value, "/f"],
        )
        .map_err(|e| format!("注册卸载信息失败({name}): {e}"))?;
    }
    Ok(())
}

fn run_quiet(program: &str, args: &[&str]) -> Result<(), String> {
    let mut cmd = std::process::Command::new(program);
    cmd.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let out = cmd.output().map_err(|e| format!("{program}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program} 退出码 {:?}: {}",
            out.status.code(),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

// ---------- 启动 / 卸载 ----------

fn launch(target: &std::path::Path) {
    let bat = target.join("start-evorule.bat");
    let one = format!("start \"\" /min \"{}\"", bat.display());
    let _ = run_quiet("cmd", &["/C", &one]);
}

fn uninstall(silent: bool) -> i32 {
    let dir = match std::env::current_exe() {
        Ok(p) => match p.parent() {
            Some(d) => d.to_path_buf(),
            None => default_install_dir(),
        },
        Err(_) => default_install_dir(),
    };
    if !silent {
        let confirm = format!(
            "确定要卸载 evorule 体验版吗？\n\n\
             将删除：\n· 程序文件（{}）\n· 桌面与开始菜单快捷方式\n· 你的全部数据（含已创建的规则与审计记录）\n\n\
             此操作不可恢复。",
            dir.display()
        );
        if !ffi::ask_ok_cancel(&confirm) {
            return 0;
        }
    }
    let _ = run_quiet("taskkill", &["/IM", "evorule-server.exe", "/F"]);
    let _ = run_quiet("taskkill", &["/IM", "evorule-rule-serve.exe", "/F"]);
    let _ = run_quiet("reg", &["delete", REG_KEY, "/f"]);
    if let Ok(desktop) = ffi::desktop_dir() {
        let _ = std::fs::remove_file(
            std::path::Path::new(&desktop).join(format!("{SHORTCUT_NAME}.lnk")),
        );
    }
    if let Ok(appdata) = std::env::var("APPDATA") {
        let _ = std::fs::remove_file(
            std::path::Path::new(&appdata)
                .join("Microsoft")
                .join("Windows")
                .join("Start Menu")
                .join("Programs")
                .join(format!("{SHORTCUT_NAME}.lnk")),
        );
    }
    // 自删除：延迟数秒后整目录删除（等本进程退出）
    let dir_str = dir.to_string_lossy().to_string();
    let script = format!("ping -n 3 127.0.0.1 >nul & rd /s /q \"{dir_str}\"");
    let _ = run_quiet("cmd", &["/C", &script]);
    if !silent {
        ffi::info("evorule 体验版已卸载。");
    }
    0
}

// ---------- 失败 / 日志 ----------

fn fail(silent: bool, target: &std::path::Path, msg: &str) -> i32 {
    if silent {
        let log = format!("FAILED: {msg}\n");
        let _ = std::fs::write(std::env::temp_dir().join("evorule-setup.log"), log);
        return 1;
    }
    ffi::error(&format!("安装失败：{msg}\n\n安装目标：{}", target.display()));
    1
}

fn write_log(log: &[String]) {
    let text = log.join("\n") + "\n";
    let _ = std::fs::write(std::env::temp_dir().join("evorule-setup.log"), text);
}
