// Windows 查詢：只讀系統與程式的記憶體計數器（跟工作管理員同一套），唯一會動到別的程式的是 kill。

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::mem::{size_of, zeroed};
use std::sync::OnceLock;
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HWND, INVALID_HANDLE_VALUE, LPARAM, LRESULT, SYSTEMTIME, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateCompatibleDC, CreateRoundRectRgn, DeleteDC, DeleteObject, GetDIBits, GetObjectW, SetWindowRgn, BITMAP,
    BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetPerformanceInfo, K32GetProcessMemoryInfo, PERFORMANCE_INFORMATION, PROCESS_MEMORY_COUNTERS,
};
use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::Threading::{
    GetActiveProcessorCount, GetCurrentProcessId, GetProcessTimes, GetSystemTimes, OpenProcess, QueryFullProcessImageNameW,
    TerminateProcess, ALL_PROCESSOR_GROUPS, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE,
};
use windows_sys::Win32::Foundation::POINT;
use windows_sys::Win32::UI::Shell::ExtractIconExW;
use windows_sys::Win32::UI::WindowsAndMessaging::{
    DestroyIcon, GetCursorPos, GetIconInfo, GetLayeredWindowAttributes, GetWindowLongPtrW, SetLayeredWindowAttributes, SetWindowLongPtrW, GWL_EXSTYLE, ICONINFO,
    LWA_ALPHA, WS_EX_LAYERED, WS_EX_TRANSPARENT,
};

const GB: f64 = 1024.0 * 1024.0 * 1024.0;

// 不准加進偵測目標的程式（小寫）；session 0 的一律也算系統程式。
const SYSTEM_EXES: &[&str] = &[
    "system", "registry", "memcompression", "smss.exe", "csrss.exe", "wininit.exe", "winlogon.exe",
    "services.exe", "lsass.exe", "lsaiso.exe", "svchost.exe", "dwm.exe", "explorer.exe", "fontdrvhost.exe",
    "sihost.exe", "ctfmon.exe", "taskhostw.exe", "runtimebroker.exe", "conhost.exe", "dllhost.exe",
    "searchhost.exe", "searchapp.exe", "startmenuexperiencehost.exe", "shellexperiencehost.exe",
    "textinputhost.exe", "applicationframehost.exe", "systemsettings.exe", "smartscreen.exe",
    "msmpeng.exe", "nissrv.exe", "securityhealthservice.exe", "securityhealthsystray.exe", "audiodg.exe",
    "spoolsv.exe", "wudfhost.exe", "userinit.exe", "taskmgr.exe",
];

pub struct Commit {
    pub total_gb: f64,
    pub limit_gb: f64,
    pub phys_used_gb: f64,
    pub phys_total_gb: f64,
}

pub struct Proc {
    pub pid: u32,
    pub exe: String,
    pub path: String,
    pub session: u32,
    pub ws_gb: f64,
    pub commit_gb: f64,
    // 開機以來用掉的 CPU 時間（核心加使用者，100 奈秒為單位）；兩次相減才是用量。
    pub cpu: u64,
}

// PROCESS_MEMORY_COUNTERS_EX2（Windows 10 1809 起），多了私人工作集，也就是工作管理員「記憶體」欄。
#[repr(C)]
#[allow(non_snake_case)]
struct PmcEx2 {
    cb: u32,
    PageFaultCount: u32,
    PeakWorkingSetSize: usize,
    WorkingSetSize: usize,
    QuotaPeakPagedPoolUsage: usize,
    QuotaPagedPoolUsage: usize,
    QuotaPeakNonPagedPoolUsage: usize,
    QuotaNonPagedPoolUsage: usize,
    PagefileUsage: usize,
    PeakPagefileUsage: usize,
    PrivateUsage: usize,
    PrivateWorkingSetSize: usize,
    SharedCommitUsage: u64,
}

