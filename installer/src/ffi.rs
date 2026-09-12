//! Win32 FFI（零第三方依赖）：MessageBoxW + SHGetKnownFolderPath + 自绘输入对话框。
//!
//! 输入对话框（ask_llm_config）为批次D（UV-179）新增：三条目表单
//! （LLM 端点/模型名/API Key），Key 用 ES_PASSWORD 掩码显示；
//! 零第三方依赖，控件全部 CreateWindowExW 手工布局。

#![allow(non_snake_case)]

/// Windows GUID 结构（按内存布局对齐 win32 定义）。
#[repr(C)]
pub struct Guid {
    a: u32,
    b: u16,
    c: u16,
    d: [u8; 8],
}

/// FOLDERID_Desktop {B4BFCC3A-DB2C-424C-B029-7FE99A87C641}
const FOLDERID_DESKTOP: Guid = Guid {
    a: 0xB4BFCC3A,
    b: 0xDB2C,
    c: 0x424C,
    d: [0xB0, 0x29, 0x7F, 0xE9, 0x9A, 0x87, 0xC6, 0x41],
};

const MB_OK: u32 = 0x0000_0000;
const MB_OKCANCEL: u32 = 0x0000_0001;
const MB_YESNO: u32 = 0x0000_0004;
const MB_ICONINFORMATION: u32 = 0x0000_0040;
const MB_ICONERROR: u32 = 0x0000_0010;
const IDOK: i32 = 1;
const IDYES: i32 = 6;

#[link(name = "user32")]
extern "system" {
    fn MessageBoxW(hWnd: isize, lpText: *const u16, lpCaption: *const u16, uType: u32) -> i32;
}

#[link(name = "shell32")]
extern "system" {
    fn SHGetKnownFolderPath(
        rfid: *const Guid,
        dwFlags: u32,
        hToken: isize,
        ppszPath: *mut *mut u16,
    ) -> i32;
}

