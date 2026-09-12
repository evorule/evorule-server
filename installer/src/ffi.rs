//! Win32 FFI（零第三方依赖）：MessageBoxW + SHGetKnownFolderPath。

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
