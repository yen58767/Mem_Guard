#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod gpu;
mod i18n;
mod raster;
mod sys;
mod tray;

use egui::{pos2, vec2, Align2, Color32, FontFamily, FontId, Id, Pos2, Rect, Rounding, Sense, Shape, Stroke, Ui};
use i18n::{t, tf};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::Write;
use std::num::NonZeroU32;
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::Command;
use std::rc::Rc;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant, SystemTime};
use tray::UserEvent;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::WindowEvent;
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::{Icon, Window, WindowId, WindowLevel};

const RUN_KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
const RUN_NAME: &str = "MemGuard";
const HISTORY: Duration = Duration::from_secs(30 * 60);
const GROWTH_WINDOW: Duration = Duration::from_secs(10 * 60);
const FAST_INTERVAL: u64 = 10;
const RANK_INTERVAL: u64 = 5;
const RANK_ROWS: usize = 40;
const TAB_H: f32 = 40.0;
const ROW_H: f32 = 70.0;
// 目標超過這麼多個，清單就在框內捲動。
const TARGETS_SHOWN: usize = 3;
const KILL_COOLDOWN: Duration = Duration::from_secs(60);
const KILLED_SHOWN: Duration = Duration::from_secs(5 * 60);
const REPEAT_SOUND: Duration = Duration::from_millis(9500);
// 十分鐘變化小於這個量就當作沒變。
const D10_MIN: f64 = 0.1;
// 到個別上限的這個比例就先變橙色。
const LIMIT_NEAR: f64 = 0.9;
const LOG_EVERY: Duration = Duration::from_secs(60);
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const WIN_W: f32 = 560.0;
// 總覽放三個目標剛好不出捲軸的高度（三個目標時最後一張卡片底在 830 左右，加底部留白）。
const MAX_H: f32 = 850.0;
const TITLE_H: f32 = 40.0;
const BAR_H: f32 = 84.0;
const MINI_H: f32 = 40.0;
const ICON_RGBA: &[u8] = include_bytes!("../assets/icon_64.rgba");
const VERSION: &str = env!("CARGO_PKG_VERSION");
const AUTHOR: &str = env!("CARGO_PKG_AUTHORS");
const REPO: &str = "yen58767/Mem_Guard";

// 檢查更新的狀態；只在使用者按下「檢查更新」時連網。
#[derive(Clone, PartialEq)]
enum Update {
    Idle,
    Checking,
    Latest,
    Newer(String),
    Failed,
}

// 到 GitHub 問最新發佈的版本，跟自己比；版本號寫成 v1.2.3 或 1.2.3 都可以。
fn check_update() -> Update {
    let Some((200, body)) = sys::https_get("api.github.com", &format!("/repos/{}/releases/latest", REPO)) else { return Update::Failed };
    let Some(tag) = serde_json::from_str::<serde_json::Value>(&body).ok().and_then(|v| v["tag_name"].as_str().map(String::from)) else {
        return Update::Failed;
    };
    let parse = |s: &str| -> Vec<u32> { s.trim_start_matches(['v', 'V']).split('.').map(|n| n.parse().unwrap_or(0)).collect() };
    if parse(&tag) > parse(VERSION) { Update::Newer(tag) } else { Update::Latest }
}

fn open_url(url: &str) {
    let _ = Command::new("explorer").arg(url).spawn();
}

// ---------- 設定 ----------

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Theme {
    Dark,
    Sakura,
}

// 看哪一種記憶體：認可（會卡死的那個）或實際使用（工作管理員「記憶體 使用中」）。
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Metric {
    Commit,
    Physical,
}

impl Metric {
    fn name(self) -> String {
        match self {
            Metric::Commit => t("m_commit"),
            Metric::Physical => t("m_phys"),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum Sound {
    Off,
    Once,
    Repeat,
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
enum MiniSize {
    Small,
    Medium,
    Large,
}

impl MiniSize {
    fn k(self) -> f32 {
        match self {
            MiniSize::Small => 0.85,
            MiniSize::Medium => 1.0,
            MiniSize::Large => 1.25,
        }
    }
}

// 小橫條要畫什麼：前六個是每個目標那一段（RAM 是實際使用），sys_ 開頭的是總量那一段。
#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct MiniCfg {
    name: bool,
    ram: bool,
    commit: bool,
    d10: bool,
    cpu: bool,
    gpu: bool,
    sys_ram: bool,
    sys_commit: bool,
    sys_cpu: bool,
    sys_gpu: bool,
    // 總量每一項畫不畫進度條；不畫就只留百分比。
    bars: bool,
    size: MiniSize,
    // 直向：每個目標一列往下排。
    vertical: bool,
}

impl Default for MiniCfg {
    fn default() -> Self {
        Self {
            name: true,
            ram: false,
            commit: true,
            d10: true,
            cpu: false,
            gpu: false,
            sys_ram: false,
            sys_commit: true,
            sys_cpu: false,
            sys_gpu: false,
            bars: true,
            size: MiniSize::Medium,
            vertical: true,
        }
    }
}

fn yes() -> bool {
    true
}

// 一個個別上限：開了沒、幾 GB。
#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
struct Limit {
    on: bool,
    gb: f64,
}

impl Default for Limit {
    fn default() -> Self {
        Limit { on: false, gb: 8.0 }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
struct Target {
    exe: String,
    name: String,
    kill: bool,
    #[serde(default)]
    path: String,
    // 個別上限：這個程式自己的認可或實際使用超過幾 GB 就照 kill 處理，兩個各自開關，跟整台門檻各算各的。
    #[serde(default)]
    commit_limit: Limit,
    #[serde(default)]
    phys_limit: Limit,
    // 動作是關掉時，比上限低的那條先響的線；只警告的目標不看這兩個。
    #[serde(default)]
    commit_warn: Limit,
    #[serde(default)]
    phys_warn: Limit,
    // 舊設定檔沒有這欄時當作要顯示，維持原本小橫條的樣子。
    #[serde(default = "yes")]
    mini: bool,
}

impl Target {
    fn new(exe: &str, path: &str) -> Self {
        Target {
            exe: exe.to_string(),
            name: strip_exe(exe),
            kill: false,
            path: path.to_string(),
            commit_limit: Limit::default(),
            phys_limit: Limit::default(),
            commit_warn: Limit::default(),
            phys_warn: Limit::default(),
            mini: false,
        }
    }

    fn limit(&self, m: Metric) -> Limit {
        if m == Metric::Commit { self.commit_limit } else { self.phys_limit }
    }

    fn limit_mut(&mut self, m: Metric) -> &mut Limit {
        if m == Metric::Commit { &mut self.commit_limit } else { &mut self.phys_limit }
    }

    // 先響的線只在動作是關掉時才算數。
    fn warn_line(&self, m: Metric) -> Option<f64> {
        let w = if m == Metric::Commit { self.commit_warn } else { self.phys_warn };
        (self.kill && w.on).then_some(w.gb)
    }

    fn warn_mut(&mut self, m: Metric) -> &mut Limit {
        if m == Metric::Commit { &mut self.commit_warn } else { &mut self.phys_warn }
    }
}

#[derive(Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    targets: Vec<Target>,
    warn_pct: f64,
    kill_pct: f64,
    growth_gb: f64,
    interval_s: u64,
    keep_days: u64,
    sound: Sound,
    on_top: bool,
    theme: Theme,
    mini_opacity: u8,
    mini_pos: Option<[i32; 2]>,
    view: Metric,
    basis: Metric,
    lang: String,
    paused: bool,
    gpu: bool,
    mini_bar: MiniCfg,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            targets: Vec::new(),
            warn_pct: 85.0,
            kill_pct: 93.0,
            growth_gb: 3.0,
            interval_s: 30,
            keep_days: 30,
            sound: Sound::Once,
            on_top: false,
            theme: Theme::Sakura,
            mini_opacity: 70,
            mini_pos: None,
            view: Metric::Commit,
            basis: Metric::Commit,
            lang: "auto".into(),
            paused: false,
            gpu: false,
            mini_bar: MiniCfg::default(),
        }
    }
}

impl Config {
    // 還原預設只動門檻與選項；偵測目標、畫面狀態、暫停不動。
    fn reset_options(&self) -> Config {
        Config {
            targets: self.targets.clone(),
            mini_pos: self.mini_pos,
            view: self.view,
            paused: self.paused,
            ..Config::default()
        }
    }
}

fn base_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

fn config_path() -> PathBuf {
    base_dir().join("config.json")
}

fn log_dir() -> PathBuf {
    base_dir().join("log")
}

fn load_config() -> Config {
    // 用記事本存過的設定檔開頭會多一個 BOM，先去掉才讀得懂。
    let Some(v) = fs::read_to_string(config_path())
        .ok()
        .and_then(|s| serde_json::from_str::<serde_json::Value>(s.trim_start_matches('\u{feff}')).ok())
    else {
        return Config::default();
    };
    let mut c: Config = serde_json::from_value(v.clone()).unwrap_or_default();
    // 舊設定檔的嗶聲開關與小橫條十分鐘開關，搬到新欄位。
    if v.get("sound").is_none() && v.get("beep").and_then(|b| b.as_bool()) == Some(false) {
        c.sound = Sound::Off;
    }
    if v.get("mini_bar").is_none() {
        if let Some(b) = v.get("mini_d10").and_then(|b| b.as_bool()) {
            c.mini_bar.d10 = b;
        }
    }
    // 舊設定檔每個目標只有一個上限（limit_on、limit_metric、limit_gb），搬到對應的那一個。
    if let Some(old) = v.get("targets").and_then(|t| t.as_array()) {
        for (tg, o) in c.targets.iter_mut().zip(old) {
            if o.get("commit_limit").is_some() || o.get("limit_gb").is_none() {
                continue;
            }
            let m = if o.get("limit_metric").and_then(|m| m.as_str()) == Some("Physical") { Metric::Physical } else { Metric::Commit };
            let l = tg.limit_mut(m);
            l.on = o.get("limit_on").and_then(|b| b.as_bool()).unwrap_or(false);
            l.gb = o.get("limit_gb").and_then(|g| g.as_f64()).unwrap_or(l.gb);
        }
    }
    c
}

fn save_config(c: &Config) {
    if let Ok(s) = serde_json::to_string_pretty(c) {
        let _ = fs::write(config_path(), s);
    }
}

fn autostart_get() -> bool {
    Command::new("reg")
        .args(["query", RUN_KEY, "/v", RUN_NAME])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn autostart_set(on: bool) {
    let mut cmd = Command::new("reg");
    if on {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
        cmd.args(["add", RUN_KEY, "/v", RUN_NAME, "/t", "REG_SZ", "/d", &format!("\"{}\"", exe), "/f"]);
    } else {
        cmd.args(["delete", RUN_KEY, "/v", RUN_NAME, "/f"]);
    }
    let _ = cmd.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).creation_flags(CREATE_NO_WINDOW).status();
}

fn strip_exe(exe: &str) -> String {
    if exe.to_lowercase().ends_with(".exe") { exe[..exe.len() - 4].to_string() } else { exe.to_string() }
}

// ---------- 紀錄檔 ----------

const LOG_HEADER: &str = "\u{feff}時間,項目,實際佔用GB,認可大小GB,整台已認可GB,整台上限GB,整台實際使用GB,整台實體總量GB";

fn append_log(lines: &[String]) {
    let dir = log_dir();
    let _ = fs::create_dir_all(&dir);
    let date = sys::date();
    let path = dir.join(format!("{}.csv", date));
    // 同一天的舊格式檔改名保留，新欄位另開一檔，兩邊都讀得回來。
    if let Ok(fh) = fs::File::open(&path) {
        let mut first = String::new();
        let _ = std::io::BufRead::read_line(&mut std::io::BufReader::new(fh), &mut first);
        if first.trim_end() != LOG_HEADER {
            let _ = fs::rename(&path, dir.join(format!("{}-舊格式.csv", date)));
        }
    }
    let new = !path.exists();
    if let Ok(mut f) = fs::OpenOptions::new().create(true).append(true).open(&path) {
        if new {
            let _ = writeln!(f, "{}", LOG_HEADER);
        }
        for l in lines {
            let _ = writeln!(f, "{}", l);
        }
    }
}

fn clean_logs(keep_days: u64) {
    let Ok(rd) = fs::read_dir(log_dir()) else { return };
    let keep = Duration::from_secs(keep_days.max(1) * 86_400);
    for e in rd.flatten() {
        let p = e.path();
        if p.extension().map_or(true, |x| x != "csv") {
            continue;
        }
        let old = e
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|m| SystemTime::now().duration_since(m).ok())
            .map_or(false, |age| age > keep);
        if old {
            let _ = fs::remove_file(p);
        }
    }
}

fn csv_text(s: &str) -> String {
    s.replace(',', " ")
}

// 紀錄檔每列最後四欄：整台已認可、整台上限、整台實際使用、整台實體總量。
fn whole_cols(c: &sys::Commit) -> String {
    format!("{:.2},{:.2},{:.2},{:.2}", c.total_gb, c.limit_gb, c.phys_used_gb, c.phys_total_gb)
}

// ---------- 量測 ----------

#[derive(Clone, Copy, PartialEq, PartialOrd)]
enum Level {
    Ok,
    Warn,
    Bad,
}

#[derive(Clone)]
struct Status {
    level: Level,
    title: String,
    msg: String,
}

impl Status {
    fn new(level: Level, title: impl Into<String>, msg: impl Into<String>) -> Self {
        Self { level, title: title.into(), msg: msg.into() }
    }
}

#[derive(Clone)]
struct Row {
    exe: String,
    name: String,
    kill: bool,
    path: String,
    ws: Option<f64>,
    commit: Option<f64>,
    d10: Option<f64>,
    hot: bool,
    cpu: Option<f64>,
    gpu: Option<f64>,
}

#[derive(Clone)]
struct Sample {
    t: Instant,
    sys: f64,
    phys: f64,
    // 目標程式名（小寫）→（認可大小, 實際佔用）
    vals: HashMap<String, (f64, f64)>,
}

impl Sample {
    fn whole(&self, m: Metric) -> f64 {
        match m {
            Metric::Commit => self.sys,
            Metric::Physical => self.phys,
        }
    }

    fn of(&self, key: &str, m: Metric) -> Option<f64> {
        self.vals.get(key).map(|&(c, w)| if m == Metric::Commit { c } else { w })
    }
}

// 目前在跑的每一種程式（同名合併）：加目標的清單和排行分頁共用。
#[derive(Clone)]
struct Running {
    exe: String,
    name: String,
    path: String,
    commit: f64,
    ws: f64,
    cpu: Option<f64>,
    gpu: Option<f64>,
    system: bool,
}

impl Running {
    fn val_of(&self, m: Metric) -> f64 {
        if m == Metric::Commit { self.commit } else { self.ws }
    }

    fn val(&self, m: RankBy) -> f64 {
        match m {
            RankBy::Name => 0.0,
            RankBy::Physical => self.ws,
            RankBy::Commit => self.commit,
            RankBy::Cpu => self.cpu.unwrap_or(0.0),
            RankBy::Gpu => self.gpu.unwrap_or(0.0),
        }
    }

