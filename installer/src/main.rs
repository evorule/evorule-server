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

// AI 助手配置预填默认值(与 plugins/ai-plugin/config.example.json 同源,UV-179 批次D)
const DEFAULT_LLM_ENDPOINT: &str = "https://api.minimaxi.com/v1";
const DEFAULT_LLM_MODEL: &str = "MiniMax-Text-01";

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
        // UV-179 批次E:确认页升格「许可与隐私」语义——开源声明+数据本机+同意按钮
        let confirm = format!(
            "evorule 体验版 v{VERSION} 安装向导\n\n\
             将安装到：\n{target_str}\n\n\
             · 开源软件（AGPL-3.0 许可），无需管理员权限\n\
             · 一切数据只保存在你自己的电脑上\n\
             · 升级安装会保留你的规则与数据\n\n\
             点击「确定」即表示你同意许可条款并开始安装。"
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

    // 4. 可选:AI 助手配置（UV-179 批次D;跳过则零写入,不影响任何功能）
    //    Key 仅落本机 plugins\ai-plugin\ai-plugin.json（用户目录隔离,README 有保管警告）;
    //    写配置成功才翻转 plugin_manifest.json 的 enabled 开关。
    let mut ai_note = String::from("AI 助手未配置（可稍后在浏览器「设置」中配置）");
    if !silent && ffi::ask_yes_no(&format!(
        "是否现在配置 AI 助手？（可选，跳过完全不影响使用）\n\n\
         需要一个 OpenAI 兼容的 LLM API Key。Key 只保存在你电脑上的\n\
         plugins\\ai-plugin\\ai-plugin.json 文件中，不会上传给任何第三方。\n\n\
         没有也没关系：稍后可在浏览器「设置 → LLM 配置」中随时配置。"
    )) {
        match ffi::ask_llm_config(DEFAULT_LLM_ENDPOINT, DEFAULT_LLM_MODEL) {
            Some(cfg) => match write_ai_plugin_config(&target, &cfg) {
                Ok(()) => {
                    match enable_ai_plugin_manifest(&target) {
                        Ok(()) => {
                            log.push("ai-plugin configured + enabled".to_string());
                            ai_note = "AI 助手已配置（启动后自动生效）".to_string();
                        }
                        Err(e) => {
                            log.push(format!("ai-plugin manifest enable failed: {e}"));
                            ai_note = "AI 助手凭据已写入,但插件开关未打开(见 README 手工启用)".to_string();
                        }
                    }
                }
                Err(e) => {
                    log.push(format!("ai-plugin config write failed: {e}"));
                    ffi::error(&format!(
                        "AI 助手配置写入失败：{e}\n\n将继续安装（不影响其他功能）。"
                    ));
                }
            },
            None => {
                log.push("ai-plugin skipped".to_string());
            }
        }
    }

    if silent {
        write_log(&log);
        return 0;
    }

    // UV-179 批次E:完成页指路浏览器内引导(/welcome 口径一致)
    let done = format!(
        "安装完成！已释放 {file_count} 个文件。{ai_note}\n\n\
         桌面与开始菜单已创建「{SHORTCUT_NAME}」快捷方式，\n\
         以后双击快捷方式即可启动。\n\n\
         启动后浏览器会自动打开，按页面内的引导（约 3 分钟）\n\
         完成登录与治理连接即可开始使用。\n\n\
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

// ---------- AI 助手配置写入（UV-179 批次D） ----------

/// JSON 字符串转义（零依赖;覆盖控制符/引号/反斜杠,凭据常见字符集足够）。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// 写 plugins\ai-plugin\ai-plugin.json（字段与 config.example.json 同构;
/// llm_api_key 明文落本机 = 批次B 既有「文件 Key 兼容形态」,用户目录天然隔离）。
fn write_ai_plugin_config(
    target: &std::path::Path,
    cfg: &ffi::LlmConfigInput,
) -> Result<(), String> {
    let dir = target.join("plugins").join("ai-plugin");
    std::fs::create_dir_all(&dir).map_err(|e| format!("创建目录失败 {}: {e}", dir.display()))?;
    let content = format!(
        "{{\n  \"listen_addr\": \"127.0.0.1:9130\",\n  \
         \"server_base_url\": \"http://127.0.0.1:18080\",\n  \
         \"server_auth_token\": \"\",\n  \
         \"llm_endpoint\": \"{}\",\n  \
         \"llm_api_key\": \"{}\",\n  \
         \"llm_model\": \"{}\",\n  \
         \"llm_temperature\": 0.2,\n  \
         \"llm_timeout_ms\": 60000\n}}\n",
        json_escape(cfg.endpoint.trim()),
        json_escape(cfg.key.trim()),
        json_escape(cfg.model.trim())
    );
    let path = dir.join("ai-plugin.json");
    std::fs::write(&path, content).map_err(|e| format!("写入失败 {}: {e}", path.display()))
}

/// 翻转 plugin_manifest.json 中 ai-plugin 的 enabled 开关。
/// 载荷内该文件形态固定（单 ai-plugin 条目,"enabled": false）,做精确子串替换;
/// 防御:若形态漂移（找不到目标子串）则报错交人工处理,不做盲目改写。
fn enable_ai_plugin_manifest(target: &std::path::Path) -> Result<(), String> {
    let path = target.join("plugin_manifest.json");
    let text = std::fs::read_to_string(&path)
        .map_err(|e| format!("读取失败 {}: {e}", path.display()))?;
    if text.contains("\"enabled\": true") {
        return Ok(()); // 已启用,幂等
    }
    if !text.contains("\"enabled\": false") {
        return Err("plugin_manifest.json 形态与预期不符(未找到 enabled:false)".to_string());
    }
    let updated = text.replacen("\"enabled\": false", "\"enabled\": true", 1);
    std::fs::write(&path, updated).map_err(|e| format!("写入失败 {}: {e}", path.display()))
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

// ---------- 单测(UV-179 批次D:GUI 对话框外的纯文件逻辑) ----------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn json_escape_covers_quotes_backslash_and_controls() {
        assert_eq!(json_escape("plain"), "plain");
        assert_eq!(json_escape("a\"b"), "a\\\"b");
        assert_eq!(json_escape("a\\b"), "a\\\\b");
        assert_eq!(json_escape("a\nb"), "a\\nb");
        assert_eq!(json_escape("a\tb"), "a\\tb");
        assert_eq!(json_escape("\u{1}"), "\\u0001");
    }

    #[test]
    fn write_ai_plugin_config_wears_batch_b_file_key_shape() {
        let dir =
            std::env::temp_dir().join(format!("evorule-setup-test-{}-cfg", std::process::id()));
        let cfg = ffi::LlmConfigInput {
            endpoint: "https://api.minimaxi.com/v1".to_string(),
            model: "MiniMax-Text-01".to_string(),
            key: "k\"ey\\with-special".to_string(),
        };
        write_ai_plugin_config(&dir, &cfg).expect("write ai-plugin.json");
        let text =
            std::fs::read_to_string(dir.join("plugins").join("ai-plugin").join("ai-plugin.json"))
                .expect("read back");
        // 字段与 config.example.json 同构;Key 明文落本机 = 批次B 既有兼容形态
        assert!(text.contains("\"listen_addr\": \"127.0.0.1:9130\""));
        assert!(text.contains("\"server_base_url\": \"http://127.0.0.1:18080\""));
        assert!(text.contains("\"llm_endpoint\": \"https://api.minimaxi.com/v1\""));
        assert!(text.contains("\"llm_api_key\": \"k\\\"ey\\\\with-special\""));
        assert!(text.contains("\"llm_model\": \"MiniMax-Text-01\""));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn enable_manifest_is_idempotent_and_detects_shape_drift() {
        let dir =
            std::env::temp_dir().join(format!("evorule-setup-test-{}-manifest", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap_or_else(|e| panic!("create dir: {e}"));
        let path = dir.join("plugin_manifest.json");
        // 1) 正常翻转
        std::fs::write(&path, r#"{ "plugins": { "ai-plugin": { "enabled": false } } }"#)
            .unwrap_or_else(|e| panic!("seed manifest: {e}"));
        enable_ai_plugin_manifest(&dir).expect("enable");
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read: {e}"));
        assert!(text.contains("\"enabled\": true"));
        // 2) 幂等(已启用再跑不报错不改写)
        enable_ai_plugin_manifest(&dir).expect("idempotent");
        // 3) 形态漂移 → 报错交人工,不盲目改写
        std::fs::write(&path, r#"{ "plugins": { "ai-plugin": { "on": false } } }"#)
            .unwrap_or_else(|e| panic!("drift manifest: {e}"));
        assert!(enable_ai_plugin_manifest(&dir).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
