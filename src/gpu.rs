// GPU 用量：讀效能計數器「GPU Engine」，跟工作管理員同一套。只在打開開關時建立，關掉就整個釋放。

use std::collections::HashMap;
use windows_sys::Win32::System::Performance::{
    PdhAddEnglishCounterW, PdhCloseQuery, PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_MORE_DATA,
};

pub struct Gpu {
    query: isize,
    counter: isize,
}

pub struct Sample {
    pub whole: f64,
    // 程式 pid → 每個引擎的使用率（%）。
    per_pid: HashMap<u32, HashMap<String, f64>>,
}

impl Gpu {
    pub fn new() -> Option<Gpu> {
        let path: Vec<u16> = r"\GPU Engine(*)\Utilization Percentage".encode_utf16().chain(std::iter::once(0)).collect();
        unsafe {
            let mut q = 0isize;
            if PdhOpenQueryW(std::ptr::null(), 0, &mut q) != 0 {
                return None;
            }
            let mut c = 0isize;
            if PdhAddEnglishCounterW(q, path.as_ptr(), 0, &mut c) != 0 {
                PdhCloseQuery(q);
                return None;
            }
            // 使用率是兩次取樣之間的比例，先取一次當起點。
            PdhCollectQueryData(q);
            Some(Gpu { query: q, counter: c })
        }
    }

    pub fn sample(&mut self) -> Option<Sample> {
        unsafe {
            if PdhCollectQueryData(self.query) != 0 {
                return None;
            }
            let (mut size, mut count) = (0u32, 0u32);
            let r = PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, std::ptr::null_mut());
            if r != PDH_MORE_DATA || size == 0 {
                return None;
            }
            // 項目裡有指標和小數，緩衝要 8 位元組對齊。
            let mut buf = vec![0u64; (size as usize).div_ceil(8)];
            let items = buf.as_mut_ptr() as *mut PDH_FMT_COUNTERVALUE_ITEM_W;
            if PdhGetFormattedCounterArrayW(self.counter, PDH_FMT_DOUBLE, &mut size, &mut count, items) != 0 {
                return None;
            }
            let mut per_engine: HashMap<String, f64> = HashMap::new();
            let mut per_pid: HashMap<u32, HashMap<String, f64>> = HashMap::new();
            for i in 0..count as usize {
                let it = &*items.add(i);
                if it.FmtValue.CStatus > 1 || it.szName.is_null() {
                    continue;
                }
                let v = it.FmtValue.Anonymous.doubleValue;
                let mut n = 0;
                while *it.szName.add(n) != 0 {
                    n += 1;
                }
                let name = String::from_utf16_lossy(std::slice::from_raw_parts(it.szName, n));
                // 名稱像 pid_1234_luid_0x0_0x1_phys_0_eng_3_engtype_3D；pid 後面那段就是哪一個引擎。
                let Some((pid, engine)) = name.strip_prefix("pid_").and_then(|r| r.split_once('_')) else { continue };
                let Ok(pid) = pid.parse::<u32>() else { continue };
                *per_engine.entry(engine.to_string()).or_default() += v;
                *per_pid.entry(pid).or_default().entry(engine.to_string()).or_default() += v;
            }
            Some(Sample { whole: busiest(per_engine.values()), per_pid })
        }
    }
}

impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            PdhCloseQuery(self.query);
        }
    }
}

impl Sample {
    // 一組程式（同名的多個 pid）的用量：每個引擎先加總，再取最忙的那個，跟工作管理員一樣。
    pub fn group(&self, pids: &[u32]) -> f64 {
        let mut eng: HashMap<&str, f64> = HashMap::new();
        for pid in pids {
            for (k, v) in self.per_pid.get(pid).into_iter().flatten() {
                *eng.entry(k).or_default() += v;
            }
        }
        busiest(eng.values())
    }
}

fn busiest<'a>(v: impl Iterator<Item = &'a f64>) -> f64 {
    v.fold(0.0f64, |a, &b| a.max(b)).clamp(0.0, 100.0)
}