    // 照欄位比大小；名稱照字母，其餘照數值。
    fn cmp(&self, o: &Running, m: RankBy) -> std::cmp::Ordering {
        if m == RankBy::Name {
            self.name.to_lowercase().cmp(&o.name.to_lowercase())
        } else {
            self.val(m).total_cmp(&o.val(m))
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum RankBy {
    Name,
    Physical,
    Commit,
    Cpu,
    Gpu,
}

#[derive(Clone, Default)]
struct View {
    ready: bool,
    sys: f64,
    limit: f64,
    phys: f64,
    phys_total: f64,
    cpu: Option<f64>,
    gpu: Option<f64>,
    rows: Vec<Row>,
    running: Vec<Running>,
}

impl View {
    fn whole(&self, m: Metric) -> (f64, f64) {
        match m {
            Metric::Commit => (self.sys, self.limit),
            Metric::Physical => (self.phys, self.phys_total),
        }
    }
}

struct Shared {
    cfg: Config,
    hist: VecDeque<Sample>,
    view: View,
    status: Status,
    wake: bool,
    // 警告音靜音中；狀態再變差時由量測解除。
    muted: bool,
    // 使用者按了「立即結束」的目標（小寫程式名），量測拿它的 pid 去關。
    end_now: Option<String>,
    // 排行分頁開著，量測改成每 RANK_INTERVAL 秒一次。
    fast: bool,
}

// 量測執行緒和畫面共用的狀態；設定一改就叫醒量測立刻重量。
struct Hub {
    s: Mutex<Shared>,
    cv: Condvar,
}

impl Hub {
    fn wake(&self) {
        self.s.lock().unwrap().wake = true;
        self.cv.notify_all();
    }
}

struct Group {
    exe: String,
    path: String,
    ws: f64,
    commit: f64,
    pids: Vec<u32>,
    // 跟 pids 一一對應的累計 CPU 時間。
    times: Vec<u64>,
    system: bool,
}

impl Group {
    fn val(&self, m: Metric) -> f64 {
        if m == Metric::Commit { self.commit } else { self.ws }
    }
}

fn monitor(hub: Arc<Hub>, ctx: egui::Context) {
    let me = sys::current_pid();
    let mut last_log: Option<Instant> = None;
    let mut last_clean: Option<Instant> = None;
    let mut last_kill_try: Option<Instant> = None;
    let mut target_kill_try: HashMap<String, Instant> = HashMap::new();
    let mut kill_status: Option<(Instant, Status)> = None;
    let mut prev_level = Level::Ok;
    let mut last_sound: Option<Instant> = None;
    // 上一次量測的時間點與 CPU 累計時間，兩次相減才算得出 CPU 用量。
    let mut prev_t: Option<Instant> = None;
    let mut prev_cpu: HashMap<u32, u64> = HashMap::new();
    let mut prev_sys: Option<(u64, u64, u64)> = None;
    let ncpu = sys::cpu_count() as f64;
    let mut gpu: Option<gpu::Gpu> = None;
    loop {
        let cfg = hub.s.lock().unwrap().cfg.clone();
        let now = Instant::now();
        let Some(c) = sys::commit() else {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        };

        let mut groups: HashMap<String, Group> = HashMap::new();
        for p in sys::processes() {
            let g = groups.entry(p.exe.to_lowercase()).or_insert_with(|| Group {
                exe: p.exe.clone(),
                path: String::new(),
                ws: 0.0,
                commit: 0.0,
                pids: Vec::new(),
                times: Vec::new(),
                system: false,
            });
            if g.path.is_empty() {
                g.path = p.path.clone();
            }
            g.ws += p.ws_gb;
            g.commit += p.commit_gb;
            g.pids.push(p.pid);
            g.times.push(p.cpu);
            g.system |= sys::is_system(&p, me);
        }
        let target_of = |key: &str| cfg.targets.iter().find(|t| t.exe.to_lowercase() == key);
        let display = |key: &str, exe: &str| target_of(key).map(|t| t.name.clone()).unwrap_or_else(|| strip_exe(exe));

        // 立即結束：使用者手動要的，不看門檻也不看暫停；關完馬上重量一次。
        let end = hub.s.lock().unwrap().end_now.take();
        if let Some(k) = end {
            if let Some(g) = groups.get(&k) {
                let name = display(&k, &g.exe);
                let ok = g.pids.iter().fold(true, |acc, &pid| sys::kill(pid) && acc);
                let st = if ok {
                    Status::new(Level::Ok, tf("st_ended", &[&name]), tf("msg_ended", &[&sys::hhmm()]))
                } else {
                    Status::new(Level::Warn, tf("st_kill_fail", &[&name]), t("msg_no_perm"))
                };
                append_log(&[format!("{},#{} {},,,{}", sys::time(), csv_text(&st.title), csv_text(&st.msg), whole_cols(&c))]);
                kill_status = Some((now, st));
                std::thread::sleep(Duration::from_millis(300));
                continue;
            }
        }

        let wall = prev_t.map(|t| now.duration_since(t).as_secs_f64() * 1e7);
        let cpu_of = |g: &Group| -> Option<f64> {
            let w = wall.filter(|w| *w > 0.0)?;
            let mut used = 0u64;
            let mut seen = false;
            for (pid, t) in g.pids.iter().zip(&g.times) {
                if let Some(o) = prev_cpu.get(pid) {
                    used += t.saturating_sub(*o);
                    seen = true;
                }
            }
            seen.then(|| (used as f64 / (w * ncpu) * 100.0).min(100.0))
        };
        let sys_now = sys::system_times();
        let sys_cpu = match (prev_sys, sys_now) {
            (Some((i0, k0, u0)), Some((i1, k1, u1))) => {
                let busy = (k1 - k0) + (u1 - u0);
                (busy > 0).then(|| ((1.0 - (i1 - i0) as f64 / busy as f64) * 100.0).clamp(0.0, 100.0))
            }
            _ => None,
        };
        if cfg.gpu {
            if gpu.is_none() {
                gpu = gpu::Gpu::new();
            }
        } else {
            gpu = None;
        }
        let gs = gpu.as_mut().and_then(|g| g.sample());
        // 警告、動手、漲太快、誰最多都看同一個判斷依據。
        let basis = cfg.basis;
        let pct = match basis {
            Metric::Commit => c.total_gb / c.limit_gb * 100.0,
            Metric::Physical => c.phys_used_gb / c.phys_total_gb * 100.0,
        };

        let mut vals = HashMap::new();
        for t in &cfg.targets {
            let k = t.exe.to_lowercase();
            if let Some(g) = groups.get(&k) {
                vals.insert(k, (g.commit, g.ws));
            }
        }

        let mut sorted: Vec<(&String, &Group)> = groups.iter().collect();
        sorted.sort_by(|a, b| b.1.val(basis).total_cmp(&a.1.val(basis)));
        let running: Vec<Running> = sorted
            .iter()
            .map(|(k, g)| Running {
                exe: g.exe.clone(),
                name: display(k, &g.exe),
                path: g.path.clone(),
                commit: g.commit,
                ws: g.ws,
                cpu: cpu_of(g),
                gpu: gs.as_ref().map(|s| s.group(&g.pids)),
                system: g.system,
            })
            .collect();

        let rows: Vec<Row> = {
            let mut sh = hub.s.lock().unwrap();
            sh.hist.push_back(Sample { t: now, sys: c.total_gb, phys: c.phys_used_gb, vals });
            while sh.hist.front().map_or(false, |s| now.duration_since(s.t) > HISTORY + Duration::from_secs(60)) {
                sh.hist.pop_front();
            }
            cfg.targets
                .iter()
                .map(|t| {
                    let k = t.exe.to_lowercase();
                    let g = groups.get(&k);
                    // 比較基準是十分鐘內這個程式最早出現的那筆；剛加入的目標下一次量測就有數字。
                    let base = sh.hist.iter().filter(|s| now.duration_since(s.t) <= GROWTH_WINDOW).find(|s| s.vals.contains_key(&k));
                    let d10 = match (g.map(|g| g.val(basis)), base.and_then(|b| b.of(&k, basis))) {
                        (Some(n), Some(o)) => Some(n - o),
                        _ => None,
                    };
                    let hot = d10.map_or(false, |d| d >= cfg.growth_gb);
                    let path = g.map(|g| g.path.clone()).filter(|p| !p.is_empty()).unwrap_or_else(|| t.path.clone());
                    Row {
                        exe: t.exe.clone(),
                        name: t.name.clone(),
                        kill: t.kill,
                        path,
                        ws: g.map(|g| g.ws),
                        commit: g.map(|g| g.commit),
                        d10,
                        hot,
                        cpu: g.and_then(cpu_of),
                        gpu: g.and_then(|g| gs.as_ref().map(|s| s.group(&g.pids))),
                    }
                })
                .collect()
        };
        let first = prev_t.is_none();
        prev_t = Some(now);
        prev_sys = sys_now;
        prev_cpu = groups.values().flat_map(|g| g.pids.iter().copied().zip(g.times.iter().copied())).collect();

        let pct_s = format!("{:.0}", pct);
        let status = if pct >= cfg.kill_pct {
            let first = sorted.first();
            let name = first.map(|(k, g)| display(k, &g.exe)).unwrap_or_default();
            match first {
                // 暫停時照樣量、照樣警告，只是不關程式。
                Some((k, g)) if !cfg.paused && target_of(k).map_or(false, |t| t.kill) => {
                    if last_kill_try.map_or(true, |x| now.duration_since(x) >= KILL_COOLDOWN) {
                        last_kill_try = Some(now);
                        let ok = g.pids.iter().fold(true, |acc, &pid| sys::kill(pid) && acc);
                        let st = if ok {
                            Status::new(Level::Bad, tf("st_killed", &[&name]), tf("msg_killed", &[&sys::hhmm(), &pct_s]))
                        } else {
                            Status::new(Level::Bad, tf("st_kill_fail", &[&name]), t("msg_no_perm"))
                        };
                        append_log(&[format!("{},#{} {},,,{}", sys::time(), csv_text(&st.title), csv_text(&st.msg), whole_cols(&c))]);
                        kill_status = Some((now, st.clone()));
                        st
                    } else {
                        kill_status.as_ref().map(|(_, s)| s.clone()).unwrap_or_else(|| Status::new(Level::Bad, t("st_danger"), ""))
                    }
                }
                _ => {
                    let key = if cfg.paused { "msg_top_paused" } else { "msg_top" };
                    Status::new(Level::Warn, t("st_warn"), tf(key, &[&pct_s, &name]))
                }
            }
        } else if let Some((tg, g, m, v, lim)) = cfg.targets.iter().find_map(|x| {
            let g = groups.get(&x.exe.to_lowercase())?;
            [Metric::Commit, Metric::Physical].into_iter().find_map(|m| {
                let l = x.limit(m);
                let v = g.val(m);
                (l.on && v >= l.gb).then_some((x, g, m, v, l.gb))
            })
        }) {
            let msg = tf("msg_over", &[&tg.name, &m.name(), &format!("{:.1}", v), &format!("{:.1}", lim)]);
            if tg.kill && !cfg.paused {
                let key = tg.exe.to_lowercase();
                if target_kill_try.get(&key).map_or(true, |x| now.duration_since(*x) >= KILL_COOLDOWN) {
                    target_kill_try.insert(key, now);
                    let ok = g.pids.iter().fold(true, |acc, &pid| sys::kill(pid) && acc);
                    let st = if ok {
                        Status::new(Level::Bad, tf("st_killed", &[&tg.name]), msg)
                    } else {
                        Status::new(Level::Bad, tf("st_kill_fail", &[&tg.name]), t("msg_no_perm"))
                    };
                    append_log(&[format!("{},#{} {},,,{}", sys::time(), csv_text(&st.title), csv_text(&st.msg), whole_cols(&c))]);
                    kill_status = Some((now, st.clone()));
                    st
                } else {
                    kill_status.as_ref().map(|(_, s)| s.clone()).unwrap_or_else(|| Status::new(Level::Bad, t("st_danger"), msg))
                }
            } else {
                Status::new(Level::Warn, t("st_warn"), msg)
            }
        } else if let Some((tg, m, v, w)) = cfg.targets.iter().find_map(|x| {
            let g = groups.get(&x.exe.to_lowercase())?;
            [Metric::Commit, Metric::Physical].into_iter().find_map(|m| {
                let w = x.warn_line(m)?;
                let v = g.val(m);
                (v >= w).then_some((x, m, v, w))
            })
        }) {
            Status::new(Level::Warn, t("st_warn"), tf("msg_over", &[&tg.name, &m.name(), &format!("{:.1}", v), &format!("{:.1}", w)]))
        } else if pct >= cfg.warn_pct {
            Status::new(Level::Warn, t("st_warn"), tf("msg_level", &[&basis.name(), &pct_s]))
        } else if let Some(r) = rows.iter().find(|r| r.hot) {
            Status::new(Level::Warn, t("st_warn"), tf("msg_fast", &[&r.name]))
        } else {
            match &kill_status {
                Some((t0, s)) if now.duration_since(*t0) < KILLED_SHOWN => s.clone(),
                _ => Status::new(Level::Ok, t("st_ok"), ""),
            }
        };

        // 狀態變差一定響（除非關掉）並解除靜音；「持續」在警告期間每 10 秒再響，靜音中不響。
        let worse = status.level > prev_level;
        let muted = {
            let mut sh = hub.s.lock().unwrap();
            if worse {
                sh.muted = false;
            }
            sh.muted
        };
        // 暫停時完全不響。
        let repeat_due = cfg.sound == Sound::Repeat
            && status.level != Level::Ok
            && !muted
            && last_sound.map_or(true, |x| now.duration_since(x) >= REPEAT_SOUND);
        if !cfg.paused && ((worse && cfg.sound != Sound::Off) || (!worse && repeat_due)) {
            sys::play_alert();
            last_sound = Some(now);
        }
        if worse {
            ctx.send_viewport_cmd(egui::ViewportCommand::RequestUserAttention(egui::UserAttentionType::Critical));
        }
        prev_level = status.level;

        if last_log.map_or(true, |t| now.duration_since(t) >= LOG_EVERY) {
            last_log = Some(now);
            let ts = sys::time();
            let tail = whole_cols(&c);
            let mut lines = vec![format!("{},(整台),,,{}", ts, tail)];
            for r in &rows {
                if let (Some(ws), Some(cm)) = (r.ws, r.commit) {
                    lines.push(format!("{},{},{:.2},{:.2},{}", ts, csv_text(&r.name), ws, cm, tail));
                }
            }
            append_log(&lines);
        }
        if last_clean.map_or(true, |t| now.duration_since(t) >= Duration::from_secs(3600)) {
            last_clean = Some(now);
            clean_logs(cfg.keep_days);
        }

        // 開頭第一次量完很快再量一次，CPU 用量才不用等一整個間隔才有數字。
        let mut sh = hub.s.lock().unwrap();
        let normal = if status.level == Level::Ok { cfg.interval_s.max(5) } else { FAST_INTERVAL };
        let wait = if first {
            2
        } else if sh.fast {
            normal.min(RANK_INTERVAL)
        } else {
            normal
        };
        sh.view = View {
            ready: true,
            sys: c.total_gb,
            limit: c.limit_gb,
            phys: c.phys_used_gb,
            phys_total: c.phys_total_gb,
            cpu: sys_cpu,
            gpu: gs.map(|s| s.whole),
            rows,
            running,
        };
        sh.status = status;
        ctx.request_repaint();
        let (mut sh, _) = hub.cv.wait_timeout_while(sh, Duration::from_secs(wait), |s| !s.wake).unwrap();
        sh.wake = false;
    }
}

// ---------- 外觀 ----------

struct Pal {
    win: Color32,
    title: Color32,
    card: Color32,
    line: Color32,
    well: Color32,
    txt: Color32,
    dim: Color32,
    strong: Color32,
    pill: Color32,
    shadow: Color32,
    ok: Color32,
    warn: Color32,
    bad: Color32,
    sys: Color32,
    accents: [Color32; 4],
    mini_bg: Color32,
    mini_txt: Color32,
    mini_dim: Color32,
    mini_track: Color32,
}

impl Pal {
    fn accent(&self, i: usize) -> Color32 {
        self.accents[i % self.accents.len()]
    }

    fn level(&self, l: Level) -> Color32 {
        match l {
            Level::Ok => self.ok,
            Level::Warn => self.warn,
            Level::Bad => self.bad,
        }
    }
}

const fn hex(v: u32) -> Color32 {
    Color32::from_rgb((v >> 16) as u8, (v >> 8) as u8, v as u8)
}

fn palette(t: Theme) -> Pal {
    match t {
        Theme::Sakura => Pal {
            win: hex(0xf8f0ea),
            title: hex(0xf3e6dd),
            card: hex(0xfffaf7),
            line: hex(0xecdcd2),
            well: hex(0xf2e6de),
            txt: hex(0x5a4640),
            dim: hex(0xa68f85),
            strong: hex(0x43302b),
            pill: hex(0xf6e3e7),
            shadow: Color32::from_rgba_unmultiplied(120, 80, 60, 22),
            ok: hex(0x78a985),
            warn: hex(0xd9984a),
            bad: hex(0xd4627a),
            sys: hex(0x9c8378),
            accents: [hex(0xe58fa7), hex(0xb8916f), hex(0x8fb3c9), hex(0xa7c48f)],
            mini_bg: hex(0xfffaf7),
            mini_txt: hex(0x43302b),
            mini_dim: hex(0xa68f85),
            mini_track: hex(0xf2e6de),
        },
        Theme::Dark => Pal {
            win: hex(0x1a1a1c),
            title: hex(0x141416),
            card: hex(0x232326),
            line: hex(0x2f2f33),
            well: hex(0x18181a),
            txt: hex(0xd6d6d8),
            dim: hex(0x86868c),
            strong: hex(0xffffff),
            pill: hex(0x2c2c31),
            shadow: Color32::from_rgba_unmultiplied(0, 0, 0, 80),
            ok: hex(0x4fb477),
            warn: hex(0xe0a83a),
            bad: hex(0xe0524a),
            sys: hex(0xc9c9c9),
            accents: [hex(0x5aa0ff), hex(0xb58cff), hex(0x4fd1c5), hex(0xf28c5a)],
            mini_bg: hex(0x1e1816),
            mini_txt: hex(0xf3e9e4),
            mini_dim: hex(0xa0928c),
            mini_track: hex(0x3a3330),
        },
    }
}

fn visuals(t: Theme, p: &Pal) -> egui::Visuals {
    let mut v = if t == Theme::Dark { egui::Visuals::dark() } else { egui::Visuals::light() };
    v.panel_fill = p.win;
    v.window_fill = p.win;
    v.extreme_bg_color = p.well;
    v.faint_bg_color = p.card;
    v.override_text_color = Some(p.txt);
    for w in [&mut v.widgets.inactive, &mut v.widgets.noninteractive, &mut v.widgets.hovered, &mut v.widgets.active] {
        w.bg_fill = p.well;
        w.weak_bg_fill = p.well;
        w.bg_stroke = Stroke::new(1.0, p.line);
        w.fg_stroke = Stroke::new(1.0, p.txt);
        w.rounding = Rounding::same(6.0);
    }
    v.selection.bg_fill = p.pill;
    v.selection.stroke = Stroke::new(1.0, p.strong);
    v.text_cursor.stroke = Stroke::new(1.5, p.strong);
    v
}

fn map_font(file: &str) -> Option<&'static [u8]> {
    static CACHE: Mutex<Option<HashMap<String, &'static [u8]>>> = Mutex::new(None);
    let mut g = CACHE.lock().unwrap();
    let map = g.get_or_insert_with(HashMap::new);
    if let Some(b) = map.get(file) {
        return Some(b);
    }
    let win = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".into());
    let f = fs::File::open(PathBuf::from(win).join("Fonts").join(file)).ok()?;
    // 映射檔案而不是讀進來：字型頁面跟系統共用，不算這支程式的認可量。
    let m = unsafe { memmap2::Mmap::map(&f) }.ok()?;
    let m: &'static memmap2::Mmap = Box::leak(Box::new(m));
    map.insert(file.to_string(), &m[..]);
    Some(&m[..])
}

// 每種語言的（一般, 粗體）字型檔，依序找第一個有的。
fn lang_fonts(lang: &str) -> (&'static [&'static str], &'static [&'static str]) {
    match lang {
        "zh-CN" => (&["msyh.ttc"], &["msyhbd.ttc"]),
        "ja" => (&["YuGothM.ttc", "meiryo.ttc", "msgothic.ttc"], &["YuGothB.ttc", "meiryob.ttc"]),
        "ko" => (&["malgun.ttf"], &["malgunbd.ttf"]),
        "en" => (&["segoeui.ttf"], &["segoeuib.ttf"]),
        _ => (&["msjh.ttc"], &["msjhbd.ttc"]),
    }
}

fn lang_family(code: &str) -> FontFamily {
    FontFamily::Name(format!("lang-{}", code).into())
}

// 每種語言先用它自己的字型，其餘語言的字型排在後面備用，語言名稱才都顯示得出來。
fn setup_fonts(ctx: &egui::Context, lang: &str) {
    let (reg, bold) = lang_fonts(lang);
    let fallback = ["msjh.ttc", "msyh.ttc", "YuGothM.ttc", "malgun.ttf"];
    let mut fonts = egui::FontDefinitions::default();
    let mut regular_names = Vec::new();
    for f in reg.iter().take(1).chain(fallback.iter()) {
        if regular_names.iter().any(|n: &String| n == f) {
            continue;
        }
        if let Some(b) = map_font(f) {
            fonts.font_data.insert(f.to_string(), egui::FontData::from_static(b));
            regular_names.push(f.to_string());
        }
    }
    let primary_bold = bold.iter().find_map(|f| map_font(f).map(|b| (f.to_string(), b)));
    let prop = fonts.families.entry(FontFamily::Proportional).or_default();
    for (i, n) in regular_names.iter().enumerate() {
        prop.insert(i, n.clone());
    }
    fonts.families.entry(FontFamily::Monospace).or_default().extend(regular_names.iter().cloned());
    let mut bold_family = Vec::new();
    if let Some((name, b)) = primary_bold {
        let key = format!("bold-{}", name);
        fonts.font_data.insert(key.clone(), egui::FontData::from_static(b));
        bold_family.push(key);
    }
    bold_family.extend(fonts.families.get(&FontFamily::Proportional).cloned().unwrap_or_default());
    fonts.families.insert(FontFamily::Name("bold".into()), bold_family);
    // 語言選單裡每個語言名稱用它自己的字型；只靠備用字型補字時，同一個名稱裡的字會大小不一。
    for (code, _) in i18n::LANGS {
        let mut fam = Vec::new();
        if let Some((name, b)) = lang_fonts(code).0.iter().find_map(|f| map_font(f).map(|b| (f.to_string(), b))) {
            fonts.font_data.entry(name.clone()).or_insert_with(|| egui::FontData::from_static(b));
            fam.push(name);
        }
        fam.extend(regular_names.iter().filter(|n| !fam.contains(n)).cloned().collect::<Vec<_>>());
        fonts.families.insert(lang_family(code), fam);
    }
    ctx.set_fonts(fonts);
}

fn f(size: f32) -> FontId {
    FontId::proportional(size)
}

fn fb(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name("bold".into()))
}

fn gb_text(v: Option<f64>) -> String {
    v.map_or("—".into(), |v| format!("{:.1} GB", v))
}

fn delta_text(v: Option<f64>) -> String {
    match v {
        Some(d) if d >= D10_MIN => format!("▲ {:.1} GB", d),
        Some(d) if d <= -D10_MIN => format!("▼ {:.1} GB", -d),
        _ => "—".into(),
    }
}

// 小橫條上的十分鐘變化：沒變化就整段不畫。
fn mini_delta(v: Option<f64>) -> Option<String> {
    match v {
        Some(d) if d >= D10_MIN => Some(format!("▲{:.1}", d)),
        Some(d) if d <= -D10_MIN => Some(format!("▼{:.1}", -d)),
        _ => None,
    }
}

fn delta_color(r: &Row, p: &Pal, normal: Color32) -> Color32 {
    match r.d10 {
        _ if r.hot => p.warn,
        Some(d) if d <= -D10_MIN => p.ok,
        _ => normal,
    }
}

// 單行文字從 left_center 往右畫，超過 max_w 就截斷加「…」；回傳範圍和有沒有被截。
fn text_elided(painter: &egui::Painter, left_center: Pos2, text: &str, font: FontId, color: Color32, max_w: f32) -> (Rect, bool) {
    let mut job = egui::text::LayoutJob::simple_singleline(text.to_string(), font, color);
    job.wrap = egui::text::TextWrapping { max_width: max_w.max(20.0), max_rows: 1, break_anywhere: true, overflow_character: Some('…') };
    let g = painter.layout_job(job);
    let r = Rect::from_min_size(pos2(left_center.x, left_center.y - g.size().y / 2.0), g.size());
    let cut = g.elided;
    painter.galley(r.min, g, color);
    (r, cut)
}

// 小橫條上的一格：文字、字型、顏色；沒有就是空格。
type MiniCell = Option<(String, FontId, Color32)>;
// 小橫條總量的一項：標籤、百分比、是不是判斷依據、記憶體的「已用 / 總共」。
type WholeItem = (String, Option<f64>, bool, Option<String>);

// 有開個別上限的目標，小橫條上跟上限同一種的記憶體數字照離上限多近變色；其他照常色。
fn limit_color(cfg: &Config, r: &Row, p: &Pal, metric: Metric) -> Color32 {
    let Some(tg) = cfg.targets.iter().find(|t| t.exe.eq_ignore_ascii_case(&r.exe)) else {
        return p.mini_txt;
    };
    let l = tg.limit(metric);
    if !l.on {
        return p.mini_txt;
    }
    // 有先響的線就照它變橙，沒有才用上限的九成。
    let near = tg.warn_line(metric).unwrap_or(l.gb * LIMIT_NEAR);
    let v = if metric == Metric::Commit { r.commit } else { r.ws };
    match v {
        Some(v) if v >= l.gb => p.bad,
        Some(v) if v >= near => p.warn,
        _ => p.mini_txt,
    }
}

fn pct_text(v: Option<f64>) -> String {
    v.map_or("—".into(), |v| format!("{:.0}%", v))
}

