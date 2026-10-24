// 介面文字：每種語言一個 lang/*.json，編譯時包進執行檔；缺字時退回英文。

use std::collections::HashMap;
use std::sync::RwLock;

pub const LANGS: [(&str, &str); 5] =
    [("zh-TW", "繁體中文"), ("zh-CN", "简体中文"), ("ja", "日本語"), ("ko", "한국어"), ("en", "English")];

const FILES: [(&str, &str); 5] = [
    ("zh-TW", include_str!("../lang/zh-TW.json")),
    ("zh-CN", include_str!("../lang/zh-CN.json")),
    ("ja", include_str!("../lang/ja.json")),
    ("ko", include_str!("../lang/ko.json")),
    ("en", include_str!("../lang/en.json")),
];

struct Tables {
    cur: HashMap<String, String>,
    en: HashMap<String, String>,
}

static TABLE: RwLock<Option<Tables>> = RwLock::new(None);

fn parse(code: &str) -> HashMap<String, String> {
    let raw = FILES.iter().find(|f| f.0 == code).map(|f| f.1).unwrap_or(FILES[4].1);
    serde_json::from_str(raw).unwrap_or_default()
}

// 設定值 "auto" 跟著 Windows 介面語言。
pub fn resolve(setting: &str) -> &'static str {
    if let Some(l) = LANGS.iter().find(|l| l.0 == setting) {
        return l.0;
    }
    let id = unsafe { windows_sys::Win32::Globalization::GetUserDefaultUILanguage() };
    match id & 0x3ff {
        0x04 if id == 0x0804 || id == 0x1004 => "zh-CN",
        0x04 => "zh-TW",
        0x11 => "ja",
        0x12 => "ko",
        _ => "en",
    }
}

pub fn set(code: &str) {
    *TABLE.write().unwrap() = Some(Tables { cur: parse(code), en: parse("en") });
}

pub fn t(key: &str) -> String {
    let g = TABLE.read().unwrap();
    g.as_ref()
        .and_then(|t| t.cur.get(key).or_else(|| t.en.get(key)))
        .cloned()
        .unwrap_or_else(|| key.to_string())
}

pub fn tf(key: &str, args: &[&str]) -> String {
    let mut s = t(key);
    for (i, a) in args.iter().enumerate() {
        s = s.replace(&format!("{{{}}}", i), a);
    }
    s
}
