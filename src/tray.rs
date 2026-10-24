// 系統匣圖示：自己一條執行緒跑 Windows 訊息迴圈，點擊與選單轉成事件丟回主視窗。

use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    Shell_NotifyIconW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY,
    NOTIFYICONDATAW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
    TrackPopupMenu, TranslateMessage, HICON, ICONINFO, MF_STRING, MSG, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP,
    WM_CLOSE, WM_DESTROY, WM_LBUTTONUP, WM_RBUTTONUP, WNDCLASSW,
};
use winit::event_loop::EventLoopProxy;

#[derive(Debug)]
pub enum UserEvent {
    Repaint,
    Show,
    TogglePause,
    Quit,
}

#[derive(Clone, Default)]
pub struct TrayState {
    pub color: [u8; 3],
    pub tip: String,
    pub menu: [String; 3],
    pub notify: Option<(String, String)>,
}

const WM_TRAY: u32 = WM_APP + 1;
const WM_UPDATE: u32 = WM_APP + 2;

static PROXY: OnceLock<Mutex<EventLoopProxy<UserEvent>>> = OnceLock::new();
static STATE: Mutex<Option<TrayState>> = Mutex::new(None);
static HWND_TRAY: AtomicIsize = AtomicIsize::new(0);

pub fn start(proxy: EventLoopProxy<UserEvent>) {
    let _ = PROXY.set(Mutex::new(proxy));
    std::thread::spawn(|| unsafe { run() });
}

pub fn update(s: TrayState) {
    *STATE.lock().unwrap() = Some(s);
    post(WM_UPDATE);
}

pub fn stop() {
    post(WM_CLOSE);
}

fn post(msg: u32) {
    let h = HWND_TRAY.load(Ordering::SeqCst);
    if h != 0 {
        unsafe {
            PostMessageW(h as HWND, msg, 0, 0);
        }
    }
}