fn card(ui: &mut Ui, p: &Pal, add: impl FnOnce(&mut Ui)) {
    egui::Frame::none()
        .fill(p.card)
        .stroke(Stroke::new(1.0, p.line))
        .rounding(10.0)
        .inner_margin(egui::Margin::same(14.0))
        .shadow(egui::epaint::Shadow { offset: vec2(0.0, 2.0), blur: 10.0, spread: 0.0, color: p.shadow })
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
}

fn header(ui: &mut Ui, p: &Pal, left: &str, right: &str) {
    let w = ui.available_width();
    let (r, _) = ui.allocate_exact_size(vec2(w, 16.0), Sense::hover());
    ui.painter().text(r.left_center(), Align2::LEFT_CENTER, left, f(12.0), p.dim);
    if !right.is_empty() {
        ui.painter().text(r.right_center(), Align2::RIGHT_CENTER, right, f(12.0), p.dim);
    }
}

fn pill(painter: &egui::Painter, pos: Pos2, text: &str, fg: Color32, bg: Color32) -> Rect {
    pill_sized(painter, pos, text, fg, bg, 1.0)
}

fn pill_sized(painter: &egui::Painter, pos: Pos2, text: &str, fg: Color32, bg: Color32, k: f32) -> Rect {
    let g = painter.layout_no_wrap(text.to_string(), f(11.0 * k), fg);
    let r = Rect::from_min_size(pos2(pos.x, pos.y - g.size().y / 2.0 - 2.0 * k), g.size() + vec2(16.0, 4.0) * k);
    painter.rect_filled(r, 999.0, bg);
    painter.galley(r.min + vec2(8.0, 2.0) * k, g, fg);
    r
}

// 小橫條上總量的進度條，從 left_center 往右畫；回傳右緣。
fn mini_bar(painter: &egui::Painter, left_center: Pos2, k: f32, pct: f64, c: &impl Fn(Color32) -> Color32, p: &Pal, frame: Color32) -> f32 {
    let bw = 70.0 * k;
    let br = Rect::from_min_size(pos2(left_center.x, left_center.y - 3.0 * k), vec2(bw, 6.0 * k));
    painter.rect_filled(br, 3.0 * k, c(p.mini_track));
    let fill = (bw * (pct / 100.0).clamp(0.0, 1.0) as f32).max(6.0 * k);
    painter.rect_filled(Rect::from_min_size(br.min, vec2(fill, 6.0 * k)), 3.0 * k, c(frame));
    br.right()
}

// 表格裡的小勾選框，畫在 center；點框附近一點也算。
fn check_cell(ui: &mut Ui, p: &Pal, id: Id, center: Pos2, on: &mut bool) {
    let r = Rect::from_center_size(center, vec2(20.0, 20.0));
    let resp = ui.interact(r.expand(6.0), id, Sense::click());
    if resp.clicked() {
        *on = !*on;
    }
    let painter = ui.painter();
    if *on {
        painter.rect_filled(r, 5.0, p.ok);
        let c = r.center();
        painter.add(Shape::line(vec![c + vec2(-5.0, 0.0), c + vec2(-1.5, 3.5), c + vec2(5.0, -4.0)], Stroke::new(2.0, Color32::WHITE)));
    } else {
        painter.rect(r, 5.0, if resp.hovered() { p.pill } else { p.well }, Stroke::new(1.0, p.line));
    }
}

fn toggle(ui: &mut Ui, p: &Pal, on: &mut bool) {
    let (rect, mut resp) = ui.allocate_exact_size(vec2(38.0, 22.0), Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let k = ui.ctx().animate_bool(resp.id, *on);
    let painter = ui.painter();
    painter.rect_filled(rect, 11.0, if *on { p.ok } else { p.line });
    let x = egui::lerp((rect.left() + 11.0)..=(rect.right() - 11.0), k);
    painter.circle_filled(pos2(x, rect.center().y + 0.5), 8.5, Color32::from_black_alpha(30));
    painter.circle_filled(pos2(x, rect.center().y), 8.0, Color32::WHITE);
}

// 膠囊多選一；回傳是否有改。
fn seg(ui: &mut Ui, p: &Pal, id: &str, opts: &[String], sel: &mut usize) -> bool {
    let painter = ui.painter().clone();
    let gs: Vec<_> = opts.iter().map(|o| painter.layout_no_wrap(o.clone(), f(12.0), p.txt)).collect();
    let ws: Vec<f32> = gs.iter().map(|g| g.size().x + 24.0).collect();
    let (rect, _) = ui.allocate_exact_size(vec2(ws.iter().sum::<f32>() + 6.0, 28.0), Sense::hover());
    painter.rect(rect, 999.0, p.well, Stroke::new(1.0, p.line));
    let mut x = rect.left() + 3.0;
    let mut changed = false;
    for (i, (g, w)) in gs.into_iter().zip(ws).enumerate() {
        let r = Rect::from_min_size(pos2(x, rect.top() + 3.0), vec2(w, rect.height() - 6.0));
        let resp = ui.interact(r, Id::new(("seg", id, i)), Sense::click());
        if resp.clicked() && *sel != i {
            *sel = i;
            changed = true;
        }
        let on = *sel == i;
        if on {
            painter.rect_filled(r.translate(vec2(0.0, 1.0)), 999.0, p.shadow);
            painter.rect_filled(r, 999.0, p.card);
        }
        let c = if on { p.strong } else { p.dim };
        painter.galley_with_override_text_color(r.center() - g.size() / 2.0, g, c);
        x += w;
    }
    changed
}

fn stepper(ui: &mut Ui, p: &Pal, id: &str, v: &mut f64, step: f64, lo: f64, hi: f64, text: String) -> bool {
    let (rect, _) = ui.allocate_exact_size(vec2(118.0, 28.0), Sense::hover());
    let painter = ui.painter();
    painter.rect(rect, 8.0, p.well, Stroke::new(1.0, p.line));
    let mid = Rect::from_min_max(pos2(rect.left() + 28.0, rect.top() + 1.0), pos2(rect.right() - 28.0, rect.bottom() - 1.0));
    painter.rect_filled(mid, 0.0, p.card);
    painter.text(mid.center(), Align2::CENTER_CENTER, text, fb(13.0), p.strong);
    let mut changed = false;
    for (dx, sign, glyph) in [(14.0, -1.0, "－"), (rect.width() - 14.0, 1.0, "＋")] {
        let c = pos2(rect.left() + dx, rect.center().y);
        let r = Rect::from_center_size(c, vec2(28.0, 28.0));
        let resp = ui.interact(r, Id::new(("step", id, dx as i32)), Sense::click());
        let col = if resp.hovered() { p.strong } else { p.dim };
        ui.painter().text(c, Align2::CENTER_CENTER, glyph, f(14.0), col);
        if resp.clicked() {
            let nv = ((*v + sign * step) / step).round() * step;
            *v = nv.clamp(lo, hi);
            changed = true;
        }
    }
    changed
}

fn item(ui: &mut Ui, p: &Pal, first: bool, title: &str, desc: &str, right: impl FnOnce(&mut Ui)) {
    if !first {
        let w = ui.available_width();
        let (r, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
        ui.painter().rect_filled(r, 0.0, p.line);
    }
    ui.add_space(if first { 0.0 } else { 8.0 });
    // 左邊文字最多佔一半寬，長的說明換行，換語言時才不會壓到右邊的控制項。
    let w = ui.available_width();
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_max_width(w * 0.5);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.add(egui::Label::new(egui::RichText::new(title).font(f(14.0)).color(p.txt)).wrap());
            if !desc.is_empty() {
                ui.add(egui::Label::new(egui::RichText::new(desc).font(f(12.0)).color(p.dim)).wrap());
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), right);
    });
    ui.add_space(8.0);
}

fn text_button(ui: &mut Ui, p: &Pal, text: &str, filled: bool, color: Color32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), f(13.0), color);
    let (r, resp) = ui.allocate_exact_size(g.size() + vec2(28.0, 12.0), Sense::click());
    let painter = ui.painter();
    if filled {
        painter.rect_filled(r, 8.0, if resp.hovered() { color.gamma_multiply(0.85) } else { color });
        painter.galley_with_override_text_color(r.center() - g.size() / 2.0, g, Color32::WHITE);
    } else {
        painter.rect(r, 8.0, if resp.hovered() { p.well } else { p.card }, Stroke::new(1.0, p.line));
        painter.galley_with_override_text_color(r.center() - g.size() / 2.0, g, color);
    }
    resp
}

fn big_button(ui: &mut Ui, text: &str, color: Color32) -> egui::Response {
    let g = ui.painter().layout_no_wrap(text.to_string(), fb(16.0), Color32::WHITE);
    let (r, resp) = ui.allocate_exact_size(g.size() + vec2(64.0, 24.0), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(r, 10.0, if resp.hovered() { color.gamma_multiply(0.85) } else { color });
    painter.galley_with_override_text_color(r.center() - g.size() / 2.0, g, Color32::WHITE);
    resp
}

// 一鍵開始的單選卡片：標題、可有可無的小標籤和說明，右邊一個圓點表示選中；回傳是否被點。
fn choice(ui: &mut Ui, p: &Pal, id: &str, title: &str, tag: &str, desc: &str, on: bool) -> bool {
    let bg = ui.painter().add(Shape::Noop);
    let w = ui.available_width();
    let rect = egui::Frame::none()
        .inner_margin(egui::Margin { left: 14.0, right: 44.0, top: 12.0, bottom: 12.0 })
        .show(ui, |ui| {
            ui.set_width(w - 58.0);
            ui.spacing_mut().item_spacing.y = 4.0;
            ui.horizontal_wrapped(|ui| {
                ui.label(egui::RichText::new(title).font(fb(15.0)).color(p.strong));
                if !tag.is_empty() {
                    let g = ui.painter().layout_no_wrap(tag.to_string(), f(11.0), p.ok);
                    let (r, _) = ui.allocate_exact_size(vec2(g.size().x + 16.0, 20.0), Sense::hover());
                    pill(ui.painter(), r.left_center(), tag, p.ok, p.ok.gamma_multiply(0.14));
                }
            });
            if !desc.is_empty() {
                ui.add(egui::Label::new(egui::RichText::new(desc).font(f(12.0)).color(p.dim)).wrap());
            }
        })
        .response
        .rect;
    let resp = ui.interact(rect, Id::new(("choice", id)), Sense::click());
    let fill = if on || resp.hovered() { p.pill } else { p.well };
    let stroke = if on { Stroke::new(2.0, p.accent(0)) } else { Stroke::new(1.0, p.line) };
    ui.painter().set(bg, Shape::Rect(egui::epaint::RectShape::new(rect, Rounding::same(8.0), fill, stroke)));
    let dot = pos2(rect.right() - 22.0, rect.center().y);
    if on {
        ui.painter().circle_filled(dot, 8.0, p.accent(0));
        ui.painter().circle_filled(dot, 3.0, Color32::WHITE);
    } else {
        ui.painter().circle_stroke(dot, 7.5, Stroke::new(1.5, p.line));
    }
    resp.clicked()
}

#[derive(Clone, Copy)]
enum Glyph {
    Mini,
    Pause,
    Play,
    Gear,
    Min,
    Close,
}

fn icon_button(ui: &mut Ui, p: &Pal, g: Glyph, tint: Option<Color32>) -> egui::Response {
    let (r, resp) = ui.allocate_exact_size(vec2(30.0, 26.0), Sense::click());
    let painter = ui.painter();
    if resp.hovered() {
        let bg = if matches!(g, Glyph::Close) { p.bad.gamma_multiply(0.25) } else { p.well };
        painter.rect_filled(r, 6.0, bg);
    }
    let c = r.center();
    let col = tint.unwrap_or(if resp.hovered() { p.strong } else { p.dim });
    let s = Stroke::new(1.4, col);
    match g {
        Glyph::Mini => {
            painter.rect_stroke(Rect::from_center_size(c, vec2(14.0, 7.0)), 3.5, s);
        }
        Glyph::Pause => {
            for dx in [-2.5, 2.5] {
                painter.rect_filled(Rect::from_center_size(c + vec2(dx, 0.0), vec2(2.2, 10.0)), 1.0, col);
            }
        }
        Glyph::Play => {
            painter.add(Shape::convex_polygon(vec![c + vec2(-3.5, -5.5), c + vec2(5.5, 0.0), c + vec2(-3.5, 5.5)], col, Stroke::NONE));
        }
        Glyph::Gear => {
            painter.circle_stroke(c, 3.2, s);
            for k in 0..8 {
                let a = k as f32 * std::f32::consts::TAU / 8.0;
                let d = vec2(a.cos(), a.sin());
                painter.line_segment([c + d * 4.8, c + d * 7.0], Stroke::new(2.0, col));
            }
            painter.circle_stroke(c, 5.2, Stroke::new(1.2, col));
        }
        Glyph::Min => {
            painter.line_segment([c - vec2(6.0, 0.0), c + vec2(6.0, 0.0)], s);
        }
        Glyph::Close => {
            painter.line_segment([c - vec2(5.0, 5.0), c + vec2(5.0, 5.0)], s);
            painter.line_segment([c - vec2(5.0, -5.0), c + vec2(5.0, -5.0)], s);
        }
    }
    resp
}

// 單一目標的設定：名稱、動作、認可與實際使用兩個個別上限（旁邊寫目前用量）、要不要上小橫條，
// 在跑時多一顆立即結束（按兩下才算）。主畫面就地編輯和設定頁共用這一份；回傳是否確認要立即結束。
fn target_editor(ui: &mut Ui, p: &Pal, tg: &mut Target, now: (Option<f64>, Option<f64>), armed: &mut bool) -> bool {
    let running = now.0.is_some();
    let id = tg.exe.to_lowercase();
    ui.spacing_mut().item_spacing.y = 0.0;
    item(ui, p, true, &t("name"), &tg.exe, |ui| {
        ui.add(egui::TextEdit::singleline(&mut tg.name).desired_width(140.0).font(f(13.0)));
    });
    item(ui, p, false, &t("kill"), "", |ui| {
        let mut sel = if tg.kill { 0 } else { 1 };
        if seg(ui, p, &format!("act-{}", id), &[t("act_kill"), t("act_warn")], &mut sel) {
            tg.kill = sel == 0;
        }
    });
    for (m, key, wkey, cur) in [(Metric::Commit, "limit_commit", "warn_commit", now.0), (Metric::Physical, "limit_phys", "warn_phys", now.1)] {
        let desc = cur.map_or(String::new(), |v| tf("cur_val", &[&format!("{:.1} GB", v)]));
        let l = tg.limit_mut(m);
        item(ui, p, false, &t(key), &desc, |ui| {
            toggle(ui, p, &mut l.on);
            let s = format!("{:.1} GB", l.gb);
            stepper(ui, p, &format!("{}-{}", key, id), &mut l.gb, 0.5, 0.5, 256.0, s);
        });
        // 只警告的目標上限本身就是警告線，不另外給先響的線。
        if tg.kill {
            let w = tg.warn_mut(m);
            item(ui, p, false, &t(wkey), &t("warn_line_d"), |ui| {
                toggle(ui, p, &mut w.on);
                let s = format!("{:.1} GB", w.gb);
                stepper(ui, p, &format!("{}-{}", wkey, id), &mut w.gb, 0.5, 0.5, 256.0, s);
            });
        }
    }
    item(ui, p, false, &t("show_mini"), "", |ui| {
        toggle(ui, p, &mut tg.mini);
    });
    let mut end = false;
    if running {
        item(ui, p, false, &t("end_now"), "", |ui| {
            let label = if *armed { t("end_confirm") } else { t("end_btn") };
            // 平常是紅字外框，按第一下後才變實心紅色等確認。
            if text_button(ui, p, &label, *armed, p.bad).clicked() {
                end = *armed;
                *armed = !*armed;
            }
        });
    }
    end
}

// 資料夾式分頁：選中的那頁跟下面內容框接在一起。回傳是否換了分頁。
fn folder_tabs(ui: &mut Ui, ctx: &egui::Context, p: &Pal, id: &str, labels: &[String], sel: &mut usize) -> bool {
    let w = ui.available_width();
    let (row, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
    let painter = ui.painter().clone();
    let mut x = row.left() + 6.0;
    let mut changed = false;
    for (i, label) in labels.iter().enumerate() {
        let g = painter.layout_no_wrap(label.clone(), f(13.0), p.txt);
        let r = Rect::from_min_size(pos2(x, row.top()), vec2(g.size().x + 28.0, row.height()));
        let resp = ui.interact(r, Id::new((id, i)), Sense::click());
        if resp.clicked() && *sel != i {
            *sel = i;
            changed = true;
        }
        let on = *sel == i;
        let round = Rounding { nw: 8.0, ne: 8.0, sw: 0.0, se: 0.0 };
        if on {
            painter.rect_filled(r, round, p.card);
            painter.rect_stroke(r, round, Stroke::new(1.0, p.line));
            // 內容框緊貼在分頁下面；把框線在選中分頁底下那一段蓋掉，兩者看起來是同一塊。
            ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new((id, "join")))).rect_filled(
                Rect::from_min_max(pos2(r.left() + 1.0, r.bottom() - 1.0), pos2(r.right() - 1.0, r.bottom() + 1.5)),
                0.0,
                p.card,
            );
        } else if resp.hovered() {
            painter.rect_filled(r, round, p.well);
        }
        let c = if on { p.strong } else { p.dim };
        painter.galley_with_override_text_color(r.center() - g.size() / 2.0, g, c);
        x = r.right() + 4.0;
    }
    changed
}

// 每個圖例是一個整塊，才會跟著寬度換行。
fn legends(ui: &mut Ui, p: &Pal, items: &[(Color32, String)]) {
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = vec2(14.0, 4.0);
        for (color, text) in items {
            let g = ui.painter().layout_no_wrap(text.clone(), f(12.0), p.dim);
            let (r, _) = ui.allocate_exact_size(vec2(17.0 + g.size().x, 16.0), Sense::hover());
            let painter = ui.painter();
            painter.rect_filled(Rect::from_center_size(pos2(r.left() + 6.0, r.center().y), vec2(12.0, 4.0)), 2.0, *color);
            painter.galley(pos2(r.left() + 17.0, r.center().y - g.size().y / 2.0), g, p.dim);
        }
    });
}

// ---------- 介面 ----------

enum Action {
    Drag,
    Hide,
    Close,
    Mini(bool),
}

// 一鍵開始的暫存：選好的程式、判斷依據、動作、警告音，和認可、實際使用兩列線（開不開、響、關掉）。
// 按「完成設定」才寫進設定，中途離開什麼都不存。
struct Wizard {
    step: usize,
    exe: String,
    path: String,
    name: String,
    q: String,
    basis: Metric,
    kill: bool,
    sound: Sound,
    lines: [(bool, f64, f64); 2],
}

const WIZ_STEPS: usize = 4;

// 依電腦實體記憶體先填的（響, 關掉）GB：取不超過這台的最大一檔，比最小一檔還小就照比例。
fn wizard_lines(total_gb: f64) -> (f64, f64) {
    const TIERS: [(f64, f64, f64); 4] = [(8.0, 4.0, 6.0), (16.0, 8.0, 12.0), (32.0, 12.0, 20.0), (64.0, 24.0, 40.0)];
    match TIERS.iter().rev().find(|t| t.0 <= total_gb + 1.0) {
        Some(t) => (t.1, t.2),
        None => ((total_gb * 0.5).round().max(1.0), (total_gb * 0.75).round().max(2.0)),
    }
}

struct Gui {
    hub: Arc<Hub>,
    wizard: Option<Wizard>,
    cfg: Config,
    draft: Config,
    draft_auto: bool,
    settings: bool,
    picker: bool,
    autostart: bool,
    confirm_reset: bool,
    confirm_leave: bool,
    mini: bool,
    lang: &'static str,
    tab: usize,
    picker_q: String,
    // 主畫面正在就地編輯的目標（小寫程式名）和它的暫存值。
    edit_row: Option<(String, Target)>,
    // 設定頁「目標」分頁裡展開的那一個。
    set_expand: Option<String>,
    // 「立即結束」已按第一下的目標（小寫程式名）。
    end_armed: Option<String>,
    // 主畫面分頁：0 總覽、1 所有程式；所有程式照哪一欄排。
    main_tab: usize,
    rank_by: RankBy,
    rank_desc: bool,
    rank_mb: bool,
    // 等待確認要結束的程式（小寫程式名, 顯示名稱）。
    confirm_end: Option<(String, String)>,
    update: Arc<Mutex<Update>>,
    is_admin: bool,
    icons: HashMap<String, Option<egui::TextureHandle>>,
    actions: Vec<Action>,
    want: egui::Vec2,
    // 小橫條目前的圓角半徑（邏輯點），視窗外形照它裁。
    mini_round: f32,
    applied_theme: Option<Theme>,
}