pub fn commit() -> Option<Commit> {
    unsafe {
        let mut p: PERFORMANCE_INFORMATION = zeroed();
        p.cb = size_of::<PERFORMANCE_INFORMATION>() as u32;
        if K32GetPerformanceInfo(&mut p, p.cb) == 0 {
            return None;
        }
        let page = p.PageSize as f64;
        Some(Commit {
            total_gb: p.CommitTotal as f64 * page / GB,
            limit_gb: p.CommitLimit as f64 * page / GB,
            phys_used_gb: (p.PhysicalTotal - p.PhysicalAvailable) as f64 * page / GB,
            phys_total_gb: p.PhysicalTotal as f64 * page / GB,
        })
    }
}

pub fn processes() -> Vec<Proc> {
    let mut out = Vec::new();
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
        if snap == INVALID_HANDLE_VALUE {
            return out;
        }
        let mut e: PROCESSENTRY32W = zeroed();
        e.dwSize = size_of::<PROCESSENTRY32W>() as u32;
        let mut ok = Process32FirstW(snap, &mut e) != 0;
        while ok {
            let pid = e.th32ProcessID;
            if pid != 0 {
                if let Some((ws_gb, commit_gb, path, cpu)) = query(pid) {
                    let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                    let mut session = 0u32;
                    ProcessIdToSessionId(pid, &mut session);
                    out.push(Proc { pid, exe: String::from_utf16_lossy(&e.szExeFile[..len]), path, session, ws_gb, commit_gb, cpu });
                }
            }
            ok = Process32NextW(snap, &mut e) != 0;
        }
        CloseHandle(snap);
    }
    out
}

// 只要「查詢有限資訊」權限，不讀對方記憶體內容。
unsafe fn query(pid: u32) -> Option<(f64, f64, String, u64)> {
    let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
    if h.is_null() {
        return None;
    }
    let mut m: PmcEx2 = zeroed();
    m.cb = size_of::<PmcEx2>() as u32;
    let mem = if K32GetProcessMemoryInfo(h, &mut m as *mut PmcEx2 as *mut PROCESS_MEMORY_COUNTERS, m.cb) != 0 {
        let ws = if m.PrivateWorkingSetSize > 0 { m.PrivateWorkingSetSize } else { m.WorkingSetSize };
        Some((ws as f64 / GB, m.PrivateUsage as f64 / GB))
    } else {
        None
    };
    let mut buf = [0u16; 1024];
    let mut n = buf.len() as u32;
    let path = if QueryFullProcessImageNameW(h, 0, buf.as_mut_ptr(), &mut n) != 0 {
        String::from_utf16_lossy(&buf[..n as usize])
    } else {
        String::new()
    };
    let (mut c, mut x, mut k, mut u): (FILETIME, FILETIME, FILETIME, FILETIME) = (zeroed(), zeroed(), zeroed(), zeroed());
    let cpu = if GetProcessTimes(h, &mut c, &mut x, &mut k, &mut u) != 0 { ft(k) + ft(u) } else { 0 };
    CloseHandle(h);
    mem.map(|(ws, cm)| (ws, cm, path, cpu))
}

fn ft(t: FILETIME) -> u64 {
    (t.dwHighDateTime as u64) << 32 | t.dwLowDateTime as u64
}

// 整台的（閒置, 核心, 使用者）累計時間；核心時間本身含閒置。
pub fn system_times() -> Option<(u64, u64, u64)> {
    unsafe {
        let (mut i, mut k, mut u): (FILETIME, FILETIME, FILETIME) = (zeroed(), zeroed(), zeroed());
        (GetSystemTimes(&mut i, &mut k, &mut u) != 0).then(|| (ft(i), ft(k), ft(u)))
    }
}

pub fn cpu_count() -> u32 {
    unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS).max(1) }
}