fn send(e: UserEvent) {
    if let Some(p) = PROXY.get() {
        let _ = p.lock().unwrap().send_event(e);
    }
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn copy_into(dst: &mut [u16], s: &str) {
    let w: Vec<u16> = s.encode_utf16().take(dst.len() - 1).collect();
    dst[..w.len()].copy_from_slice(&w);
    dst[w.len()] = 0;
}

fn base(h: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = unsafe { zeroed() };
    n.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = h;
    n.uID = 1;
    n
}

// 32×32 的狀態圖示：狀態色圓角方塊加白色圓環。
unsafe fn make_icon(c: [u8; 3]) -> HICON {
    const S: usize = 32;
    let mut px = vec![0u8; S * S * 4];
    let cover = |d: f32| (0.5 - d).clamp(0.0, 1.0);
    for y in 0..S {
        for x in 0..S {
            let (fx, fy) = (x as f32 + 0.5, y as f32 + 0.5);
            let qx = (fx - 16.0).abs() - 8.0;
            let qy = (fy - 16.0).abs() - 8.0;
            let box_d = (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() + qx.max(qy).min(0.0) - 7.0;
            let a = cover(box_d);
            let r = ((fx - 16.0).powi(2) + (fy - 16.0).powi(2)).sqrt();
            let ring = cover((r - 8.5).abs() - 2.5);
            let (mut rr, mut gg, mut bb) = (c[0] as f32, c[1] as f32, c[2] as f32);
            rr += (255.0 - rr) * ring;
            gg += (255.0 - gg) * ring;
            bb += (255.0 - bb) * ring;
            let i = (y * S + x) * 4;
            px[i] = (bb * a) as u8;
            px[i + 1] = (gg * a) as u8;
            px[i + 2] = (rr * a) as u8;
            px[i + 3] = (a * 255.0) as u8;
        }
    }
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = S as i32;
    bi.bmiHeader.biHeight = -(S as i32);
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    bi.bmiHeader.biCompression = BI_RGB;
    let mut bits: *mut c_void = std::ptr::null_mut();
    let color = CreateDIBSection(std::ptr::null_mut(), &bi, DIB_RGB_COLORS, &mut bits, std::ptr::null_mut(), 0);
    if color.is_null() || bits.is_null() {
        return std::ptr::null_mut();
    }
    std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
    let mask_bits = vec![0u8; S * S / 8];
    let mask = CreateBitmap(S as i32, S as i32, 1, 1, mask_bits.as_ptr() as *const c_void);
    let info = ICONINFO { fIcon: 1, xHotspot: 0, yHotspot: 0, hbmMask: mask, hbmColor: color };
    let icon = CreateIconIndirect(&info);
    DeleteObject(color);
    DeleteObject(mask);
    icon
}

fn icon_for(c: [u8; 3]) -> HICON {
    static CACHE: Mutex<Option<HashMap<[u8; 3], isize>>> = Mutex::new(None);
    let mut g = CACHE.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    *map.entry(c).or_insert_with(|| unsafe { make_icon(c) } as isize) as HICON
}

unsafe fn run() {
    let hinst = GetModuleHandleW(std::ptr::null());
    let class = wide("MemGuardTray");
    let wc = WNDCLASSW { lpfnWndProc: Some(wndproc), hInstance: hinst, lpszClassName: class.as_ptr(), ..zeroed() };
    RegisterClassW(&wc);
    // 一般隱藏視窗（不用訊息專用視窗），右鍵選單才收得起來。
    let h = CreateWindowExW(
        0,
        class.as_ptr(),
        class.as_ptr(),
        0,
        0,
        0,
        0,
        0,
        std::ptr::null_mut(),
        std::ptr::null_mut(),
        hinst,
        std::ptr::null(),
    );
    if h.is_null() {
        return;
    }
    HWND_TRAY.store(h as isize, Ordering::SeqCst);
    let mut n = base(h);
    n.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = icon_for([120, 169, 133]);
    copy_into(&mut n.szTip, "Memory Guard");
    Shell_NotifyIconW(NIM_ADD, &n);
    if STATE.lock().unwrap().is_some() {
        apply(h);
    }
    let mut msg: MSG = zeroed();
    while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
        TranslateMessage(&msg);
        DispatchMessageW(&msg);
    }
}

unsafe fn apply(h: HWND) {
    let Some(s) = STATE.lock().unwrap().as_mut().map(|s| {
        let out = s.clone();
        s.notify = None;
        out
    }) else {
        return;
    };
    let mut n = base(h);
    n.uFlags = NIF_ICON | NIF_TIP;
    n.hIcon = icon_for(s.color);
    copy_into(&mut n.szTip, &s.tip);
    if let Some((title, msg)) = &s.notify {
        n.uFlags |= NIF_INFO;
        n.dwInfoFlags = NIIF_WARNING;
        copy_into(&mut n.szInfoTitle, title);
        copy_into(&mut n.szInfo, msg);
    }
    Shell_NotifyIconW(NIM_MODIFY, &n);
}

unsafe fn menu(h: HWND) {
    let labels = STATE
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.menu.clone())
        .unwrap_or_else(|| ["Show".into(), "Pause".into(), "Quit".into()]);
    let m = CreatePopupMenu();
    for (i, l) in labels.iter().enumerate() {
        let w = wide(l);
        AppendMenuW(m, MF_STRING, i + 1, w.as_ptr());
    }
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(h);
    let cmd = TrackPopupMenu(m, TPM_RETURNCMD | TPM_RIGHTBUTTON, pt.x, pt.y, 0, h, std::ptr::null());
    DestroyMenu(m);
    match cmd {
        1 => send(UserEvent::Show),
        2 => send(UserEvent::TogglePause),
        3 => send(UserEvent::Quit),
        _ => {}
    }
}

unsafe extern "system" fn wndproc(h: HWND, m: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    match m {
        WM_TRAY => {
            match (l as u32) & 0xffff {
                WM_LBUTTONUP => send(UserEvent::Show),
                WM_RBUTTONUP => menu(h),
                _ => {}
            }
            0
        }
        WM_UPDATE => {
            apply(h);
            0
        }
        WM_CLOSE => {
            let n = base(h);
            Shell_NotifyIconW(NIM_DELETE, &n);
            DestroyWindow(h);
            0
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(h, m, w, l),
    }
}