impl Gui {
    fn apply_cfg(&mut self, ctx: &egui::Context, new: Config) {
        let lang = i18n::resolve(&new.lang);
        if lang != self.lang {
            self.lang = lang;
            i18n::set(lang);
            setup_fonts(ctx, lang);
        }
        self.cfg = new;
        save_config(&self.cfg);
        self.hub.s.lock().unwrap().cfg = self.cfg.clone();
        self.hub.wake();
    }

    fn open_settings(&mut self) {
        self.wizard = None;
        self.draft = self.cfg.clone();
        self.draft_auto = self.autostart;
        self.settings = true;
        self.picker = false;
        self.confirm_reset = false;
        self.confirm_leave = false;
    }

    fn dirty(&self) -> bool {
        self.draft != self.cfg || self.draft_auto != self.autostart
    }

    // 有沒存的修改時，第一次離開只提醒，第二次才放棄。
    fn leave_settings(&mut self) {
        if self.dirty() && !self.confirm_leave {
            self.confirm_leave = true;
            return;
        }
        self.settings = false;
        self.picker = false;
    }

    fn request_end(&self, key: String) {
        self.hub.s.lock().unwrap().end_now = Some(key);
        self.hub.wake();
    }

    fn mute(&self) {
        self.hub.s.lock().unwrap().muted = true;
    }

    // 編輯區的立即結束：armed 只在這個目標身上；確認後交給量測執行緒去關。
    // now 是這個目標目前的（認可, 實際使用），沒在跑就是 None。
    fn edit_target(&mut self, ui: &mut Ui, p: &Pal, tg: &mut Target, now: (Option<f64>, Option<f64>)) {
        let key = tg.exe.to_lowercase();
        let mut armed = self.end_armed.as_deref() == Some(key.as_str());
        if target_editor(ui, p, tg, now, &mut armed) {
            self.request_end(key.clone());
        }
        if armed {
            self.end_armed = Some(key);
        } else if self.end_armed.as_deref() == Some(key.as_str()) {
            self.end_armed = None;
        }
    }

    fn icon(&mut self, ctx: &egui::Context, exe: &str, path: &str) -> Option<egui::TextureId> {
        let key = exe.to_lowercase();
        if !self.icons.contains_key(&key) || (self.icons[&key].is_none() && !path.is_empty()) {
            let tex = sys::icon_rgba(path).map(|(w, h, px)| {
                ctx.load_texture(format!("icon-{}", key), egui::ColorImage::from_rgba_unmultiplied([w, h], &px), egui::TextureOptions::LINEAR)
            });
            if tex.is_some() || !self.icons.contains_key(&key) {
                self.icons.insert(key.clone(), tex);
            }
        }
        self.icons.get(&key).and_then(|t| t.as_ref().map(|t| t.id()))
    }