// HTTPS GET，回傳（狀態碼, 內容）；連不上或逾時回傳 None。只在使用者按「檢查更新」時用。
pub fn https_get(host: &str, path: &str) -> Option<(u32, String)> {
    use windows_sys::Win32::Networking::WinHttp::*;
    let w = |s: &str| -> Vec<u16> { s.encode_utf16().chain(std::iter::once(0)).collect() };
    let (agent, host, path, verb) = (w("MemGuard"), w(host), w(path), w("GET"));
    unsafe {
        let session = WinHttpOpen(agent.as_ptr(), WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY, std::ptr::null(), std::ptr::null(), 0);
        if session.is_null() {
            return None;
        }
        WinHttpSetTimeouts(session, 5000, 5000, 5000, 10000);
        let conn = WinHttpConnect(session, host.as_ptr(), INTERNET_DEFAULT_HTTPS_PORT, 0);
        let req = if conn.is_null() {
            std::ptr::null_mut()
        } else {
            WinHttpOpenRequest(conn, verb.as_ptr(), path.as_ptr(), std::ptr::null(), std::ptr::null(), std::ptr::null(), WINHTTP_FLAG_SECURE)
        };
        let mut out = None;
        if !req.is_null() && WinHttpSendRequest(req, std::ptr::null(), 0, std::ptr::null(), 0, 0, 0) != 0 && WinHttpReceiveResponse(req, std::ptr::null_mut()) != 0 {
            let (mut code, mut size) = (0u32, 4u32);
            WinHttpQueryHeaders(
                req,
                WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
                std::ptr::null(),
                &mut code as *mut u32 as *mut c_void,
                &mut size,
                std::ptr::null_mut(),
            );
            let mut body = Vec::new();
            let mut buf = [0u8; 8192];
            loop {
                let mut n = 0u32;
                if WinHttpReadData(req, buf.as_mut_ptr() as *mut c_void, buf.len() as u32, &mut n) == 0 || n == 0 {
                    break;
                }
                body.extend_from_slice(&buf[..n as usize]);
            }
            out = Some((code, String::from_utf8_lossy(&body).into_owned()));
        }
        for h in [req, conn, session] {
            if !h.is_null() {
                WinHttpCloseHandle(h);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    // 要連網，平常不跑：cargo test --release -- --ignored
    #[test]
    #[ignore]
    fn https_get_reads_github_release() {
        let (code, body) = super::https_get("api.github.com", "/repos/rust-lang/rust/releases/latest").expect("連不上");
        assert_eq!(code, 200);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        assert!(v["tag_name"].as_str().is_some());
    }
}

// 把文字放進剪貼簿（Unicode 文字格式）。
pub fn copy_text(s: &str) {
    use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
    use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
    const CF_UNICODETEXT: u32 = 13;
    let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return;
        }
        EmptyClipboard();
        let h = GlobalAlloc(GMEM_MOVEABLE, w.len() * 2);
        if !h.is_null() {
            let dst = GlobalLock(h) as *mut u16;
            if !dst.is_null() {
                std::ptr::copy_nonoverlapping(w.as_ptr(), dst, w.len());
                GlobalUnlock(h);
                SetClipboardData(CF_UNICODETEXT, h);
            }
        }
        CloseClipboard();
    }
}

pub fn cursor_pos() -> (i32, i32) {
    let mut pt: POINT = unsafe { zeroed() };
    unsafe {
        GetCursorPos(&mut pt);
    }
    (pt.x, pt.y)
}

pub fn kill(pid: u32) -> bool {
    unsafe {
        let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
        if h.is_null() {
            return false;
        }
        let ok = TerminateProcess(h, 1) != 0;
        CloseHandle(h);
        ok
    }
}

pub fn is_system(p: &Proc, me: u32) -> bool {
    p.pid == me || p.session == 0 || SYSTEM_EXES.contains(&p.exe.to_lowercase().as_str())
}

pub fn current_pid() -> u32 {
    unsafe { GetCurrentProcessId() }
}

// 警告音由程式自己產生，不跟系統音效配置走；聲音資料常駐，非同步播放才不會被提早釋放。
pub fn play_alert() {
    static WAV: OnceLock<Vec<u8>> = OnceLock::new();
    let wav = WAV.get_or_init(alert_wav);
    unsafe {
        PlaySoundW(wav.as_ptr() as *const u16, std::ptr::null_mut(), SND_MEMORY | SND_ASYNC | SND_NODEFAULT);
    }
}

// 兩聲往上的短音，頭尾淡入淡出免得爆音。
fn alert_wav() -> Vec<u8> {
    const RATE: u32 = 22050;
    let mut s: Vec<i16> = Vec::new();
    for (hz, ms, gap) in [(880.0f32, 110u32, 60u32), (1175.0, 160, 0)] {
        let n = RATE * ms / 1000;
        let fade = RATE as f32 * 0.008;
        for i in 0..n {
            let env = ((i.min(n - i) as f32) / fade).min(1.0);
            let v = (i as f32 / RATE as f32 * hz * std::f32::consts::TAU).sin() * env * 0.35;
            s.push((v * 32767.0) as i16);
        }
        s.extend(std::iter::repeat(0).take((RATE * gap / 1000) as usize));
    }
    let data = (s.len() * 2) as u32;
    let mut w = Vec::with_capacity(44 + data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&RATE.to_le_bytes());
    w.extend_from_slice(&(RATE * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data.to_le_bytes());
    for v in s {
        w.extend_from_slice(&v.to_le_bytes());
    }
    w
}

// 從硬碟上的執行檔讀第一個大圖示，回傳未預乘的 RGBA。
pub fn icon_rgba(path: &str) -> Option<(usize, usize, Vec<u8>)> {
    if path.is_empty() {
        return None;
    }
    let wide: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut large: *mut c_void = std::ptr::null_mut();
        if ExtractIconExW(wide.as_ptr(), 0, &mut large, std::ptr::null_mut(), 1) == 0 || large.is_null() {
            return None;
        }
        let mut info: ICONINFO = zeroed();
        let got = GetIconInfo(large, &mut info) != 0;
        let mut result = None;
        if got && !info.hbmColor.is_null() {
            let mut bm: BITMAP = zeroed();
            GetObjectW(info.hbmColor, size_of::<BITMAP>() as i32, &mut bm as *mut BITMAP as *mut c_void);
            let (w, h) = (bm.bmWidth as usize, bm.bmHeight as usize);
            if w > 0 && h > 0 {
                let mut bi: BITMAPINFO = zeroed();
                bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
                bi.bmiHeader.biWidth = w as i32;
                bi.bmiHeader.biHeight = -(h as i32);
                bi.bmiHeader.biPlanes = 1;
                bi.bmiHeader.biBitCount = 32;
                bi.bmiHeader.biCompression = BI_RGB;
                let mut px = vec![0u8; w * h * 4];
                let dc = CreateCompatibleDC(std::ptr::null_mut());
                let lines = GetDIBits(dc, info.hbmColor, 0, h as u32, px.as_mut_ptr() as *mut c_void, &mut bi, DIB_RGB_COLORS);
                DeleteDC(dc);
                if lines > 0 {
                    let no_alpha = px.chunks(4).all(|c| c[3] == 0);
                    for c in px.chunks_mut(4) {
                        c.swap(0, 2);
                        if no_alpha {
                            c[3] = 255;
                        }
                    }
                    result = Some((w, h, px));
                }
            }
        }
        if got {
            if !info.hbmColor.is_null() {
                DeleteObject(info.hbmColor);
            }
            if !info.hbmMask.is_null() {
                DeleteObject(info.hbmMask);
            }
        }
        DestroyIcon(large);
        result
    }
}

// 整個視窗的透明度；255 就是不透明。樣式或數值已經對了就不動。
pub fn ensure_window_alpha(hwnd: isize, alpha: u8) {
    unsafe {
        let h = hwnd as *mut c_void;
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let layered = ex & WS_EX_LAYERED as isize != 0;
        if !layered {
            if alpha == 255 {
                return;
            }
            SetWindowLongPtrW(h, GWL_EXSTYLE, ex | WS_EX_LAYERED as isize);
        } else {
            let (mut key, mut cur, mut flags) = (0u32, 0u8, 0u32);
            if GetLayeredWindowAttributes(h, &mut key, &mut cur, &mut flags) != 0 && flags & LWA_ALPHA != 0 && cur == alpha {
                return;
            }
        }
        SetLayeredWindowAttributes(h, 0, alpha, LWA_ALPHA);
    }
}

// 不畫系統的外框：視窗套件把整個視窗都當內容區，但半透明（分層）視窗在啟用、閃爍時
// 系統仍會把舊式標題列畫上來；這裡攔掉外框重畫。
pub fn hide_system_frame(hwnd: isize) {
    use windows_sys::Win32::UI::WindowsAndMessaging::GWLP_WNDPROC;
    unsafe {
        let old = SetWindowLongPtrW(hwnd as *mut c_void, GWLP_WNDPROC, frame_proc as *const () as isize);
        OLD_PROC.store(old, std::sync::atomic::Ordering::SeqCst);
    }
}

static OLD_PROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

unsafe extern "system" fn frame_proc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    use windows_sys::Win32::UI::WindowsAndMessaging::{CallWindowProcW, WM_NCACTIVATE, WM_NCPAINT, WNDPROC};
    let old: WNDPROC = std::mem::transmute(OLD_PROC.load(std::sync::atomic::Ordering::SeqCst));
    match m {
        WM_NCPAINT => 0,
        // 最後一個參數給 -1，系統就不重畫外框。
        WM_NCACTIVATE => CallWindowProcW(old, h, m, w, -1),
        _ => CallWindowProcW(old, h, m, w, l),
    }
}