#[link(name = "ole32")]
extern "system" {
    fn CoTaskMemFree(pv: *mut u16);
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn box_(text: &str, flags: u32) -> i32 {
    // SAFETY: 两个 wide 字符串临时值在本语句内存活，指针有效；参数均为只读。
    unsafe {
        MessageBoxW(
            0,
            wide(text).as_ptr(),
            wide("evorule 体验版").as_ptr(),
            flags,
        )
    }
}

/// 信息提示（确定）。
pub fn info(text: &str) {
    let _ = box_(text, MB_OK | MB_ICONINFORMATION);
}

/// 确认框（确定/取消），返回是否选择了确定。
pub fn ask_ok_cancel(text: &str) -> bool {
    box_(text, MB_OKCANCEL | MB_ICONINFORMATION) == IDOK
}

/// 询问框（是/否），返回是否选择了是。
pub fn ask_yes_no(text: &str) -> bool {
    box_(text, MB_YESNO | MB_ICONINFORMATION) == IDYES
}

/// 错误提示（确定）。
pub fn error(text: &str) {
    let _ = box_(text, MB_OK | MB_ICONERROR);
}

/// 解析当前用户桌面目录（无需管理员权限，HKCU 语境）。
pub fn desktop_dir() -> Result<String, String> {
    let mut ptr: *mut u16 = std::ptr::null_mut();
    // SAFETY: FOLDERID_DESKTOP 为常量 GUID；输出指针由 CoTaskMemFree 释放。
    let hr = unsafe { SHGetKnownFolderPath(&FOLDERID_DESKTOP, 0, 0, &mut ptr) };
    if hr != 0 || ptr.is_null() {
        return Err(format!("SHGetKnownFolderPath 失败 (hr={hr})"));
    }
    let mut len: usize = 0;
    // SAFETY: 系统返回 NUL 结尾的宽字符串，逐字符扫描终止符。
    unsafe {
        while *ptr.add(len) != 0 {
            len += 1;
        }
    }
    // SAFETY: len 为扫描所得长度，切片范围合法。
    let slice = unsafe { std::slice::from_raw_parts(ptr, len) };
    let out = String::from_utf16_lossy(slice);
    // SAFETY: 分配方为 CoTaskMemAlloc，须以 CoTaskMemFree 归还。
    unsafe { CoTaskMemFree(ptr) };
    Ok(out)
}

// ============================================================
// 自绘输入对话框（UV-179 批次D：AI 助手可选配置）
// ============================================================

use std::cell::RefCell;

const WS_OVERLAPPED: u32 = 0x0000_0000;
const WS_CAPTION: u32 = 0x00C0_0000;
const WS_SYSMENU: u32 = 0x0008_0000;
const WS_VISIBLE: u32 = 0x1000_0000;
const WS_CHILD: u32 = 0x4000_0000;
const WS_TABSTOP: u32 = 0x0001_0000;
const WS_GROUP: u32 = 0x0002_0000;
const ES_AUTOHSCROLL: u32 = 0x0080;
const ES_PASSWORD: u32 = 0x0020;
const BS_DEFPUSHBUTTON: u32 = 0x0001;
const BS_PUSHBUTTON: u32 = 0x0000;
const SS_LEFT: u32 = 0x0000_0000;
const SW_SHOW: i32 = 5;

const WM_SETFONT: u32 = 0x0030;
const WM_COMMAND: u32 = 0x0111;
const WM_CLOSE: u32 = 0x0010;
const WM_DESTROY: u32 = 0x0002;
const WM_NCDESTROY: u32 = 0x0082;

const IDCANCEL: isize = 2;
const ID_SAVE: isize = 201;
const ID_EDIT_ENDPOINT: isize = 101;
const ID_EDIT_MODEL: isize = 102;
const ID_EDIT_KEY: isize = 103;

/// 对话框收集结果（Key 已掩码输入，仅在本进程内存活，不写日志）。
#[derive(Clone)]
pub struct LlmConfigInput {
    pub endpoint: String,
    pub model: String,
    pub key: String,
}

thread_local! {
    /// 单实例对话框的控件句柄与结果（安装器 UI 单线程，thread_local 安全）。
    static DLG: RefCell<DlgState> = RefCell::new(DlgState {
        edit_endpoint: 0,
        edit_model: 0,
        edit_key: 0,
        result: None,
    });
}

struct DlgState {
    edit_endpoint: isize,
    edit_model: isize,
    edit_key: isize,
    result: Option<LlmConfigInput>,
}

#[link(name = "user32")]
extern "system" {
    fn RegisterClassW(lpWndClass: *const WndClassW) -> u16;
    fn CreateWindowExW(
        dwExStyle: u32,
        lpClassName: *const u16,
        lpWindowName: *const u16,
        dwStyle: u32,
        x: i32,
        y: i32,
        nWidth: i32,
        nHeight: i32,
        hWndParent: isize,
        hMenu: isize,
        hInstance: isize,
        lpParam: *const u8,
    ) -> isize;
    fn DefWindowProcW(hWnd: isize, msg: u32, wParam: usize, lParam: isize) -> isize;
    fn ShowWindow(hWnd: isize, nCmdShow: i32) -> i32;
    fn DestroyWindow(hWnd: isize) -> i32;
    fn GetMessageW(lpMsg: *mut Msg, hWnd: isize, min: u32, max: u32) -> i32;
    fn TranslateMessage(lpMsg: *const Msg) -> i32;
    fn DispatchMessageW(lpMsg: *const Msg) -> isize;
    fn PostQuitMessage(nExitCode: i32);
    fn GetWindowTextW(hWnd: isize, lpString: *mut u16, nMaxCount: i32) -> i32;
    fn SetFocus(hWnd: isize) -> isize;
    fn SendMessageW(hWnd: isize, msg: u32, wParam: usize, lParam: isize) -> isize;
    fn GetModuleHandleW(lpModuleName: *const u16) -> isize;
}

#[link(name = "gdi32")]
extern "system" {
    fn GetStockObject(fnObject: i32) -> isize;
}

const DEFAULT_GUI_FONT: i32 = 17;

#[repr(C)]
struct WndClassW {
    style: u32,
    lpfnWndProc: unsafe extern "system" fn(isize, u32, usize, isize) -> isize,
    cbClsExtra: i32,
    cbWndExtra: i32,
    hInstance: isize,
    hIcon: isize,
    hCursor: isize,
    hbrBackground: isize,
    lpszMenuName: *const u16,
    lpszClassName: *const u16,
}

#[repr(C)]
struct Msg {
    hwnd: isize,
    message: u32,
    wParam: usize,
    lParam: isize,
    time: u32,
    pt_x: i32,
    pt_y: i32,
}

const COLOR_BTNFACE: isize = 15;

/// 创建子控件的小工具（都带默认 GUI 字体）。
unsafe fn create_control(
    class: *const u16,
    text: *const u16,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    parent: isize,
    id: i32,
    font: isize,
) -> isize {
    let hwnd = CreateWindowExW(
        0,
        class,
        text,
        WS_CHILD | WS_VISIBLE | WS_TABSTOP | style,
        x,
        y,
        w,
        h,
        parent,
        id as isize,
        0,
        std::ptr::null(),
    );
    if hwnd != 0 {
        SendMessageW(hwnd, WM_SETFONT, font as usize, 1);
    }
    hwnd
}

/// 宽字符窗口类/控件名常量。
const CLASS_NAME: &[u16] = &[b'E' as u16, b'v' as u16, b'o' as u16, b'S' as u16, b'e' as u16, b't' as u16, b'u' as u16, b'p' as u16, b'L' as u16, b'l' as u16, b'm' as u16, b'D' as u16, b'l' as u16, b'g' as u16, 0];
const STATIC_CLASS: &[u16] = &[b'S' as u16, b't' as u16, b'a' as u16, b't' as u16, b'i' as u16, b'c' as u16, 0];
const EDIT_CLASS: &[u16] = &[b'E' as u16, b'd' as u16, b'i' as u16, b't' as u16, 0];
const BUTTON_CLASS: &[u16] = &[b'B' as u16, b'u' as u16, b't' as u16, b't' as u16, b'o' as u16, b'n' as u16, 0];

unsafe extern "system" fn dlg_wnd_proc(hwnd: isize, msg: u32, wParam: usize, lParam: isize) -> isize {
    match msg {
        WM_COMMAND => {
            let id = (wParam & 0xFFFF) as isize;
            match id {
                ID_SAVE => {
                    let endpoint = DLG.with(|s| read_ctrl(s.borrow().edit_endpoint));
                    let model = DLG.with(|s| read_ctrl(s.borrow().edit_model));
                    let key = DLG.with(|s| read_ctrl(s.borrow().edit_key));
                    if key.trim().is_empty() {
                        box_(
                            "API Key 不能为空。\n\n若暂无 Key,请点「跳过」——不影响 evorule 其他功能。",
                            MB_OK | MB_ICONINFORMATION,
                        );
                        return 0;
                    }
                    DLG.with(|s| {
                        s.borrow_mut().result = Some(LlmConfigInput {
                            endpoint,
                            model,
                            key,
                        })
                    });
                    DestroyWindow(hwnd);
                    return 0;
                }
                IDCANCEL => {
                    DestroyWindow(hwnd);
                    return 0;
                }
                _ => {}
            }
            DefWindowProcW(hwnd, msg, wParam, lParam)
        }
        WM_CLOSE => {
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY | WM_NCDESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wParam, lParam),
    }
}

/// 读取控件文本（GetWindowTextW；缓冲不足按实际返回截断）。
unsafe fn read_ctrl(hwnd: isize) -> String {
    if hwnd == 0 {
        return String::new();
    }
    let len = GetWindowTextW(hwnd, std::ptr::null_mut(), 0);
    let mut buf = vec![0u16; (len + 1) as usize];
    GetWindowTextW(hwnd, buf.as_mut_ptr(), buf.len() as i32);
    let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..end]).trim().to_string()
}