    #[allow(clippy::too_many_arguments)]
    fn draw_icon(&mut self, ctx: &egui::Context, painter: &egui::Painter, rect: Rect, exe: &str, path: &str, name: &str, color: Color32, a: f32) {
        let white = Color32::WHITE.gamma_multiply(a);
        if let Some(id) = self.icon(ctx, exe, path) {
            painter.image(id, rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), white);
        } else {
            painter.circle_filled(rect.center(), rect.width() / 2.0, color.gamma_multiply(a));
            let ch: String = name.chars().next().map(|c| c.to_string()).unwrap_or_default();
            painter.text(rect.center(), Align2::CENTER_CENTER, ch, fb(rect.width() * 0.55), white);
        }
    }

    fn ui(&mut self, ctx: &egui::Context) {
        let p = palette(self.cfg.theme);
        if self.applied_theme != Some(self.cfg.theme) {
            ctx.set_visuals(visuals(self.cfg.theme, &p));
            self.applied_theme = Some(self.cfg.theme);
        }
        let (view, hist, st) = {
            let s = self.hub.s.lock().unwrap();
            (s.view.clone(), s.hist.clone(), s.status.clone())
        };
        // 目標的路徑記進設定，沒在跑也拿得到圖示；草稿也一起補，免得被當成有修改。
        let mut path_changed = false;
        for r in &view.rows {
            if r.path.is_empty() {
                continue;
            }
            for c in [&mut self.cfg, &mut self.draft] {
                if let Some(t) = c.targets.iter_mut().find(|t| t.exe.eq_ignore_ascii_case(&r.exe)) {
                    if t.path != r.path {
                        t.path = r.path.clone();
                        path_changed = true;
                    }
                }
            }
        }
        if path_changed {
            save_config(&self.cfg);
            self.hub.s.lock().unwrap().cfg = self.cfg.clone();
        }
        let frame = p.level(st.level);
        if self.mini {
            self.mini_ui(ctx, &p, &view, &st, frame);
        } else {
            self.full_ui(ctx, &p, &view, &hist, &st, frame);
        }
    }

    fn full_ui(&mut self, ctx: &egui::Context, p: &Pal, view: &View, hist: &VecDeque<Sample>, st: &Status, frame: Color32) {
        egui::TopBottomPanel::top("title")
            .exact_height(TITLE_H)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(p.title).inner_margin(egui::Margin::symmetric(14.0, 0.0)))
            .show(ctx, |ui| {
                let drag = ui.interact(ui.max_rect(), Id::new("title-drag"), Sense::click_and_drag());
                if drag.drag_started() {
                    self.actions.push(Action::Drag);
                }
                ui.horizontal_centered(|ui| {
                    ui.label(egui::RichText::new(t("app_title")).font(fb(13.0)).color(p.strong));
                    let (text, color) = if self.settings {
                        (t("st_settings"), p.ok)
                    } else if self.cfg.paused {
                        (t("st_paused"), if st.level == Level::Ok { p.warn } else { frame })
                    } else {
                        (st.title.clone(), frame)
                    };
                    // 警告中點狀態膠囊切換靜音；靜音到狀態再變差或再點一次為止。
                    let alarm = !self.settings && st.level != Level::Ok;
                    let muted = alarm && self.hub.s.lock().unwrap().muted;
                    let text = if muted { format!("{} · {}", text, t("muted")) } else { text };
                    let (r, _) = ui.allocate_exact_size(vec2(10.0, 20.0), Sense::hover());
                    let pr = pill(ui.painter(), pos2(r.left(), r.center().y), &format!("●  {}", text), color, color.gamma_multiply(0.16));
                    ui.allocate_exact_size(vec2(pr.width() - 10.0, 20.0), Sense::hover());
                    let tip = if muted { t("tip_unmute") } else { t("tip_mute") };
                    if alarm && ui.interact(pr, Id::new("pill-mute"), Sense::click()).on_hover_text(tip).clicked() {
                        self.hub.s.lock().unwrap().muted = !muted;
                        ui.ctx().request_repaint();
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        if icon_button(ui, p, Glyph::Close, None).clicked() {
                            self.actions.push(Action::Close);
                        }
                        if icon_button(ui, p, Glyph::Min, None).on_hover_text(t("tip_tray")).clicked() {
                            self.actions.push(Action::Hide);
                        }
                        if icon_button(ui, p, Glyph::Gear, None).on_hover_text(t("tip_settings")).clicked() {
                            if self.settings {
                                self.leave_settings();
                            } else {
                                self.open_settings();
                            }
                        }
                        let (g, tint, tip) = if self.cfg.paused {
                            (Glyph::Play, Some(p.warn), t("tip_resume"))
                        } else {
                            (Glyph::Pause, None, t("tip_pause"))
                        };
                        if icon_button(ui, p, g, tint).on_hover_text(tip).clicked() {
                            let mut c = self.cfg.clone();
                            c.paused = !c.paused;
                            self.draft.paused = c.paused;
                            self.apply_cfg(ctx, c);
                        }
                        if icon_button(ui, p, Glyph::Mini, None).on_hover_text(t("tip_mini")).clicked() {
                            self.actions.push(Action::Mini(true));
                        }
                    });
                });
            });

        if self.settings {
            self.settings_bar(ctx, p);
        } else if self.wizard.is_none() {
            self.main_tabs(ctx, p);
        }

        // 視窗固定高度；內容放不下時在框內捲動。
        let margin = egui::Margin { left: 14.0, right: 14.0, top: 0.0, bottom: 14.0 };
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(p.win).inner_margin(margin))
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = vec2(8.0, 12.0);
                if self.settings {
                    self.settings_page(ui, ctx, p, view, st, frame);
                } else if self.wizard.is_some() {
                    self.wizard_page(ui, ctx, p, view);
                } else if self.main_tab == 0 {
                    egui::ScrollArea::vertical().id_salt("overview").auto_shrink([false, false]).show(ui, |ui| {
                        ui.spacing_mut().item_spacing = vec2(8.0, 12.0);
                        self.main_page(ui, ctx, p, view, hist, st, frame);
                    });
                } else {
                    self.rank_page(ui, ctx, p, view);
                }
                self.want = vec2(WIN_W, MAX_H);
            });

        self.confirm_end_dialog(ctx, p);

        // 平常不描邊；警告或動手時才用狀態色框起來。
        if st.level != Level::Ok {
            ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, Id::new("frame")))
                .rect_stroke(ctx.screen_rect().shrink(1.0), 12.0, Stroke::new(2.0, frame));
        }
    }

    // 設定頁頂端固定列：左還原預設、右儲存，不跟著捲動。
    fn settings_bar(&mut self, ctx: &egui::Context, p: &Pal) {
        egui::TopBottomPanel::top("settings-bar")
            .exact_height(BAR_H)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(p.win).inner_margin(egui::Margin { left: 14.0, right: 14.0, top: 10.0, bottom: 0.0 }))
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    let label = if self.confirm_reset { t("reset_confirm") } else { t("reset") };
                    let color = if self.confirm_reset { p.bad } else { p.txt };
                    if text_button(ui, p, &label, false, color).clicked() {
                        if self.confirm_reset {
                            self.draft = self.draft.reset_options();
                            self.confirm_reset = false;
                        } else {
                            self.confirm_reset = true;
                        }
                    }
                    let dirty = self.dirty();
                    if self.confirm_leave && dirty {
                        ui.label(egui::RichText::new(t("unsaved_leave")).font(f(12.0)).color(p.warn));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let color = if dirty { p.ok } else { p.line };
                        if text_button(ui, p, &t("save"), true, color).clicked() && dirty {
                            if self.draft_auto != self.autostart {
                                autostart_set(self.draft_auto);
                                self.autostart = autostart_get();
                                self.draft_auto = self.autostart;
                            }
                            let new = self.draft.clone();
                            self.apply_cfg(ctx, new);
                            self.confirm_leave = false;
                            self.confirm_reset = false;
                        }
                    });
                });
                ui.add_space(10.0);
                let labels: Vec<String> = ["tab_targets", "tab_thr", "tab_general", "sec_mini", "tab_about"].iter().map(|k| t(k)).collect();
                if folder_tabs(ui, ctx, p, "set-tab", &labels, &mut self.tab) {
                    self.picker = false;
                }
            });
    }

    // 主畫面頂端的分頁：總覽、排行。
    fn main_tabs(&mut self, ctx: &egui::Context, p: &Pal) {
        egui::TopBottomPanel::top("main-tabs")
            .exact_height(TAB_H)
            .show_separator_line(false)
            .frame(egui::Frame::none().fill(p.win).inner_margin(egui::Margin { left: 14.0, right: 14.0, top: 10.0, bottom: 0.0 }))
            .show(ctx, |ui| {
                let labels = [t("tab_overview"), t("tab_rank")];
                folder_tabs(ui, ctx, p, "main-tab", &labels, &mut self.main_tab);
            });
    }

    #[allow(clippy::too_many_arguments)]
    fn main_page(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Pal, v: &View, hist: &VecDeque<Sample>, st: &Status, frame: Color32) {
        if !v.ready {
            card(ui, p, |ui| {
                ui.label(egui::RichText::new(t("measuring")).font(f(14.0)).color(p.dim));
            });
            return;
        }
        // 畫面看哪一種由切換鈕決定；狀態、外框、警告永遠照判斷依據。
        let view = self.cfg.view;
        let basis = self.cfg.basis;
        let same = view == basis;
        let (used, cap) = v.whole(view);
        let pct = used / cap * 100.0;
        let (bused, bcap) = v.whole(basis);
        let bpct = bused / bcap * 100.0;
        let warn_gb = cap * self.cfg.warn_pct / 100.0;
        let cfg = self.cfg.clone();
        let mut new_view = view;

        card(ui, p, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 18.0;
                let (rr, _) = ui.allocate_exact_size(vec2(120.0, 120.0), Sense::hover());
                let painter = ui.painter();
                let c = rr.center();
                let frac = (pct / 100.0).clamp(0.0, 1.0) as f32;
                // 底圈和進度用同一條算法，兩條線才會疊在同一個圓上。
                let arc = |frac: f32| -> Vec<Pos2> {
                    let n = ((frac * 160.0) as usize).max(2);
                    (0..=n)
                        .map(|i| {
                            let a = -std::f32::consts::FRAC_PI_2 + frac * std::f32::consts::TAU * i as f32 / n as f32;
                            c + vec2(a.cos(), a.sin()) * 50.0
                        })
                        .collect()
                };
                let mut ring = arc(1.0);
                ring.pop();
                painter.add(Shape::closed_line(ring, Stroke::new(12.0, p.well)));
                let pts = arc(frac);
                if frac > 0.0 {
                    painter.circle_filled(pts[0], 6.0, frame);
                    painter.circle_filled(*pts.last().unwrap(), 6.0, frame);
                    painter.add(Shape::line(pts, Stroke::new(12.0, frame)));
                }
                painter.text(c - vec2(0.0, 7.0), Align2::CENTER_CENTER, format!("{:.0}%", pct), fb(30.0), p.strong);
                painter.text(c + vec2(0.0, 18.0), Align2::CENTER_CENTER, view.name(), f(11.0), p.dim);

                ui.vertical(|ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    ui.add_space(6.0);
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 6.0;
                        ui.label(egui::RichText::new(format!("{:.1}", used)).font(fb(22.0)).color(p.strong));
                        ui.label(egui::RichText::new(format!("/ {:.1} GB", cap)).font(f(13.0)).color(p.dim));
                        let mut usage = format!("CPU {}", pct_text(v.cpu));
                        if cfg.gpu {
                            usage += &format!("  GPU {}", pct_text(v.gpu));
                        }
                        ui.label(egui::RichText::new(usage).font(f(12.0)).color(p.dim));
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            let mut sel = if view == Metric::Physical { 0 } else { 1 };
                            if seg(ui, p, "view", &[t("m_phys"), t("m_commit")], &mut sel) {
                                new_view = if sel == 0 { Metric::Physical } else { Metric::Commit };
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 4.0;
                        if !same {
                            ui.label(egui::RichText::new(tf("basis_is", &[&basis.name()])).font(f(13.0)).color(p.dim));
                            ui.label(egui::RichText::new(format!("{:.0}%", bpct)).font(fb(13.0)).color(frame));
                            if st.level != Level::Ok && !st.msg.is_empty() {
                                ui.label(egui::RichText::new(format!("・{}", st.msg)).font(fb(13.0)).color(frame));
                            }
                        } else if st.level == Level::Ok && used < warn_gb {
                            ui.label(egui::RichText::new(t("to_warn")).font(f(13.0)).color(p.dim));
                            ui.label(egui::RichText::new(format!("{:.1} GB", warn_gb - used)).font(fb(13.0)).color(frame));
                        } else if !st.msg.is_empty() {
                            ui.label(egui::RichText::new(&st.msg).font(fb(13.0)).color(frame));
                        }
                    });
                    let w = ui.available_width();
                    let (tr, _) = ui.allocate_exact_size(vec2(w, 8.0), Sense::hover());
                    let painter = ui.painter();
                    let xw = tr.left() + tr.width() * (cfg.warn_pct / 100.0) as f32;
                    let xk = tr.left() + tr.width() * (cfg.kill_pct / 100.0) as f32;
                    painter.rect_filled(tr, 4.0, p.well);
                    // 門檻色塊只在畫面看的就是判斷依據時才畫，避免拿錯的數字比門檻。
                    if same {
                        painter.rect_filled(Rect::from_min_max(pos2(xw, tr.top()), pos2(xk, tr.bottom())), 0.0, p.warn.gamma_multiply(0.3));
                        painter.rect_filled(
                            Rect::from_min_max(pos2(xk, tr.top()), tr.max),
                            Rounding { nw: 0.0, sw: 0.0, ne: 4.0, se: 4.0 },
                            p.bad.gamma_multiply(0.3),
                        );
                    }
                    let fw = (tr.width() * frac).max(8.0);
                    painter.rect_filled(Rect::from_min_size(tr.min, vec2(fw, tr.height())), 4.0, if same { frame } else { p.sys });
                    if same {
                        // 兩個門檻靠很近時，警告的字往左讓開，不跟動手的字疊在一起。
                        let (lr, _) = ui.allocate_exact_size(vec2(w, 14.0), Sense::hover());
                        let kr = ui.painter().text(pos2(lr.right(), lr.top()), Align2::RIGHT_TOP, tf("thr_kill", &[&format!("{:.0}", cfg.kill_pct)]), f(10.0), p.dim);
                        let wx = (xw - 2.0).min(kr.left() - 8.0);
                        ui.painter().text(pos2(wx, lr.top()), Align2::RIGHT_TOP, tf("thr_warn", &[&format!("{:.0}", cfg.warn_pct)]), f(10.0), p.dim);
                    }
                });
            });
        });
        if new_view != view {
            self.cfg.view = new_view;
            self.draft.view = new_view;
            save_config(&self.cfg);
            self.hub.s.lock().unwrap().cfg = self.cfg.clone();
        }

        // 還沒有目標時一鍵開始是大按鈕；有目標後縮成清單底下一行字。
        let mut start = false;
        card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            header(ui, p, &t("targets"), "");
            if v.rows.is_empty() {
                ui.vertical_centered(|ui| {
                    ui.add_space(6.0);
                    ui.label(egui::RichText::new(t("wiz_empty")).font(f(13.0)).color(p.dim));
                    ui.add_space(4.0);
                    start = big_button(ui, &t("wiz_start"), p.accent(0)).clicked();
                    ui.add_space(6.0);
                });
            }
            if v.rows.len() > TARGETS_SHOWN {
                let h = TARGETS_SHOWN as f32 * (ROW_H + 8.0) - 8.0;
                egui::ScrollArea::vertical().id_salt("targets").max_height(h).auto_shrink([false, true]).show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = 8.0;
                    self.target_rows(ui, ctx, p, v, &cfg);
                });
            } else {
                self.target_rows(ui, ctx, p, v, &cfg);
            }
            if !v.rows.is_empty() {
                let link = ui.add(egui::Label::new(egui::RichText::new(format!("＋ {}", t("wiz_start"))).font(f(13.0)).color(p.accent(0))).sense(Sense::click()));
                start |= link.clicked();
            }
        });
        if start {
            self.start_wizard();
        }

        card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            header(ui, p, &t("recent"), &format!("{} · GB", view.name()));
            let w = ui.available_width();
            let (cr, resp) = ui.allocate_exact_size(vec2(w, 100.0), Sense::hover());
            chart(&ui.painter().clone(), cr, p, v, hist, &cfg, resp.hover_pos());
            let mut items = vec![(p.sys, t("total"))];
            for (i, r) in v.rows.iter().enumerate() {
                items.push((p.accent(i), r.name.clone()));
            }
            legends(ui, p, &items);
        });

        // 吃最多的前三名，照判斷依據排；量測那邊已經排好序。
        card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 4.0;
            header(ui, p, &if basis == Metric::Commit { t("top_commit") } else { t("top_phys") }, "GB");
            for (i, r) in v.running.iter().take(3).enumerate() {
                let w = ui.available_width();
                let (rr, _) = ui.allocate_exact_size(vec2(w, 18.0), Sense::hover());
                let painter = ui.painter().clone();
                let ir = Rect::from_center_size(pos2(rr.left() + 8.0, rr.center().y), vec2(16.0, 16.0));
                self.draw_icon(ctx, &painter, ir, &r.exe, &r.path, &r.name, p.accent(i), 1.0);
                painter.text(pos2(ir.right() + 8.0, rr.center().y), Align2::LEFT_CENTER, &r.name, f(13.0), p.txt);
                let val = if basis == Metric::Commit { r.commit } else { r.ws };
                painter.text(rr.right_center(), Align2::RIGHT_CENTER, format!("{:.1}", val), fb(13.0), p.strong);
            }
        });
    }

    // 目標清單的每一列；點一列在底下展開它的設定。
    fn target_rows(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Pal, v: &View, cfg: &Config) {
            for (i, row) in v.rows.iter().enumerate() {
                let w = ui.available_width();
                let key = row.exe.to_lowercase();
                let open = self.edit_row.as_ref().map_or(false, |(k, _)| *k == key);
                let (rr, resp) = ui.allocate_exact_size(vec2(w, ROW_H), Sense::click());
                if resp.clicked() {
                    // 點一下展開這一列的設定，再點一下或按取消收起來。
                    self.end_armed = None;
                    self.edit_row = if open {
                        None
                    } else {
                        self.cfg.targets.iter().find(|t| t.exe.to_lowercase() == key).map(|t| (key.clone(), t.clone()))
                    };
                }
                let painter = ui.painter().clone();
                painter.rect_filled(rr, 8.0, if resp.hovered() || open { p.pill } else { p.well });
                let ir = Rect::from_center_size(pos2(rr.left() + 12.0 + 11.0, rr.center().y), vec2(22.0, 22.0));
                self.draw_icon(ctx, &painter, ir, &row.exe, &row.path, &row.name, p.accent(i), 1.0);
                let (txt, fg, bg) = if row.kill {
                    (t("tag_kill"), p.bad, p.bad.gamma_multiply(0.12))
                } else {
                    (t("tag_warn"), p.dim, p.pill)
                };
                // 名稱加標籤要停在「實際佔用」那欄前面；太長就截斷，滑鼠停上去看全名。
                let tag_w = painter.layout_no_wrap(txt.clone(), f(11.0), fg).size().x + 16.0;
                let name_w = rr.right() - 290.0 - (ir.right() + 8.0) - tag_w - 8.0;
                let (nr, cut) = text_elided(&painter, pos2(ir.right() + 8.0, rr.center().y), &row.name, fb(15.0), p.strong, name_w);
                if cut {
                    resp.clone().on_hover_text(&row.name);
                }
                pill(&painter, pos2(nr.right() + 8.0, rr.center().y), &txt, fg, bg);
                let cols = [
                    (rr.right() - 12.0, t("col_d10"), delta_text(row.d10), delta_color(row, p, p.strong)),
                    (rr.right() - 112.0, t("col_commit"), gb_text(row.commit), p.strong),
                    (rr.right() - 212.0, t("col_ws"), gb_text(row.ws), p.strong),
                ];
                for (x, label, val, col) in &cols {
                    painter.text(pos2(*x, rr.top() + 9.0), Align2::RIGHT_TOP, label, f(11.0), p.dim);
                    painter.text(pos2(*x, rr.top() + 25.0), Align2::RIGHT_TOP, val, fb(16.0), *col);
                }
                // CPU、GPU 用量放在兩個記憶體數字底下：CPU 對齊實際佔用，GPU 對齊認可大小。
                let mut usage = vec![(cols[2].0, format!("CPU {}", pct_text(row.cpu)))];
                if cfg.gpu {
                    usage.push((cols[1].0, format!("GPU {}", pct_text(row.gpu))));
                }
                for (x, s) in usage {
                    painter.text(pos2(x, rr.bottom() - 9.0), Align2::RIGHT_BOTTOM, s, f(11.0), p.dim);
                }
                if open {
                    let mut done: Option<bool> = None;
                    let now = (row.commit, row.ws);
                    if let Some((k, mut tg)) = self.edit_row.take() {
                        // 展開的設定包成一塊有框的小卡，跟上面那一列看得出是同一個目標。
                        egui::Frame::none()
                            .fill(p.card)
                            .stroke(Stroke::new(1.0, p.line))
                            .rounding(8.0)
                            .inner_margin(egui::Margin::same(12.0))
                            .show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.edit_target(ui, p, &mut tg, now);
                            ui.horizontal(|ui| {
                                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                    if text_button(ui, p, &t("edit_save"), true, p.ok).clicked() {
                                        done = Some(true);
                                    }
                                    if text_button(ui, p, &t("edit_cancel"), false, p.txt).clicked() {
                                        done = Some(false);
                                    }
                                });
                            });
                        });
                        self.edit_row = Some((k, tg));
                    }
                    if done.is_some() {
                        self.end_armed = None;
                    }
                    match done {
                        Some(true) => {
                            if let Some((k, tg)) = self.edit_row.take() {
                                let mut c = self.cfg.clone();
                                if let Some(slot) = c.targets.iter_mut().find(|t| t.exe.to_lowercase() == k) {
                                    *slot = tg.clone();
                                }
                                if let Some(slot) = self.draft.targets.iter_mut().find(|t| t.exe.to_lowercase() == k) {
                                    *slot = tg;
                                }
                                self.apply_cfg(ctx, c);
                            }
                        }
                        Some(false) => self.edit_row = None,
                        None => {}
                    }
                }
            }
    }

    // 排行分頁：所有在跑的程式照選的欄位由大到小，點欄位標題換排序。
    fn rank_page(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Pal, v: &View) {
        let gpu_on = self.cfg.gpu;
        if !gpu_on && self.rank_by == RankBy::Gpu {
            self.rank_by = RankBy::Physical;
        }
        card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            // 記憶體兩欄的單位：小程式多半不到 1 GB，用 MB 看得比較清楚。
            let w = ui.available_width();
            ui.allocate_ui_with_layout(vec2(w, 28.0), egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let mut sel = self.rank_mb as usize;
                if seg(ui, p, "rank-unit", &["GB".to_string(), "MB".to_string()], &mut sel) {
                    self.rank_mb = sel == 1;
                }
            });
            ui.add_space(6.0);
            let mb = self.rank_mb;
            let mem = |gb: f64| if mb { format!("{:.0} MB", gb * 1024.0) } else { format!("{:.2} GB", gb) };
            let mut cols = vec![(RankBy::Physical, t("m_phys")), (RankBy::Commit, t("m_commit")), (RankBy::Cpu, "CPU".to_string())];
            if gpu_on {
                cols.push((RankBy::Gpu, "GPU".to_string()));
            }
            const COL_W: f32 = 80.0;
            let w = ui.available_width();
            // 數值欄由右往左排，第 n 欄的右緣。
            let col_right = |left: f32, n: usize| left + w - 8.0 - (cols.len() - 1 - n) as f32 * COL_W;
            let (hr, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
            let painter = ui.painter().clone();
            // 點欄位標題照它排；再點同一欄就反過來。名稱預設由 A 到 Z，數值預設由大到小。
            let name_g = painter.layout_no_wrap(format!("{} ▲", t("rank_name")), fb(12.0), p.dim);
            let name_r = Rect::from_min_size(pos2(hr.left() + 2.0, hr.top() + 2.0), vec2(name_g.size().x + 16.0, hr.height() - 4.0));
            let headers = std::iter::once((RankBy::Name, t("rank_name"), name_r, Align2::LEFT_CENTER, name_r.left() + 6.0)).chain(cols.iter().enumerate().map(|(n, (by, label))| {
                let right = col_right(hr.left(), n);
                let r = Rect::from_min_max(pos2(right - COL_W + 10.0, hr.top() + 2.0), pos2(right + 6.0, hr.bottom() - 2.0));
                (*by, label.clone(), r, Align2::RIGHT_CENTER, right)
            }));
            for (n, (by, label, r, align, tx)) in headers.enumerate() {
                let resp = ui.interact(r, Id::new(("rank-col", n)), Sense::click());
                if resp.clicked() {
                    if self.rank_by == by {
                        self.rank_desc = !self.rank_desc;
                    } else {
                        self.rank_by = by;
                        self.rank_desc = by != RankBy::Name;
                    }
                }
                let on = self.rank_by == by;
                if on || resp.hovered() {
                    painter.rect_filled(r, 6.0, p.well);
                }
                let text = if on { format!("{} {}", label, if self.rank_desc { "▼" } else { "▲" }) } else { label };
                painter.text(pos2(tx, hr.center().y), align, text, if on { fb(12.0) } else { f(12.0) }, if on { p.strong } else { p.dim });
            }
            let (lr, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
            painter.rect_filled(lr, 0.0, p.line);

            let (by, desc) = (self.rank_by, self.rank_desc);
            let mut confirm = None;
            let mut list: Vec<&Running> = v.running.iter().collect();
            list.sort_by(|a, b| if desc { b.cmp(a, by) } else { a.cmp(b, by) });
            egui::ScrollArea::vertical().id_salt("rank").auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                for (i, r) in list.iter().take(RANK_ROWS).enumerate() {
                    let (rr, resp) = ui.allocate_exact_size(vec2(w, 32.0), Sense::click());
                    let painter = ui.painter().clone();
                    if resp.hovered() || resp.context_menu_opened() {
                        painter.rect_filled(rr, 6.0, p.well);
                    }
                    let ir = Rect::from_center_size(pos2(rr.left() + 17.0, rr.center().y), vec2(18.0, 18.0));
                    self.draw_icon(ctx, &painter, ir, &r.exe, &r.path, &r.name, p.accent(i), 1.0);
                    // 名稱太長就截斷在第一個數值欄前面，滑鼠停上去看全名。
                    let name_w = col_right(rr.left(), 0) - COL_W - (ir.right() + 8.0);
                    let (_, cut) = text_elided(&painter, pos2(ir.right() + 8.0, rr.center().y), &r.name, f(13.0), p.txt, name_w);
                    let resp = if cut { resp.on_hover_text(&r.name) } else { resp };
                    // 右鍵：結束工作（先確認；系統程式不行）、複製執行檔名、在檔案總管打開所在資料夾。
                    resp.context_menu(|ui| {
                        if ui.add_enabled(!r.system, egui::Button::new(t("ctx_end"))).clicked() {
                            confirm = Some((r.exe.to_lowercase(), r.name.clone()));
                            ui.close_menu();
                        }
                        if ui.button(t("ctx_copy")).clicked() {
                            sys::copy_text(&r.exe);
                            ui.close_menu();
                        }
                        if ui.add_enabled(!r.path.is_empty(), egui::Button::new(t("ctx_folder"))).clicked() {
                            let _ = Command::new("explorer").raw_arg(format!("/select,\"{}\"", r.path)).spawn();
                            ui.close_menu();
                        }
                    });
                    for (n, (c, _)) in cols.iter().enumerate() {
                        let s = match c {
                            RankBy::Name => String::new(),
                            RankBy::Physical => mem(r.ws),
                            RankBy::Commit => mem(r.commit),
                            RankBy::Cpu => pct_text(r.cpu),
                            RankBy::Gpu => pct_text(r.gpu),
                        };
                        let on = *c == by;
                        painter.text(pos2(col_right(rr.left(), n), rr.center().y), Align2::RIGHT_CENTER, s, if on { fb(13.0) } else { f(13.0) }, if on { p.strong } else { p.txt });
                    }
                }
            });
            if confirm.is_some() {
                self.confirm_end = confirm;
            }
        });
    }

    fn start_wizard(&mut self) {
        self.edit_row = None;
        self.end_armed = None;
        self.wizard = Some(Wizard {
            step: 0,
            exe: String::new(),
            path: String::new(),
            name: String::new(),
            q: String::new(),
            basis: self.cfg.basis,
            kill: false,
            sound: Sound::Repeat,
            lines: [(false, 0.0, 0.0); 2],
        });
    }

    // 選好程式就決定名稱；已經在監看的沿用它原本的名稱，完成時改它那一筆。
    fn wizard_pick(&self, w: &mut Wizard, r: &Running) {
        w.exe = r.exe.clone();
        w.path = r.path.clone();
        w.name = self.cfg.targets.iter().find(|t| t.exe.eq_ignore_ascii_case(&r.exe)).map_or_else(|| strip_exe(&r.exe), |t| t.name.clone());
        w.q.clear();
    }

    // 選了判斷依據就重填兩列線：跟依據同一種的那列打開，另一列關著。
    fn wizard_basis(w: &mut Wizard, m: Metric, phys_total: f64) {
        w.basis = m;
        let (warn, kill) = wizard_lines(phys_total);
        w.lines = [(m == Metric::Commit, warn, kill), (m == Metric::Physical, warn, kill)];
    }

    // 寫進設定：判斷依據與警告音是整個程式共用的；目標沒有就新增，有就改那一筆，並顯示在小橫條。
    // 關掉的目標，關掉的線放上限、響的線放先響的線；只警告的目標，響的線本身就是上限。
    fn wizard_apply(&mut self, ctx: &egui::Context, w: &Wizard) {
        let mut c = self.cfg.clone();
        c.basis = w.basis;
        c.sound = w.sound;
        let i = match c.targets.iter().position(|t| t.exe.eq_ignore_ascii_case(&w.exe)) {
            Some(i) => i,
            None => {
                c.targets.push(Target::new(&w.exe, &w.path));
                c.targets.len() - 1
            }
        };
        let tg = &mut c.targets[i];
        tg.kill = w.kill;
        tg.mini = true;
        for (j, m) in [(0, Metric::Commit), (1, Metric::Physical)] {
            let (on, warn, kill) = w.lines[j];
            let (lim, wl) = if w.kill { (Limit { on, gb: kill }, Limit { on, gb: warn }) } else { (Limit { on, gb: warn }, Limit { on: false, gb: warn }) };
            *tg.limit_mut(m) = lim;
            *tg.warn_mut(m) = wl;
        }
        self.draft = c.clone();
        self.apply_cfg(ctx, c);
    }

    // 一鍵開始：選程式、選怕哪種狀況、選怎麼處理與怎麼響、調兩列線、完成。右上叉叉隨時離開不存。
    fn wizard_page(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Pal, v: &View) {
        let Some(mut w) = self.wizard.take() else { return };
        // 截圖用的 --wizard=N 直接跳到第 N 步，替它選吃最多的那個程式。
        if w.step > 0 && w.exe.is_empty() {
            if let Some(r) = v.running.iter().find(|r| !r.system) {
                self.wizard_pick(&mut w, r);
                let m = w.basis;
                Self::wizard_basis(&mut w, m, v.phys_total);
            }
        }
        let mut close = false;
        let mut apply = false;
        egui::ScrollArea::vertical().id_salt("wizard").auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 12.0);
            card(ui, p, |ui| {
                ui.spacing_mut().item_spacing.y = 10.0;
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(t("wiz_start")).font(fb(13.0)).color(p.dim));
                    if w.step < WIZ_STEPS {
                        ui.label(egui::RichText::new(tf("wiz_step", &[&(w.step + 1).to_string(), &WIZ_STEPS.to_string()])).font(f(12.0)).color(p.dim));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if icon_button(ui, p, Glyph::Close, None).on_hover_text(t("wiz_close")).clicked() {
                            close = true;
                        }
                    });
                });
                let q = ["wiz_q_pick", "wiz_q_mode", "wiz_q_act", if w.kill { "wiz_q_lines_kill" } else { "wiz_q_lines" }];
                let title = if w.step < WIZ_STEPS { t(q[w.step]) } else { tf("wiz_done", &[&w.name]) };
                ui.add(egui::Label::new(egui::RichText::new(title).font(fb(18.0)).color(p.strong)).wrap());
                match w.step {
                    0 => {
                        if !v.ready {
                            ui.label(egui::RichText::new(t("measuring")).font(f(13.0)).color(p.dim));
                            return;
                        }
                        ui.label(egui::RichText::new(t("wiz_pick_d")).font(f(12.0)).color(p.dim));
                        ui.add(egui::TextEdit::singleline(&mut w.q).hint_text(t("search")).desired_width(f32::INFINITY).font(f(13.0)));
                        let q = w.q.trim().to_lowercase();
                        let mut picked = None;
                        ui.spacing_mut().item_spacing.y = 4.0;
                        for (i, r) in v.running.iter().filter(|r| !r.system).filter(|r| q.is_empty() || r.exe.to_lowercase().contains(&q)).take(30).enumerate() {
                            let wd = ui.available_width();
                            let (rr, resp) = ui.allocate_exact_size(vec2(wd, 40.0), Sense::click());
                            let painter = ui.painter().clone();
                            painter.rect_filled(rr, 8.0, if resp.hovered() { p.pill } else { p.well });
                            let ir = Rect::from_center_size(pos2(rr.left() + 22.0, rr.center().y), vec2(24.0, 24.0));
                            self.draw_icon(ctx, &painter, ir, &r.exe, &r.path, &r.exe, p.accent(i), 1.0);
                            let nr = painter.text(pos2(ir.right() + 10.0, rr.center().y), Align2::LEFT_CENTER, &r.exe, fb(14.0), p.strong);
                            if self.cfg.targets.iter().any(|t| t.exe.eq_ignore_ascii_case(&r.exe)) {
                                pill(&painter, pos2(nr.right() + 8.0, rr.center().y), &t("wiz_watching"), p.dim, p.card);
                            }
                            painter.text(pos2(rr.right() - 12.0, rr.center().y), Align2::RIGHT_CENTER, format!("{:.1} GB", r.val_of(w.basis)), f(13.0), p.dim);
                            if resp.clicked() {
                                picked = Some(r.clone());
                            }
                        }
                        if let Some(r) = picked {
                            self.wizard_pick(&mut w, &r);
                            let m = w.basis;
                            Self::wizard_basis(&mut w, m, v.phys_total);
                            w.step = 1;
                        }
                    }
                    1 => {
                        for (m, key, tag) in [(Metric::Commit, "wiz_m_commit", t("wiz_m_commit_tag")), (Metric::Physical, "wiz_m_phys", String::new())] {
                            if choice(ui, p, key, &t(key), &tag, "", w.basis == m) {
                                Self::wizard_basis(&mut w, m, v.phys_total);
                                w.step = 2;
                            }
                        }
                    }
                    2 => {
                        if choice(ui, p, "act-warn", &t("wiz_a_warn"), "", &t("wiz_a_warn_d"), !w.kill) {
                            w.kill = false;
                        }
                        if choice(ui, p, "act-kill", &t("wiz_a_kill"), "", &tf("wiz_a_kill_d", &[&w.name]), w.kill) {
                            w.kill = true;
                        }
                        ui.horizontal(|ui| {
                            ui.label(egui::RichText::new(t("wiz_q_sound")).font(fb(15.0)).color(p.strong));
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if text_button(ui, p, &t("snd_test"), false, p.txt).clicked() {
                                    sys::play_alert();
                                }
                            });
                        });
                        for (s, key, tag) in [(Sound::Repeat, "wiz_s_repeat", t("wiz_rec")), (Sound::Once, "wiz_s_once", String::new())] {
                            if choice(ui, p, key, &t(key), &tag, &t(&format!("{}_d", key)), w.sound == s) {
                                w.sound = s;
                            }
                        }
                    }
                    3 => {
                        ui.add(egui::Label::new(egui::RichText::new(tf("wiz_ram", &[&format!("{:.0}", v.phys_total)])).font(f(13.0)).color(p.dim)).wrap());
                        let cur = v.running.iter().find(|r| r.exe.eq_ignore_ascii_case(&w.exe));
                        for (j, m, key) in [(0, Metric::Commit, "wiz_l_commit"), (1, Metric::Physical, "wiz_l_phys")] {
                            let kill = w.kill;
                            let name = w.name.clone();
                            let (on, warn, off) = &mut w.lines[j];
                            egui::Frame::none().fill(p.well).stroke(Stroke::new(1.0, p.line)).rounding(8.0).inner_margin(egui::Margin::symmetric(12.0, 10.0)).show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.spacing_mut().item_spacing.y = 0.0;
                                let desc = match cur {
                                    Some(r) => format!("{}　{}", t(&format!("{}_d", key)), tf("wiz_cur", &[&name, &format!("{:.1}", r.val_of(m))])),
                                    None => t(&format!("{}_d", key)),
                                };
                                item(ui, p, true, &t(key), &desc, |ui| toggle(ui, p, on));
                                if *on {
                                    let s = format!("{:.0} GB", warn);
                                    item(ui, p, false, &t("wiz_l_warn"), "", |ui| {
                                        stepper(ui, p, &format!("wiz-warn-{}", j), warn, 1.0, 1.0, 255.0, s);
                                    });
                                    if kill {
                                        let s = format!("{:.0} GB", off);
                                        item(ui, p, false, &t("wiz_l_kill"), "", |ui| {
                                            stepper(ui, p, &format!("wiz-kill-{}", j), off, 1.0, 2.0, 256.0, s);
                                        });
                                        // 關掉的線至少比響的線高 1 GB；調高響的線就把關掉的線往上推。
                                        if *off < *warn + 1.0 {
                                            *off = *warn + 1.0;
                                        }
                                    }
                                }
                            });
                        }
                    }
                    _ => {
                        let snd = if w.sound == Sound::Repeat { t("wiz_snd_repeat") } else { t("wiz_snd_once") };
                        let mut any = false;
                        for (j, key) in [(0, "wiz_l_commit"), (1, "wiz_l_phys")] {
                            let (on, warn, off) = w.lines[j];
                            if !on {
                                continue;
                            }
                            any = true;
                            // 名稱後面帶它看哪一種記憶體，跟第四步那列的說明同一句。
                            let label = tf("wiz_label", &[&t(key), &t(&format!("{}_d", key))]);
                            let s = if w.kill {
                                tf("wiz_r_kill", &[&label, &format!("{:.0}", warn), &snd, &format!("{:.0}", off), &w.name])
                            } else {
                                tf("wiz_r_warn", &[&label, &format!("{:.0}", warn), &snd])
                            };
                            ui.add(egui::Label::new(egui::RichText::new(s).font(f(14.0)).color(p.txt)).wrap());
                        }
                        if !any {
                            ui.add(egui::Label::new(egui::RichText::new(t("wiz_none")).font(f(14.0)).color(p.txt)).wrap());
                        }
                        if w.sound == Sound::Repeat {
                            ui.add(egui::Label::new(egui::RichText::new(t("wiz_mute")).font(f(12.0)).color(p.dim)).wrap());
                        }
                    }
                }
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if (1..WIZ_STEPS).contains(&w.step) && text_button(ui, p, &t("wiz_back"), false, p.txt).clicked() {
                        w.step -= 1;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        match w.step {
                            1 | 2 => {
                                if text_button(ui, p, &t("wiz_next"), true, p.accent(0)).clicked() {
                                    if w.step == 1 {
                                        let m = w.basis;
                                        Self::wizard_basis(&mut w, m, v.phys_total);
                                    }
                                    w.step += 1;
                                }
                            }
                            3 => {
                                if text_button(ui, p, &t("wiz_finish"), true, p.ok).clicked() {
                                    apply = true;
                                    w.step = WIZ_STEPS;
                                }
                            }
                            s if s >= WIZ_STEPS => {
                                if text_button(ui, p, &t("wiz_ok"), true, p.ok).clicked() {
                                    close = true;
                                }
                            }
                            _ => {}
                        }
                    });
                });
            });
        });
        if apply {
            self.wizard_apply(ctx, &w);
        }
        if !close {
            self.wizard = Some(w);
        }
    }

    // 「所有程式」右鍵結束工作的確認框；按結束才交給量測執行緒去關。
    fn confirm_end_dialog(&mut self, ctx: &egui::Context, p: &Pal) {
        let Some((key, name)) = self.confirm_end.clone() else { return };
        let mut close = false;
        egui::Window::new(t("ctx_end"))
            .collapsible(false)
            .resizable(false)
            .anchor(Align2::CENTER_CENTER, vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label(egui::RichText::new(tf("confirm_end", &[&name])).font(f(14.0)).color(p.txt));
                ui.add_space(10.0);
                ui.horizontal(|ui| {
                    if text_button(ui, p, &t("end_btn"), true, p.bad).clicked() {
                        self.request_end(key.clone());
                        close = true;
                    }
                    if text_button(ui, p, &t("edit_cancel"), false, p.txt).clicked() {
                        close = true;
                    }
                });
            });
        if close {
            self.confirm_end = None;
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn settings_page(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Pal, v: &View, st: &Status, frame: Color32) {
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.spacing_mut().item_spacing = vec2(8.0, 12.0);
            let mut remove = None;
            let mut add: Option<Running> = None;
            let mut targets = self.draft.targets.clone();
            if self.tab == 0 {
            card(ui, p, |ui| {
                ui.spacing_mut().item_spacing.y = 8.0;
                for (i, tg) in targets.iter_mut().enumerate() {
                    let key = tg.exe.to_lowercase();
                    let open = self.set_expand.as_deref() == Some(key.as_str());
                    let w = ui.available_width();
                    let (rr, resp) = ui.allocate_exact_size(vec2(w, 36.0), Sense::click());
                    let painter = ui.painter().clone();
                    painter.rect_filled(rr, 8.0, if resp.hovered() || open { p.pill } else { p.well });
                    let ir = Rect::from_center_size(pos2(rr.left() + 21.0, rr.center().y), vec2(22.0, 22.0));
                    self.draw_icon(ctx, &painter, ir, &tg.exe, &tg.path, &tg.name, p.accent(i), 1.0);
                    let nr = painter.text(pos2(ir.right() + 8.0, rr.center().y), Align2::LEFT_CENTER, &tg.name, fb(14.0), p.strong);
                    let (txt, fg, bg) = if tg.kill { (t("tag_kill"), p.bad, p.bad.gamma_multiply(0.12)) } else { (t("tag_warn"), p.dim, p.card) };
                    pill(&painter, pos2(nr.right() + 8.0, rr.center().y), &txt, fg, bg);
                    let xr = Rect::from_center_size(pos2(rr.right() - 20.0, rr.center().y), vec2(28.0, 26.0));
                    let del = ui.interact(xr, Id::new(("del", &key)), Sense::click());
                    let xc = if del.hovered() { p.bad } else { p.dim };
                    let c = xr.center();
                    painter.line_segment([c - vec2(5.0, 5.0), c + vec2(5.0, 5.0)], Stroke::new(1.4, xc));
                    painter.line_segment([c - vec2(5.0, -5.0), c + vec2(5.0, -5.0)], Stroke::new(1.4, xc));
                    if del.clicked() {
                        remove = Some(i);
                    } else if resp.clicked() {
                        self.set_expand = if open { None } else { Some(key.clone()) };
                        self.end_armed = None;
                    }
                    if open {
                        let now = v.rows.iter().find(|r| r.exe.eq_ignore_ascii_case(&tg.exe)).map_or((None, None), |r| (r.commit, r.ws));
                        egui::Frame::none().inner_margin(egui::Margin::symmetric(10.0, 4.0)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            self.edit_target(ui, p, tg, now);
                        });
                    }
                }
                let link = ui.add(
                    egui::Label::new(
                        egui::RichText::new(if self.picker { t("collapse") } else { t("add_target") }).font(f(13.0)).color(p.accent(0)),
                    )
                    .sense(Sense::click()),
                );
                if link.clicked() {
                    self.picker = !self.picker;
                }
                if self.picker {
                    ui.add(egui::TextEdit::singleline(&mut self.picker_q).hint_text(t("search")).desired_width(f32::INFINITY).font(f(13.0)));
                    ui.label(egui::RichText::new(t("running_hint")).font(f(12.0)).color(p.dim));
                    let existing: Vec<String> = targets.iter().map(|t| t.exe.to_lowercase()).collect();
                    let q = self.picker_q.trim().to_lowercase();
                    for (i, r) in v
                        .running
                        .iter()
                        .filter(|r| !r.system && !existing.contains(&r.exe.to_lowercase()))
                        .filter(|r| q.is_empty() || r.exe.to_lowercase().contains(&q))
                        .take(30)
                        .enumerate()
                    {
                        let w = ui.available_width();
                        let (rr, resp) = ui.allocate_exact_size(vec2(w, 30.0), Sense::click());
                        let painter = ui.painter().clone();
                        if resp.hovered() {
                            painter.rect_filled(rr, 6.0, p.well);
                        }
                        let ir = Rect::from_center_size(pos2(rr.left() + 15.0, rr.center().y), vec2(20.0, 20.0));
                        self.draw_icon(ctx, &painter, ir, &r.exe, &r.path, &r.exe, p.accent(i), 1.0);
                        painter.text(pos2(ir.right() + 8.0, rr.center().y), Align2::LEFT_CENTER, &r.exe, f(13.0), p.txt);
                        painter.text(pos2(rr.right() - 8.0, rr.center().y), Align2::RIGHT_CENTER, format!("{:.1} GB", r.commit), f(12.0), p.dim);
                        if resp.clicked() {
                            add = Some(r.clone());
                        }
                    }
                }
            });
            }
            if let Some(i) = remove {
                targets.remove(i);
            }
            if let Some(r) = add {
                targets.push(Target::new(&r.exe, &r.path));
                self.picker = false;
                self.picker_q.clear();
            }
            self.draft.targets = targets;

            let d = &mut self.draft;
            if self.tab == 1 {
            card(ui, p, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                item(ui, p, true, &t("basis"), &t("basis_d"), |ui| {
                    let mut sel = if d.basis == Metric::Commit { 0 } else { 1 };
                    if seg(ui, p, "basis", &[t("m_commit"), t("m_phys")], &mut sel) {
                        d.basis = if sel == 0 { Metric::Commit } else { Metric::Physical };
                    }
                });
                item(ui, p, false, &t("warn"), &t("warn_d"), |ui| {
                    let s = format!("{:.0} %", d.warn_pct);
                    stepper(ui, p, "warn", &mut d.warn_pct, 1.0, 10.0, 99.0, s);
                });
                item(ui, p, false, &t("kill"), &t("kill_d"), |ui| {
                    let s = format!("{:.0} %", d.kill_pct);
                    stepper(ui, p, "kill", &mut d.kill_pct, 1.0, 10.0, 99.0, s);
                });
                item(ui, p, false, &t("fast"), &t("fast_d"), |ui| {
                    let s = format!("{:.1} GB", d.growth_gb);
                    stepper(ui, p, "fast", &mut d.growth_gb, 0.5, 0.5, 64.0, s);
                });
                ui.label(egui::RichText::new(t("mem_only")).font(f(12.0)).color(p.dim));
            });
            }

            let mut auto = self.draft_auto;
            let mut open_logs = false;
            let mut relaunch = false;
            let is_admin = self.is_admin;
            if self.tab == 2 {
            card(ui, p, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                item(ui, p, true, &t("lang"), "", |ui| {
                    let codes: Vec<&str> = std::iter::once("auto").chain(i18n::LANGS.iter().map(|l| l.0)).collect();
                    let cur = codes.iter().position(|c| *c == d.lang).unwrap_or(0);
                    let label = if cur == 0 {
                        egui::RichText::new(t("lang_auto"))
                    } else {
                        let (code, name) = i18n::LANGS[cur - 1];
                        egui::RichText::new(name).family(lang_family(code))
                    };
                    egui::ComboBox::from_id_salt("lang").selected_text(label).width(150.0).show_ui(ui, |ui| {
                        if ui.selectable_label(cur == 0, t("lang_auto")).clicked() {
                            d.lang = "auto".into();
                        }
                        for (code, name) in i18n::LANGS {
                            if ui.selectable_label(d.lang == code, egui::RichText::new(name).family(lang_family(code))).clicked() {
                                d.lang = code.into();
                            }
                        }
                    });
                });
                item(ui, p, false, &t("theme"), "", |ui| {
                    let mut sel = if d.theme == Theme::Sakura { 0 } else { 1 };
                    if seg(ui, p, "theme", &[t("light"), t("dark")], &mut sel) {
                        d.theme = if sel == 0 { Theme::Sakura } else { Theme::Dark };
                    }
                });
                item(ui, p, false, &t("interval"), &t("interval_d"), |ui| {
                    let mut x = d.interval_s as f64;
                    if stepper(ui, p, "interval", &mut x, 5.0, 5.0, 600.0, tf("unit_sec", &[&d.interval_s.to_string()])) {
                        d.interval_s = x as u64;
                    }
                });
                item(ui, p, false, &t("keep"), &t("keep_d"), |ui| {
                    let mut x = d.keep_days as f64;
                    if stepper(ui, p, "keep", &mut x, 1.0, 1.0, 365.0, tf("unit_day", &[&d.keep_days.to_string()])) {
                        d.keep_days = x as u64;
                    }
                    let l = ui.add(egui::Label::new(egui::RichText::new(t("open_folder")).font(f(12.0)).color(p.accent(0))).sense(Sense::click()));
                    if l.clicked() {
                        open_logs = true;
                    }
                });
                item(ui, p, false, &t("sound"), &t("sound_d"), |ui| {
                    if text_button(ui, p, &t("snd_test"), false, p.txt).clicked() {
                        sys::play_alert();
                    }
                    let opts = [Sound::Off, Sound::Once, Sound::Repeat];
                    let mut sel = opts.iter().position(|s| *s == d.sound).unwrap_or(1);
                    if seg(ui, p, "sound", &[t("snd_off"), t("snd_once"), t("snd_repeat")], &mut sel) {
                        d.sound = opts[sel];
                    }
                });
                item(ui, p, false, &t("gpu"), "", |ui| toggle(ui, p, &mut d.gpu));
                item(ui, p, false, &t("on_top"), "", |ui| toggle(ui, p, &mut d.on_top));
                item(ui, p, false, &t("autostart"), "", |ui| toggle(ui, p, &mut auto));
                item(ui, p, false, &t("admin"), "", |ui| {
                    if is_admin {
                        ui.label(egui::RichText::new(t("admin_on")).font(f(12.0)).color(p.ok));
                    } else if text_button(ui, p, &t("admin_btn"), false, p.txt).clicked() {
                        relaunch = true;
                    }
                });
            });
            }
            if self.tab == 3 {
                self.mini_settings(ui, ctx, p, v, st, frame);
            }
            if self.tab == 4 {
                self.about(ui, ctx, p);
            }
            self.draft_auto = auto;
            if relaunch && sys::relaunch_as_admin() {
                self.actions.push(Action::Close);
            }
            if open_logs {
                let dir = log_dir();
                let _ = fs::create_dir_all(&dir);
                let _ = Command::new("explorer").arg(dir).spawn();
            }
        });
    }

    fn mini_ui(&mut self, ctx: &egui::Context, p: &Pal, v: &View, st: &Status, frame: Color32) {
        egui::CentralPanel::default().frame(egui::Frame::none().fill(p.mini_bg)).show(ctx, |ui| {
            let full = ui.max_rect();
            let resp = ui.interact(full, Id::new("mini-drag"), Sense::click_and_drag());
            if resp.drag_started() {
                self.actions.push(Action::Drag);
            }
            if resp.double_clicked() {
                self.actions.push(Action::Mini(false));
            }
            let painter = ui.painter().clone();
            let cfg = self.cfg.clone();
            let (size, round) = self.paint_mini(ctx, &painter, full.min, p, v, frame, &cfg, 1.0, 1.0, false);
            if st.level != Level::Ok {
                painter.rect_stroke(Rect::from_min_size(full.min, size).shrink(1.0), round, Stroke::new(1.5, frame));
            }
            self.want = size;
            self.mini_round = round;        });
    }

    // 小橫條本體，實際的小橫條和設定頁預覽共用；a 是預覽模擬的透明度，zoom 是預覽放不下時的縮小比例，
    // preview 是設定頁預覽（沒資料的格子也畫個位置）。
    // 勾了「顯示在小橫條」而且正在跑的目標才畫。橫向全部排一行；直向每個目標一列、欄位對齊，總量那段放最下面。
    // 回傳畫出來的大小和圓角半徑。
    #[allow(clippy::too_many_arguments)]
    fn paint_mini(&mut self, ctx: &egui::Context, painter: &egui::Painter, origin: Pos2, p: &Pal, v: &View, frame: Color32, cfg: &Config, a: f32, zoom: f32, preview: bool) -> (egui::Vec2, f32) {
        let m = &cfg.mini_bar;
        let k = m.size.k() * zoom;
        let vert = m.vertical;
        let gap = 12.0 * k;
        let tight = 4.0 * k;
        let c = |col: Color32| col.gamma_multiply(a);
        let gpu_on = cfg.gpu;
        let shown: Vec<usize> = v
            .rows
            .iter()
            .enumerate()
            .filter(|(_, r)| r.commit.is_some() && cfg.targets.iter().any(|t| t.exe.eq_ignore_ascii_case(&r.exe) && t.mini))
            .map(|(i, _)| i)
            .collect();
        // 只有一個目標的橫向才幫十分鐘加小字標籤；RAM、認可、CPU、GPU 一律加，才分得出誰是誰。
        let single = shown.len() == 1 && !vert;
        let num = f(13.0 * k);
        let label = |s: &str| Some((s.to_string(), f(11.0 * k), p.mini_dim));
        // 每列固定十一格：名稱，以及 RAM、認可、十分鐘、CPU、GPU 各一格小字標籤加一格數字（單數格是標籤）。
        let rows: Vec<(usize, Vec<MiniCell>)> = shown
            .iter()
            .map(|&i| {
                let r = &v.rows[i];
                let mut s = vec![None; 11];
                if m.name {
                    s[0] = Some((r.name.clone(), fb(13.0 * k), p.mini_txt));
                }
                for (j, on, lbl, val, metric) in [(1, m.ram, "RAM".to_string(), r.ws, Metric::Physical), (3, m.commit, t("m_commit"), r.commit, Metric::Commit)] {
                    if on {
                        s[j] = label(&lbl);
                        s[j + 1] = Some((gb_text(val), num.clone(), limit_color(cfg, r, p, metric)));
                    }
                }
                // 十分鐘沒變化時小橫條不畫這格；設定頁預覽改畫淡色的「—」，看得出它會出現在哪。
                if m.d10 {
                    if let Some(d) = mini_delta(r.d10) {
                        s[5] = if single { label(&t("col_d10")) } else { None };
                        s[6] = Some((d, num.clone(), delta_color(r, p, p.mini_txt)));
                    } else if preview {
                        s[5] = if single { label(&t("col_d10")) } else { None };
                        s[6] = Some(("—".to_string(), num.clone(), p.mini_dim));
                    }
                }
                for (j, on, lbl, val) in [(7, m.cpu, "CPU", r.cpu), (9, m.gpu && gpu_on, "GPU", r.gpu)] {
                    if on {
                        s[j] = label(lbl);
                        s[j + 1] = Some((pct_text(val), num.clone(), p.mini_txt));
                    }
                }
                (i, s)
            })
            .collect();
        // 總量每一項：標籤、百分比、是不是判斷依據（是的話照狀態上色）、記憶體的「已用 / 總共」。
        let ratio = |used: f64, cap: f64| (v.ready && cap > 0.0).then(|| used / cap * 100.0);
        let gb = |used: f64, cap: f64| v.ready.then(|| format!("{:.1} / {:.1} GB", used, cap));
        let whole: Vec<WholeItem> = [
            (m.sys_ram, "RAM".to_string(), ratio(v.phys, v.phys_total), cfg.basis == Metric::Physical, gb(v.phys, v.phys_total)),
            (m.sys_commit, t("m_commit"), ratio(v.sys, v.limit), cfg.basis == Metric::Commit, gb(v.sys, v.limit)),
            (m.sys_cpu, "CPU".to_string(), v.cpu, false, None),
            (m.sys_gpu && gpu_on, "GPU".to_string(), v.gpu, false, None),
        ]
        .into_iter()
        .filter(|w| w.0)
        .map(|w| (w.1, w.2, w.3, w.4))
        .collect();
        if vert {
            return self.paint_mini_v(ctx, painter, origin, p, v, frame, cfg, a, k, &rows, &whole);
        }

        let after = |j: usize| if j % 2 == 1 { tight } else { gap };
        let h = MINI_H * k;
        let cy = origin.y + h / 2.0;
        let text = |x: f32, s: &str, font: FontId, col: Color32| painter.text(pos2(x, cy), Align2::LEFT_CENTER, s, font, c(col)).right();
        // 兩端留的空白要大於半個高度，膠囊形的圓角才不會切到字。
        let mut x = origin.x + 18.0 * k;
        for (i, s) in &rows {
            let r = &v.rows[*i];
            let ir = Rect::from_center_size(pos2(x + 10.0 * k, cy), vec2(20.0, 20.0) * k);
            self.draw_icon(ctx, painter, ir, &r.exe, &r.path, &r.name, p.accent(*i), a);
            x = ir.right() + 8.0 * k;
            for (j, (txt, font, col)) in s.iter().enumerate().filter_map(|(j, cell)| cell.as_ref().map(|cell| (j, cell))) {
                x = text(x, txt, font.clone(), *col) + after(j);
            }
        }
        if (cfg.paused || !whole.is_empty()) && !rows.is_empty() {
            painter.rect_filled(Rect::from_min_size(pos2(x, cy - 7.0 * k), vec2(1.0, 14.0 * k)), 0.0, c(p.mini_dim.gamma_multiply(0.5)));
            x += 11.0 * k;
        }
        if cfg.paused {
            x = pill_sized(painter, pos2(x, cy), &t("st_paused"), c(p.warn), c(p.warn.gamma_multiply(0.16)), k).right() + 10.0 * k;
        }
        for (lbl, pct, is_basis, _) in &whole {
            let col = if *is_basis { frame } else { p.sys };
            x = text(x, lbl, f(11.0 * k), p.mini_dim) + tight;
            if m.bars {
                x = mini_bar(painter, pos2(x, cy), k, pct.unwrap_or(0.0), &c, p, col) + 10.0 * k;
            }
            x = text(x, &pct_text(*pct), fb(13.0 * k), if *is_basis { frame } else { p.mini_txt }) + gap;
        }
        (vec2((x - gap + 20.0 * k - origin.x).max(h), h), h / 2.0)
    }

    // 直向：每個目標一列，名稱、RAM、認可、十分鐘各成一欄對齊；CPU、GPU 放在記憶體數字底下一行，
    // 所有列的 CPU、GPU 對齊同兩欄。總量放最下面，每一項一行：標籤、進度條、百分比。
    #[allow(clippy::too_many_arguments)]
    fn paint_mini_v(
        &mut self,
        ctx: &egui::Context,
        painter: &egui::Painter,
        origin: Pos2,
        p: &Pal,
        v: &View,
        frame: Color32,
        cfg: &Config,
        a: f32,
        k: f32,
        rows: &[(usize, Vec<MiniCell>)],
        whole: &[WholeItem],
    ) -> (egui::Vec2, f32) {
        let m = &cfg.mini_bar;
        let c = |col: Color32| col.gamma_multiply(a);
        let (gap, tight, pad, top) = (12.0 * k, 4.0 * k, 18.0 * k, 10.0 * k);
        let (main_h, sub_h, whole_h, row_gap) = (26.0 * k, 18.0 * k, 22.0 * k, 4.0 * k);
        let tw = |s: &str, font: FontId| painter.layout_no_wrap(s.to_string(), font, Color32::WHITE).size().x;
        let (lab, val) = (f(11.0 * k), f(12.0 * k));
        let pair_w = |u: &(&str, String)| tw(u.0, lab.clone()) + tight + tw(&u.1, val.clone());
        let usage_of = |s: &[MiniCell]| -> Vec<(&str, String)> {
            [(7, "CPU"), (9, "GPU")].into_iter().filter_map(|(j, l)| s[j + 1].as_ref().map(|cell| (l, cell.0.clone()))).collect()
        };

        // 名稱、RAM、認可、十分鐘各成欄，每欄取所有列裡最寬的；標籤和數字之間只留小空隙，數字靠右。
        const MAIN: usize = 7;
        let space = |j: usize| if j % 2 == 1 { tight } else { gap };
        let mut colw = [None::<f32>; MAIN];
        for (_, s) in rows {
            for (j, w) in colw.iter_mut().enumerate() {
                if let Some((txt, font, _)) = &s[j] {
                    let cw = tw(txt, font.clone());
                    *w = Some(w.map_or(cw, |o: f32| o.max(cw)));
                }
            }
        }
        let x_icon = origin.x + pad;
        let mut colx = [0.0f32; MAIN];
        let mut x = x_icon + 28.0 * k;
        let mut last = 0.0;
        for j in 0..MAIN {
            colx[j] = x;
            if let Some(w) = colw[j] {
                last = space(j);
                x += w + last;
            }
        }
        let main_right = x - last;
        // 底下 CPU、GPU 那行從第一個記憶體標籤的位置開始，上下對齊。
        let sub_x = [1, 3].into_iter().find(|&j| colw[j].is_some()).map_or(colx[0], |j| colx[j]);

        // CPU 一欄、GPU 一欄，寬度取所有列裡最寬的。
        let all_usage: Vec<(&str, String)> = rows.iter().flat_map(|(_, s)| usage_of(s)).collect();
        let cpu_w = all_usage.iter().filter(|u| u.0 == "CPU").map(pair_w).fold(0.0, f32::max);
        let gpu_w = all_usage.iter().filter(|u| u.0 == "GPU").map(pair_w).fold(0.0, f32::max);
        let gpu_x = if cpu_w > 0.0 { sub_x + cpu_w + gap } else { sub_x };

        // 總量：標籤一欄、進度條、百分比，記憶體再加「已用 / 總共」靠右對齊到最右緣。
        let lab_w = whole.iter().map(|w| tw(&w.0, lab.clone())).fold(0.0, f32::max);
        let bar_x = x_icon + lab_w + 8.0 * k;
        let pct_x = if m.bars { bar_x + 70.0 * k + 10.0 * k } else { bar_x };
        let pct_w = tw("100%", fb(13.0 * k));
        let detail_w = whole.iter().filter_map(|w| w.3.as_ref()).map(|s| tw(s, val.clone())).fold(0.0, f32::max);
        let pill_w = if cfg.paused { tw(&t("st_paused"), f(11.0 * k)) + 16.0 * k } else { 0.0 };

        let mut right = if rows.is_empty() { x_icon } else { main_right };
        if gpu_w > 0.0 {
            right = right.max(gpu_x + gpu_w);
        } else if cpu_w > 0.0 {
            right = right.max(sub_x + cpu_w);
        }
        if !whole.is_empty() {
            right = right.max(pct_x + pct_w);
        }
        if detail_w > 0.0 {
            right = right.max(pct_x + pct_w + gap + detail_w);
        }
        right = right.max(x_icon + pill_w);
        let draw_usage = |cy: f32, u: &[(&str, String)]| {
            for (l, s) in u {
                let xs = if *l == "CPU" { sub_x } else { gpu_x };
                let lx = painter.text(pos2(xs, cy), Align2::LEFT_CENTER, *l, lab.clone(), c(p.mini_dim)).right() + tight;
                painter.text(pos2(lx, cy), Align2::LEFT_CENTER, s, val.clone(), c(p.mini_txt));
            }
        };

        let mut y = origin.y + top;
        for (i, s) in rows {
            let r = &v.rows[*i];
            let cy = y + main_h / 2.0;
            let ir = Rect::from_center_size(pos2(x_icon + 10.0 * k, cy), vec2(20.0, 20.0) * k);
            self.draw_icon(ctx, painter, ir, &r.exe, &r.path, &r.name, p.accent(*i), a);
            for j in 0..MAIN {
                if let (Some((txt, font, col)), Some(w)) = (&s[j], colw[j]) {
                    let (px, align) = if j == 0 || j % 2 == 1 { (colx[j], Align2::LEFT_CENTER) } else { (colx[j] + w, Align2::RIGHT_CENTER) };
                    painter.text(pos2(px, cy), align, txt, font.clone(), c(*col));
                }
            }
            y += main_h;
            let u = usage_of(s);
            if !u.is_empty() {
                draw_usage(y + sub_h / 2.0 - 2.0 * k, &u);
                y += sub_h;
            }
            y += row_gap;
        }
        if cfg.paused || !whole.is_empty() {
            if !rows.is_empty() {
                painter.rect_filled(Rect::from_min_size(pos2(x_icon, y), vec2(right - x_icon, 1.0)), 0.0, c(p.mini_dim.gamma_multiply(0.5)));
                y += 1.0 + row_gap * 2.0;
            }
            if cfg.paused {
                pill_sized(painter, pos2(x_icon, y + whole_h / 2.0), &t("st_paused"), c(p.warn), c(p.warn.gamma_multiply(0.16)), k);
                y += whole_h;
            }
            for (lbl, pct, is_basis, detail) in whole {
                let cy = y + whole_h / 2.0;
                let col = if *is_basis { frame } else { p.sys };
                painter.text(pos2(x_icon, cy), Align2::LEFT_CENTER, lbl, lab.clone(), c(p.mini_dim));
                if m.bars {
                    mini_bar(painter, pos2(bar_x, cy), k, pct.unwrap_or(0.0), &c, p, col);
                }
                painter.text(pos2(pct_x + pct_w, cy), Align2::RIGHT_CENTER, pct_text(*pct), fb(13.0 * k), c(if *is_basis { frame } else { p.mini_txt }));
                if let Some(d) = detail {
                    painter.text(pos2(right, cy), Align2::RIGHT_CENTER, d, val.clone(), c(p.mini_dim));
                }
                y += whole_h;
            }
            y += row_gap;
        }
        let size = vec2(right + pad - origin.x, y - row_gap + top - origin.y).max(vec2(main_h, main_h));
        (size, 12.0 * k)
    }

    // 設定頁「關於」分頁：版本、作者、GitHub、手動檢查更新（在背景執行緒連網，畫面不會卡住）。
    fn about(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Pal) {
        let state = self.update.lock().unwrap().clone();
        let repo_url = format!("https://github.com/{}", REPO);
        card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 0.0;
            item(ui, p, true, &t("app_title"), "", |ui| {
                ui.label(egui::RichText::new(format!("v{}", VERSION)).font(fb(13.0)).color(p.strong));
            });
            item(ui, p, false, &t("about_author"), "", |ui| {
                ui.label(egui::RichText::new(AUTHOR).font(f(13.0)).color(p.txt));
            });
            item(ui, p, false, "GitHub", "", |ui| {
                let l = ui.add(egui::Label::new(egui::RichText::new(&repo_url).font(f(12.0)).color(p.accent(0))).sense(Sense::click()));
                if l.clicked() {
                    open_url(&repo_url);
                }
            });
            let desc = match &state {
                Update::Idle => String::new(),
                Update::Checking => t("upd_checking"),
                Update::Latest => t("upd_latest"),
                Update::Newer(tag) => tf("upd_newer", &[tag]),
                Update::Failed => t("upd_failed"),
            };
            item(ui, p, false, &t("upd"), &desc, |ui| {
                if let Update::Newer(_) = state {
                    if text_button(ui, p, &t("upd_download"), true, p.ok).clicked() {
                        open_url(&format!("{}/releases/latest", repo_url));
                    }
                }
                if state != Update::Checking && text_button(ui, p, &t("upd_check"), false, p.txt).clicked() {
                    *self.update.lock().unwrap() = Update::Checking;
                    let (st, ctx) = (self.update.clone(), ctx.clone());
                    std::thread::spawn(move || {
                        let r = check_update();
                        *st.lock().unwrap() = r;
                        ctx.request_repaint();
                    });
                }
            });
        });
    }

    // 設定頁「小橫條」分頁：上面即時預覽（照草稿畫，底下墊格紋看得出透明度），下面是選項。
    #[allow(clippy::too_many_arguments)]
    fn mini_settings(&mut self, ui: &mut Ui, ctx: &egui::Context, p: &Pal, v: &View, st: &Status, frame: Color32) {
        let before = (self.draft.mini_bar.clone(), self.draft.mini_opacity);
        card(ui, p, |ui| {
            ui.spacing_mut().item_spacing.y = 8.0;
            let cfg = self.draft.clone();
            let w = ui.available_width();
            // 先不顯示地量一次大小，放不下就等比例縮小。
            let probe = ui.painter().with_clip_rect(Rect::NOTHING);
            let (full, _) = self.paint_mini(ctx, &probe, Pos2::ZERO, p, v, frame, &cfg, 1.0, 1.0, true);
            let zoom = ((w - 24.0) / full.x).min(1.0);
            let (r, _) = ui.allocate_exact_size(vec2(w, full.y * zoom + 24.0), Sense::hover());
            let painter = ui.painter_at(r);
            painter.rect_filled(r, 8.0, hex(0x8a8a8a));
            let cell = 10.0;
            for j in 0..(r.height() / cell).ceil() as i32 {
                for i in 0..(r.width() / cell).ceil() as i32 {
                    if (i + j) % 2 == 0 {
                        let cr = Rect::from_min_size(r.min + vec2(i as f32, j as f32) * cell, vec2(cell, cell));
                        painter.rect_filled(cr, 0.0, hex(0xbdbdbd));
                    }
                }
            }
            let a = cfg.mini_opacity as f32 / 100.0;
            let origin = pos2(r.left() + 12.0, r.top() + 12.0);
            // 背景要墊在內容底下，先佔位，畫完再補。
            let bg = painter.add(Shape::Noop);
            let (size, round) = self.paint_mini(ctx, &painter, origin, p, v, frame, &cfg, a, zoom, true);
            let br = Rect::from_min_size(origin, size);
            painter.set(bg, Shape::rect_filled(br, round, p.mini_bg.gamma_multiply(a)));
            if st.level != Level::Ok {
                painter.rect_stroke(br.shrink(1.0), round, Stroke::new(1.5, frame.gamma_multiply(a)));
            }

            let gpu_on = self.draft.gpu;
            let d = &mut self.draft;
            ui.spacing_mut().item_spacing.y = 0.0;
            item(ui, p, true, &t("mini_size"), "", |ui| {
                let opts = [MiniSize::Small, MiniSize::Medium, MiniSize::Large];
                let mut sel = opts.iter().position(|s| *s == d.mini_bar.size).unwrap_or(1);
                if seg(ui, p, "mini-size", &[t("size_s"), t("size_m"), t("size_l")], &mut sel) {
                    d.mini_bar.size = opts[sel];
                }
            });
            item(ui, p, false, &t("mini_layout"), "", |ui| {
                let mut sel = d.mini_bar.vertical as usize;
                if seg(ui, p, "mini-layout", &[t("lay_h"), t("lay_v")], &mut sel) {
                    d.mini_bar.vertical = sel == 1;
                }
            });
            item(ui, p, false, &t("mini_op"), &t("mini_op_d"), |ui| {
                ui.label(egui::RichText::new(format!("{} %", d.mini_opacity)).font(fb(13.0)).color(p.strong));
                ui.spacing_mut().slider_width = 160.0;
                let mut x = d.mini_opacity as f64;
                if ui.add(egui::Slider::new(&mut x, 30.0..=100.0).step_by(5.0).show_value(false)).changed() {
                    d.mini_opacity = x as u8;
                }
            });
            // 顯示項目表：橫的是每一項，直的是每個目標與總量，打勾的才畫；沒有的格子是一條線。
            let m = &mut d.mini_bar;
            let mut cols = vec![t("name"), "RAM".to_string(), t("m_commit"), t("col_d10"), "CPU".to_string()];
            if gpu_on {
                cols.push("GPU".to_string());
            }
            let w = ui.available_width();
            let (sr, _) = ui.allocate_exact_size(vec2(w, 1.0), Sense::hover());
            ui.painter().rect_filled(sr, 0.0, p.line);
            const FIRST: f32 = 92.0;
            let cw = (w - FIRST) / cols.len() as f32;
            let (hr, _) = ui.allocate_exact_size(vec2(w, 30.0), Sense::hover());
            for (n, name) in cols.iter().enumerate() {
                let x = hr.left() + FIRST + cw * (n as f32 + 0.5);
                ui.painter().text(pos2(x, hr.center().y), Align2::CENTER_CENTER, name, f(12.0), p.dim);
            }
            for (row, title) in [t("mini_seg"), t("mini_whole")].iter().enumerate() {
                let (rr, _) = ui.allocate_exact_size(vec2(w, 34.0), Sense::hover());
                ui.painter().text(pos2(rr.left(), rr.center().y), Align2::LEFT_CENTER, title, f(14.0), p.txt);
                for n in 0..cols.len() {
                    let field = match (row, n) {
                        (0, 0) => Some(&mut m.name),
                        (0, 1) => Some(&mut m.ram),
                        (0, 2) => Some(&mut m.commit),
                        (0, 3) => Some(&mut m.d10),
                        (0, 4) => Some(&mut m.cpu),
                        (0, 5) => Some(&mut m.gpu),
                        (1, 1) => Some(&mut m.sys_ram),
                        (1, 2) => Some(&mut m.sys_commit),
                        (1, 4) => Some(&mut m.sys_cpu),
                        (1, 5) => Some(&mut m.sys_gpu),
                        _ => None,
                    };
                    let center = pos2(rr.left() + FIRST + cw * (n as f32 + 0.5), rr.center().y);
                    match field {
                        Some(on) => check_cell(ui, p, Id::new(("mini-cell", row, n)), center, on),
                        None => {
                            ui.painter().line_segment([center - vec2(5.0, 0.0), center + vec2(5.0, 0.0)], Stroke::new(1.0, p.line));
                        }
                    }
                }
            }
            ui.add_space(6.0);
            item(ui, p, false, &t("mini_bars"), "", |ui| toggle(ui, p, &mut m.bars));
        });
        // 預覽畫在選項上面，選項一改要再畫一次預覽才跟得上。
        if before != (self.draft.mini_bar.clone(), self.draft.mini_opacity) {
            ctx.request_repaint();
        }
    }
}