// 滑鼠點擊穿過視窗：要分層樣式加透明樣式；剛加上分層時先給不透明，透明度由 ensure_window_alpha 再補。
pub fn set_click_through(hwnd: isize, on: bool) {
    unsafe {
        let h = hwnd as *mut c_void;
        let ex = GetWindowLongPtrW(h, GWL_EXSTYLE);
        let want = if on { ex | (WS_EX_TRANSPARENT | WS_EX_LAYERED) as isize } else { ex & !(WS_EX_TRANSPARENT as isize) };
        if want == ex {
            return;
        }
        SetWindowLongPtrW(h, GWL_EXSTYLE, want);
        if ex & WS_EX_LAYERED as isize == 0 && want & WS_EX_LAYERED as isize != 0 {
            SetLayeredWindowAttributes(h, 0, 255, LWA_ALPHA);
        }
    }
}

pub fn set_round_region(hwnd: isize, w: i32, h: i32, r: i32) {
    unsafe {
        let rgn = CreateRoundRectRgn(0, 0, w + 1, h + 1, r, r);
        SetWindowRgn(hwnd as *mut c_void, rgn, 1);
    }
}

pub fn is_admin() -> bool {
    unsafe { windows_sys::Win32::UI::Shell::IsUserAnAdmin() != 0 }
}

// 用「以系統管理員身分執行」重開自己；使用者在權限視窗按否就回傳 false。
pub fn relaunch_as_admin() -> bool {
    let Ok(exe) = std::env::current_exe() else { return false };
    let file: Vec<u16> = exe.as_os_str().encode_wide().chain(std::iter::once(0)).collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let r = unsafe {
        windows_sys::Win32::UI::Shell::ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            file.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL,
        )
    };
    r as isize > 32
}

fn local() -> SYSTEMTIME {
    unsafe {
        let mut t: SYSTEMTIME = zeroed();
        GetLocalTime(&mut t);
        t
    }
}

pub fn date() -> String {
    let t = local();
    format!("{:04}-{:02}-{:02}", t.wYear, t.wMonth, t.wDay)
}

pub fn time() -> String {
    let t = local();
    format!("{:02}:{:02}:{:02}", t.wHour, t.wMinute, t.wSecond)
}

pub fn hhmm() -> String {
    let t = local();
    format!("{:02}:{:02}", t.wHour, t.wMinute)
}