///
/// 三条目 LLM 配置对话框（API Key 掩码）。
/// 返回 Some = 用户点了「保存并启用」；None = 跳过/关闭。
/// 预填值由调用方传入（分发默认 = config.example 同源）。
///
pub fn ask_llm_config(endpoint_default: &str, model_default: &str) -> Option<LlmConfigInput> {
    DLG.with(|s| {
        *s.borrow_mut() = DlgState {
            edit_endpoint: 0,
            edit_model: 0,
            edit_key: 0,
            result: None,
        }
    });

    unsafe {
        let hinstance = GetModuleHandleW(std::ptr::null());
        let cls = WndClassW {
            style: 0,
            lpfnWndProc: dlg_wnd_proc,
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: 0,
            hCursor: 0,
            hbrBackground: COLOR_BTNFACE + 1, // HBRUSH 常量语义
            lpszMenuName: std::ptr::null(),
            lpszClassName: CLASS_NAME.as_ptr(),
        };
        if RegisterClassW(&cls) == 0 {
            return None; // 类已注册/注册失败均走 None（跳过），安装不被阻塞
        }

        const W: i32 = 400;
        const H: i32 = 260;
        let style = WS_CAPTION | WS_SYSMENU | WS_OVERLAPPED;
        let hwnd = CreateWindowExW(
            0,
            CLASS_NAME.as_ptr(),
            wide("evorule 体验版 · 配置 AI 助手（可选）").as_ptr(),
            style,
            0x8000_0000u32 as i32, // CW_USEDEFAULT
            0,
            W,
            H,
            0,
            0,
            hinstance,
            std::ptr::null(),
        );
        if hwnd == 0 {
            return None;
        }
        let font = GetStockObject(DEFAULT_GUI_FONT);

        // 控件布局（客户区手工坐标）
        DLG.with(|s| {
            let mut st = s.borrow_mut();
            create_control(STATIC_CLASS.as_ptr(), wide("LLM API 端点 (OpenAI 兼容)").as_ptr(), SS_LEFT, 20, 20, 340, 20, hwnd, 301, font);
            st.edit_endpoint = create_control(EDIT_CLASS.as_ptr(), wide(endpoint_default).as_ptr(), ES_AUTOHSCROLL | WS_GROUP, 20, 44, 340, 24, hwnd, ID_EDIT_ENDPOINT as i32, font);
            create_control(STATIC_CLASS.as_ptr(), wide("模型名").as_ptr(), SS_LEFT, 20, 82, 340, 20, hwnd, 302, font);
            st.edit_model = create_control(EDIT_CLASS.as_ptr(), wide(model_default).as_ptr(), ES_AUTOHSCROLL, 20, 106, 340, 24, hwnd, ID_EDIT_MODEL as i32, font);
            create_control(STATIC_CLASS.as_ptr(), wide("API Key（仅保存在你电脑上的本机文件,不会上传）").as_ptr(), SS_LEFT, 20, 144, 340, 20, hwnd, 303, font);
            st.edit_key = create_control(EDIT_CLASS.as_ptr(), wide("").as_ptr(), ES_AUTOHSCROLL | ES_PASSWORD, 20, 168, 340, 24, hwnd, ID_EDIT_KEY as i32, font);
            create_control(BUTTON_CLASS.as_ptr(), wide("跳过").as_ptr(), BS_PUSHBUTTON, 200, 210, 84, 30, hwnd, IDCANCEL as i32, font);
            create_control(BUTTON_CLASS.as_ptr(), wide("保存并启用").as_ptr(), BS_DEFPUSHBUTTON, 292, 210, 94, 30, hwnd, ID_SAVE as i32, font);
        });

        ShowWindow(hwnd, SW_SHOW);
        SetFocus(DLG.with(|s| s.borrow().edit_key));

        // 消息循环（对话框关闭即退出）
        let mut msg = Msg { hwnd: 0, message: 0, wParam: 0, lParam: 0, time: 0, pt_x: 0, pt_y: 0 };
        while GetMessageW(&mut msg, 0, 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        DLG.with(|s| s.borrow().result.clone())
    }
}