// 縱軸跟著資料走：最大值往上取整，門檻色塊落在範圍內才畫；右端標出每條線的名字和數值。
fn chart(pt: &egui::Painter, r: Rect, p: &Pal, v: &View, hist: &VecDeque<Sample>, cfg: &Config, hover: Option<Pos2>) {
    let view = cfg.view;
    let (l, rr, t0, b) = (r.left() + 34.0, r.right() - 82.0, r.top() + 6.0, r.bottom() - 20.0);
    let now = Instant::now();
    let recent: Vec<&Sample> = hist.iter().filter(|s| now.duration_since(s.t) <= HISTORY).collect();
    let cap = v.whole(view).1.max(1.0);
    let mut dmax = recent.iter().map(|s| s.whole(view)).fold(0.0, f64::max);
    for row in &v.rows {
        let k = row.exe.to_lowercase();
        dmax = recent.iter().filter_map(|s| s.of(&k, view)).fold(dmax, f64::max);
    }
    let step = [0.5, 1.0, 2.0, 4.0, 5.0, 8.0, 10.0, 15.0, 20.0, 25.0, 40.0].into_iter().find(|s| s * 3.0 >= dmax * 1.1).unwrap_or(cap / 3.0);
    let ymax = if step * 3.0 >= cap { cap } else { step * 3.0 };
    let y = |val: f64| b - ((val / ymax).clamp(0.0, 1.0) as f32) * (b - t0);
    let span = HISTORY.as_secs_f32();
    let x = |ti: Instant| rr - (now.duration_since(ti).as_secs_f32() / span).min(1.0) * (rr - l);
    let font = f(10.0);
    if view == cfg.basis {
        let (w, k) = (cap * cfg.warn_pct / 100.0, cap * cfg.kill_pct / 100.0);
        if k < ymax {
            pt.rect_filled(Rect::from_min_max(pos2(l, y(ymax)), pos2(rr, y(k))), 0.0, p.bad.gamma_multiply(0.10));
        }
        if w < ymax {
            pt.rect_filled(Rect::from_min_max(pos2(l, y(k.min(ymax))), pos2(rr, y(w))), 0.0, p.warn.gamma_multiply(0.10));
        }
    }
    for k in 0..=3 {
        let val = ymax * k as f64 / 3.0;
        let yy = y(val);
        pt.line_segment([pos2(l, yy), pos2(rr, yy)], Stroke::new(1.0, p.line));
        let label = if ymax < 3.0 { format!("{:.1}", val) } else { format!("{:.0}", val) };
        pt.text(pos2(l - 6.0, yy), Align2::RIGHT_CENTER, label, font.clone(), p.dim);
    }
    for (frac, label, align) in [
        (0.0, t("min_ago"), Align2::LEFT_TOP),
        (1.0 / 3.0, "20".to_string(), Align2::CENTER_TOP),
        (2.0 / 3.0, "10".to_string(), Align2::CENTER_TOP),
        (1.0, t("now"), Align2::RIGHT_TOP),
    ] {
        pt.text(pos2(l + frac * (rr - l), b + 6.0), align, label, font.clone(), p.dim);
    }
    let mut series: Vec<(Vec<Pos2>, Color32, String, f64)> = Vec::new();
    let total: Vec<Pos2> = recent.iter().map(|s| pos2(x(s.t), y(s.whole(view)))).collect();
    if let Some(last) = recent.last() {
        series.push((total, p.sys, t("total"), last.whole(view)));
    }
    for (i, row) in v.rows.iter().enumerate() {
        let k = row.exe.to_lowercase();
        let mut seg_pts = Vec::new();
        let mut last = None;
        for s in &recent {
            match s.of(&k, view) {
                Some(c) => {
                    seg_pts.push(pos2(x(s.t), y(c)));
                    last = Some(c);
                }
                None => {
                    if !seg_pts.is_empty() {
                        series.push((std::mem::take(&mut seg_pts), p.accent(i), String::new(), 0.0));
                    }
                    last = None;
                }
            }
        }
        if !seg_pts.is_empty() {
            series.push((seg_pts, p.accent(i), row.name.clone(), last.unwrap_or(0.0)));
        }
    }
    let mut labels: Vec<(f32, Color32, String, String)> = Vec::new();
    for (pts, color, name, val) in &series {
        if pts.len() >= 2 {
            let mut mesh = egui::Mesh::default();
            let top = color.gamma_multiply(0.22);
            for w in pts.windows(2) {
                let base = mesh.vertices.len() as u32;
                mesh.colored_vertex(w[0], top);
                mesh.colored_vertex(w[1], top);
                mesh.colored_vertex(pos2(w[1].x, b), Color32::TRANSPARENT);
                mesh.colored_vertex(pos2(w[0].x, b), Color32::TRANSPARENT);
                mesh.add_triangle(base, base + 1, base + 2);
                mesh.add_triangle(base, base + 2, base + 3);
            }
            pt.add(Shape::mesh(mesh));
            pt.add(Shape::line(pts.clone(), Stroke::new(2.0, *color)));
        }
        if let Some(lp) = pts.last() {
            pt.circle_filled(*lp, 3.0, *color);
            // 名字非空代表這條線一直更新到最新一次量測，就標在右端。
            if !name.is_empty() {
                labels.push((lp.y, *color, name.clone(), format!("{:.1}", val)));
            }
        }
    }
    // 右端標籤由上往下排，彼此至少隔 12 點，靠得很近的線也分得出來。
    labels.sort_by(|a, b| a.0.total_cmp(&b.0));
    let mut prev = f32::MIN;
    let n = labels.len();
    let mut ys: Vec<f32> = labels.iter().map(|l| l.0).collect();
    for yv in ys.iter_mut() {
        *yv = yv.max(prev + 12.0);
        prev = *yv;
    }
    let overflow = ys.last().map_or(0.0, |l| (l - (b + 4.0)).max(0.0));
    // 名稱太長就截斷，數字一定留著，整個標籤不超出圖的右緣。
    for (i, (_, color, name, val)) in labels.into_iter().enumerate() {
        let yy = ys[i] - overflow * (i as f32 + 1.0) / n as f32;
        let vw = pt.layout_no_wrap(val.clone(), font.clone(), color).size().x;
        let (nr, _) = text_elided(pt, pos2(rr + 6.0, yy), &name, font.clone(), color, r.right() - rr - 6.0 - vw - 4.0);
        pt.text(pos2(nr.right() + 4.0, yy), Align2::LEFT_CENTER, val, font.clone(), color);
    }

    // 滑鼠在圖上時：找最接近的那次量測，畫一條直線並列出當時每條線的數值。
    let Some(h) = hover.filter(|h| h.x >= l && h.x <= rr && h.y >= t0 && h.y <= b) else { return };
    let Some(s) = recent.iter().min_by(|a, c| (x(a.t) - h.x).abs().total_cmp(&(x(c.t) - h.x).abs())) else { return };
    let sx = x(s.t);
    pt.line_segment([pos2(sx, t0), pos2(sx, b)], Stroke::new(1.0, p.dim));
    let mut rows: Vec<(Color32, String, f64)> = vec![(p.sys, t("total"), s.whole(view))];
    for (i, row) in v.rows.iter().enumerate() {
        if let Some(val) = s.of(&row.exe.to_lowercase(), view) {
            rows.push((p.accent(i), row.name.clone(), val));
        }
    }
    for (c, _, val) in &rows {
        pt.circle_filled(pos2(sx, y(*val)), 3.5, *c);
    }
    let mins = (now.duration_since(s.t).as_secs() / 60) as usize;
    let head = if mins == 0 { t("now") } else { tf("ago", &[&mins.to_string()]) };
    let lines: Vec<String> = rows.iter().map(|(_, n, val)| format!("{}  {:.1} GB", n, val)).collect();
    let gw = lines.iter().chain(std::iter::once(&head)).map(|s| pt.layout_no_wrap(s.clone(), f(11.0), p.txt).size().x).fold(0.0, f32::max);
    let bw = gw + 26.0;
    let bh = 18.0 + rows.len() as f32 * 15.0 + 6.0;
    let bx = if sx + 10.0 + bw > r.right() { sx - 10.0 - bw } else { sx + 10.0 };
    let by = (h.y - bh / 2.0).clamp(r.top(), r.bottom() - bh);
    let br = Rect::from_min_size(pos2(bx, by), vec2(bw, bh));
    pt.rect_filled(br.translate(vec2(0.0, 2.0)), 6.0, p.shadow);
    pt.rect(br, 6.0, p.card, Stroke::new(1.0, p.line));
    pt.text(pos2(br.left() + 8.0, br.top() + 6.0), Align2::LEFT_TOP, head, f(11.0), p.dim);
    for (i, ((c, _, _), line)) in rows.iter().zip(lines).enumerate() {
        let yy = br.top() + 24.0 + i as f32 * 15.0;
        pt.rect_filled(Rect::from_center_size(pos2(br.left() + 12.0, yy), vec2(8.0, 3.0)), 1.0, *c);
        pt.text(pos2(br.left() + 20.0, yy), Align2::LEFT_CENTER, line, f(11.0), p.strong);
    }
}

// ---------- 視窗迴圈 ----------



struct WinApp {
    ctx: egui::Context,
    gui: Gui,
    raster: raster::Raster,
    window: Option<Rc<Window>>,
    surface: Option<softbuffer::Surface<Rc<Window>, Rc<Window>>>,
    state: Option<egui_winit::State>,
    hwnd: isize,
    next_repaint: Option<Instant>,
    full_pos: Option<PhysicalPosition<i32>>,
    region: (u32, u32, i32),
    hidden: bool,
    tray_level: Level,
    tray_sig: String,
    top_applied: Option<bool>,
    // 左右 Ctrl 各自按住沒；小橫條開著時才收全系統按鍵（視窗被點穿時收不到一般按鍵）。
    ctrl: [bool; 2],
    keys_global: Option<bool>,
}

impl WinApp {
    // 排行分頁看得到時量測加快，切走、縮成小橫條或收進系統匣就恢復。
    fn sync_fast(&self) {
        let fast = !self.hidden && !self.gui.mini && !self.gui.settings && self.gui.main_tab == 1;
        let hub = &self.gui.hub;
        let mut sh = hub.s.lock().unwrap();
        if sh.fast != fast {
            sh.fast = fast;
            if fast {
                sh.wake = true;
                hub.cv.notify_all();
            }
        }
    }

    // 小橫條平常讓點擊穿過去，按住 Ctrl 時才接得到拖動與雙擊。
    fn apply_click_through(&self) {
        sys::set_click_through(self.hwnd, self.gui.mini && !self.ctrl.contains(&true));
        self.ensure_alpha();
    }

    fn apply_level(&mut self) {
        let top = self.gui.mini || self.gui.cfg.on_top;
        if self.top_applied == Some(top) {
            return;
        }
        if let Some(w) = &self.window {
            w.set_window_level(if top { WindowLevel::AlwaysOnTop } else { WindowLevel::Normal });
            self.top_applied = Some(top);
        }
    }

    // 視窗套件改樣式時會把半透明洗掉，所以每次畫完都檢查一次。
    fn ensure_alpha(&self) {
        let want = if self.gui.mini { (self.gui.cfg.mini_opacity as u32 * 255 / 100) as u8 } else { 255 };
        sys::ensure_window_alpha(self.hwnd, want);
    }

    fn set_mini(&mut self, on: bool) {
        let Some(w) = self.window.clone() else { return };
        if on == self.gui.mini {
            return;
        }
        if on {
            self.full_pos = w.outer_position().ok();
            let pos = self.gui.cfg.mini_pos.map(|[x, y]| PhysicalPosition::new(x, y)).unwrap_or_else(|| {
                let mw = w.current_monitor().map(|m| m.size().width as i32).unwrap_or(1920);
                PhysicalPosition::new(mw - 520, 24)
            });
            w.set_outer_position(pos);
        } else {
            if let Ok(pos) = w.outer_position() {
                self.gui.cfg.mini_pos = Some([pos.x, pos.y]);
                self.gui.draft.mini_pos = self.gui.cfg.mini_pos;
                save_config(&self.gui.cfg);
            }
            if let Some(pos) = self.full_pos {
                w.set_outer_position(pos);
            }
        }
        self.gui.mini = on;
        self.apply_level();
        self.apply_click_through();
        w.request_redraw();
    }

    fn show(&mut self) {
        if let Some(w) = &self.window {
            w.set_visible(true);
            w.focus_window();
            w.request_redraw();
        }
        self.hidden = false;
    }

    // 系統匣跟著狀態換色、更新提示；收起來時狀態變壞就跳 Windows 通知。
    fn sync_tray(&mut self) {
        let st = self.gui.hub.s.lock().unwrap().status.clone();
        let (used, cap) = self.gui.hub.s.lock().unwrap().view.whole(self.gui.cfg.basis);
        let p = palette(self.gui.cfg.theme);
        let c = if self.gui.cfg.paused && st.level == Level::Ok { p.warn } else { p.level(st.level) };
        let tip = format!("{} — {} {:.0}%", t("app_title"), self.gui.cfg.basis.name(), if cap > 0.0 { used / cap * 100.0 } else { 0.0 });
        let pause_label = if self.gui.cfg.paused { t("tip_resume") } else { t("tip_pause") };
        let notify = if self.hidden && st.level > self.tray_level { Some((st.title.clone(), st.msg.clone())) } else { None };
        self.tray_level = st.level;
        let sig = format!("{:?}{}{}{}", [c.r(), c.g(), c.b()], tip, pause_label, notify.is_some());
        if sig == self.tray_sig && notify.is_none() {
            return;
        }
        self.tray_sig = sig;
        tray::update(tray::TrayState { color: [c.r(), c.g(), c.b()], tip, menu: [t("tray_show"), pause_label, t("tray_quit")], notify });
    }

    fn redraw(&mut self, el: &ActiveEventLoop) {
        let (Some(w), Some(state)) = (self.window.clone(), self.state.as_mut()) else { return };
        let raw = state.take_egui_input(&w);
        let out = self.ctx.run(raw, |ctx| self.gui.ui(ctx));
        state.handle_platform_output(&w, out.platform_output);
        let ppp = out.pixels_per_point;
        self.raster.update_textures(&out.textures_delta);
        let prims = self.ctx.tessellate(out.shapes, ppp);

        let size = w.inner_size();
        if let (Some(wd), Some(ht), Some(surface)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height), self.surface.as_mut()) {
            if surface.resize(wd, ht).is_ok() {
                if let Ok(mut buf) = surface.buffer_mut() {
                    let p = palette(self.gui.cfg.theme);
                    let bg = if self.gui.mini { p.mini_bg } else { p.win };
                    self.raster.draw(&mut buf, size.width as usize, size.height as usize, &prims, ppp, [bg.r(), bg.g(), bg.b()]);
                    let _ = buf.present();
                }
            }
        }
        self.raster.free_textures(&out.textures_delta);

        // 視窗大小跟著內容走。
        let want = self.gui.want;
        let cur = size.to_logical::<f32>(w.scale_factor());
        if want.x > 10.0 && ((cur.width - want.x).abs() > 1.0 || (cur.height - want.y).abs() > 1.0) {
            let _ = w.request_inner_size(LogicalSize::new(want.x, want.y));
        }        // 外形裁成圓角：參數是圓角的直徑（像素）。
        let r = if self.gui.mini { self.gui.mini_round * 2.0 } else { 24.0 };
        let r = ((r as f64 * w.scale_factor()) as i32).min(size.height as i32);
        let key = (size.width, size.height, r);
        if key != self.region {
            self.region = key;
            sys::set_round_region(self.hwnd, size.width as i32, size.height as i32, r);
        }

        for a in std::mem::take(&mut self.gui.actions) {
            match a {
                Action::Drag => {
                    let _ = w.drag_window();
                }
                Action::Hide => {
                    w.set_visible(false);
                    self.hidden = true;
                    self.sync_fast();
                }
                Action::Close => el.exit(),
                Action::Mini(on) => self.set_mini(on),
            }
        }
        self.apply_level();
        self.apply_click_through();
        if let Some(vo) = out.viewport_output.get(&egui::ViewportId::ROOT) {
            for c in &vo.commands {
                // 只閃工作列；連視窗一起閃會在小橫條上畫出舊式標題列。
                if let egui::ViewportCommand::RequestUserAttention(_) = c {
                    w.request_user_attention(Some(winit::window::UserAttentionType::Informational));
                }
            }
            let d = vo.repaint_delay;
            if d < Duration::from_secs(3600) {
                let at = Instant::now() + d;
                self.next_repaint = Some(self.next_repaint.map_or(at, |n| n.min(at)));
            }
        }
        self.sync_tray();
        self.sync_fast();
    }
}

impl ApplicationHandler<UserEvent> for WinApp {
    fn resumed(&mut self, el: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let icon = Icon::from_rgba(ICON_RGBA.to_vec(), 64, 64).ok();
        let attrs = Window::default_attributes()
            .with_title(t("app_title"))
            .with_decorations(false)
            .with_resizable(false)
            .with_visible(false)
            .with_window_icon(icon)
            .with_inner_size(LogicalSize::new(WIN_W, 600.0));
        let w = Rc::new(el.create_window(attrs).expect("create window"));
        if let Ok(h) = w.window_handle() {
            if let RawWindowHandle::Win32(h) = h.as_raw() {
                self.hwnd = h.hwnd.get();
                sys::hide_system_frame(self.hwnd);
            }
        }
        let context = softbuffer::Context::new(w.clone()).expect("softbuffer context");
        self.surface = Some(softbuffer::Surface::new(&context, w.clone()).expect("softbuffer surface"));
        self.state = Some(egui_winit::State::new(self.ctx.clone(), egui::ViewportId::ROOT, &w, Some(w.scale_factor() as f32), None, None));
        self.window = Some(w.clone());
        self.apply_level();
        if std::env::args().any(|a| a == "--mini") {
            self.set_mini(true);
        }
        // 隱藏中的視窗收不到重畫事件，先自己畫兩次（第二次已是量好的大小）再顯示。
        self.redraw(el);
        self.redraw(el);
        w.set_visible(true);
        self.ensure_alpha();
    }

    fn window_event(&mut self, el: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        let Some(w) = self.window.clone() else { return };
        match &event {
            WindowEvent::CloseRequested => {
                el.exit();
                return;
            }
            WindowEvent::RedrawRequested => {
                self.redraw(el);
                return;
            }
            // 視窗跟著內容改大小後要再畫：立刻畫一次，半透明視窗的畫布可能還沒跟著變大，
            // 稍後再補畫一次，內容和外形才不會被切在舊寬度。
            WindowEvent::Resized(_) => {
                w.request_redraw();
                self.next_repaint = Some(Instant::now() + Duration::from_millis(150));
            }
            _ => {}
        }
        if let Some(state) = self.state.as_mut() {
            let r = state.on_window_event(&w, &event);
            if r.repaint {
                w.request_redraw();
            }
        }
    }

    fn user_event(&mut self, el: &ActiveEventLoop, e: UserEvent) {
        match e {
            UserEvent::Repaint => {
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                // 收進系統匣時畫面不會重畫，系統匣在這裡另外更新。
                self.sync_tray();
            }
            // 點系統匣叫回完整視窗（小橫條也還原，點不到小橫條時靠這裡回來），同時先把警告音靜音。
            UserEvent::Show => {
                self.gui.mute();
                self.set_mini(false);
                self.show();
            }
            UserEvent::TogglePause => {
                let mut c = self.gui.cfg.clone();
                c.paused = !c.paused;
                self.gui.draft.paused = c.paused;
                self.gui.apply_cfg(&self.ctx, c);
                self.sync_tray();
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            UserEvent::Quit => el.exit(),
        }
    }

    // 小橫條開著時看 Ctrl：按下就不再點穿，並把游標位置先告訴介面，不用先移動滑鼠就能拖或雙擊。
    fn device_event(&mut self, _el: &ActiveEventLoop, _id: winit::event::DeviceId, event: winit::event::DeviceEvent) {
        use winit::keyboard::{KeyCode, PhysicalKey};
        let winit::event::DeviceEvent::Key(k) = event else { return };
        let slot = match k.physical_key {
            PhysicalKey::Code(KeyCode::ControlLeft) => 0,
            PhysicalKey::Code(KeyCode::ControlRight) => 1,
            _ => return,
        };
        let down = k.state == winit::event::ElementState::Pressed;
        let was = self.ctrl.contains(&true);
        self.ctrl[slot] = down;
        let now = self.ctrl.contains(&true);
        if was == now || !self.gui.mini {
            return;
        }
        self.apply_click_through();
        if let (true, Some(w), Some(state)) = (now, self.window.as_ref(), self.state.as_mut()) {
            if let Ok(ip) = w.inner_position() {
                let (cx, cy) = sys::cursor_pos();
                let sf = w.scale_factor() as f32;
                let pt = pos2((cx - ip.x) as f32 / sf, (cy - ip.y) as f32 / sf);
                state.egui_input_mut().events.push(egui::Event::PointerMoved(pt));
                w.request_redraw();
            }
        }
    }

    fn about_to_wait(&mut self, el: &ActiveEventLoop) {
        let global = self.gui.mini;
        if self.keys_global != Some(global) {
            self.keys_global = Some(global);
            el.listen_device_events(if global { winit::event_loop::DeviceEvents::Always } else { winit::event_loop::DeviceEvents::Never });
            if !global {
                self.ctrl = [false; 2];
            }
        }
        match self.next_repaint {
            Some(t0) if Instant::now() >= t0 => {
                self.next_repaint = None;
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
                el.set_control_flow(ControlFlow::Wait);
            }
            Some(t0) => el.set_control_flow(ControlFlow::WaitUntil(t0)),
            None => el.set_control_flow(ControlFlow::Wait),
        }
    }
}

fn main() {
    let el = EventLoop::<UserEvent>::with_user_event().build().expect("event loop");
    let proxy = el.create_proxy();
    let cfg = load_config();
    let lang = i18n::resolve(&cfg.lang);
    i18n::set(lang);
    let ctx = egui::Context::default();
    setup_fonts(&ctx, lang);
    ctx.style_mut(|s| {
        s.spacing.item_spacing = vec2(8.0, 6.0);
        s.spacing.button_padding = vec2(10.0, 4.0);
    });
    {
        let proxy = proxy.clone();
        ctx.set_request_repaint_callback(move |info| {
            if info.delay.is_zero() {
                let _ = proxy.send_event(UserEvent::Repaint);
            }
        });
    }
    tray::start(proxy);

    let hub = Arc::new(Hub {
        s: Mutex::new(Shared {
            cfg: cfg.clone(),
            hist: VecDeque::new(),
            view: View::default(),
            status: Status::new(Level::Ok, t("st_ok"), ""),
            wake: false,
            muted: false,
            end_now: None,
            fast: false,
        }),
        cv: Condvar::new(),
    });
    {
        let (h, c) = (hub.clone(), ctx.clone());
        std::thread::spawn(move || monitor(h, c));
    }
    let autostart = autostart_get();
    let mut gui = Gui {
        hub,
        wizard: None,
        cfg: cfg.clone(),
        draft: cfg,
        draft_auto: autostart,
        settings: false,
        picker: false,
        autostart,
        confirm_reset: false,
        confirm_leave: false,
        mini: false,
        lang,
        tab: 0,
        picker_q: String::new(),
        edit_row: None,
        set_expand: None,
        end_armed: None,
        main_tab: 0,
        rank_by: RankBy::Physical,
        rank_desc: true,
        rank_mb: false,
        confirm_end: None,
        update: Arc::new(Mutex::new(Update::Idle)),
        is_admin: sys::is_admin(),
        icons: HashMap::new(),
        actions: Vec::new(),
        want: vec2(WIN_W, 600.0),
        mini_round: MINI_H / 2.0,
        applied_theme: None,
    };
    if std::env::args().any(|a| a == "--settings") {
        gui.open_settings();
    }
    if let Some(n) = std::env::args().find_map(|a| a.strip_prefix("--tab=").and_then(|n| n.parse::<usize>().ok())) {
        gui.tab = n.min(4);
    }
    if std::env::args().any(|a| a == "--rank") {
        gui.main_tab = 1;
    }
    // 一打開就進一鍵開始的第 N 步（從 0 算），給截圖用。
    if let Some(n) = std::env::args().find_map(|a| a.strip_prefix("--wizard=").and_then(|n| n.parse::<usize>().ok())) {
        gui.start_wizard();
        if let Some(w) = gui.wizard.as_mut() {
            w.step = n.min(WIZ_STEPS);
            w.kill = std::env::args().any(|a| a == "--wizard-kill");
        }
    }
    // 設定頁「目標」分頁一打開就展開這個目標，給截圖用。
    if let Some(x) = std::env::args().find_map(|a| a.strip_prefix("--expand=").map(|s| s.to_lowercase())) {
        gui.set_expand = Some(x);
    }
    let mut app = WinApp {
        ctx,
        gui,
        raster: raster::Raster::default(),
        window: None,
        surface: None,
        state: None,
        hwnd: 0,
        next_repaint: None,
        full_pos: None,
        region: (0, 0, 0),
        hidden: false,
        tray_level: Level::Ok,
        tray_sig: String::new(),
        top_applied: None,
        ctrl: [false; 2],
        keys_global: None,
    };
    let _ = el.run_app(&mut app);
    tray::stop();
    save_config(&app.gui.cfg);
}
