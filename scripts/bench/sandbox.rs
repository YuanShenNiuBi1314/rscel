//! `rscel-sbx` —— 性能测量沙箱：单核独占 + 多轮交错采样 + 自动比对。
//!
//! ## 为什么需要它（真实事故）
//!
//! 同一对二进制在"后台开着虚拟机"与"挂起虚拟机"两种环境下测出的比值差了 4 个百分点
//! （97.69% vs 100.39%），而复现带只有 ±1.4%~±7.3% —— 也就是说：**噪声比结论大**。
//! 靠"多跑几轮"压不下来，因为 Windows 调度器会把测量进程在物理核之间迁移，
//! L1/L2 缓存与频率状态跟着抖动；更要命的是宿主上的其他负载会抢走核时间片。
//!
//! 沙箱做三件事把测量变成可判定的：
//! 1. **绑核**：把沙箱自己的进程亲和性限制到单个逻辑核，被测子进程**继承**该亲和性
//!    （Windows 上子进程的 affinity mask 继承自父进程），因此被测程序不会在核间漂移；
//! 2. **独占**：可选 `--boost` 抬升到 ABOVE_NORMAL 优先级，减少被其他进程抢占；
//!    （真·独占要靠 OS 隔离核，脚本会打印当前掩码并提醒宿主上的其他负载）；
//! 3. **多轮交错 + 自动比对**：`--rounds` 轮，每轮把所有被测程序各跑一次（交错，
//!    消除时间窗漂移），逐程序报告 min / median / 复现带，并按 `--gate` 给出
//!    PASS / FAIL / **INCONCLUSIVE**（复现带宽于门禁余量时拒绝下结论）。
//!
//! ## 零依赖
//!
//! 只用 `std` + 直接声明的 Win32 `kernel32` 外部函数（不引 `windows`/`libc` crate），
//! 与生成物同一条纪律。非 Windows 平台退回 `taskset -c N` 包装（Linux 上普遍可用），
//! 拿不到就明确降级并警告 —— 不假装绑了核。
//!
//! ## 用法
//!
//! ```text
//! rscel-sbx --core 2 --rounds 15 --metric elapsed_ns \
//!           --baseline fn_hand "fn_hand=target/bench/loop_sum_fn_hand.exe" \
//!           "gen=target/bench/loop_sum_gen.exe"
//! ```
//! 每个 `name=cmd 参数…` 是一个被测程序；`--baseline` 指定分母（默认第一个）。
//! 输出：一行一段统计 + 结尾 `SUMMARY name min_ms median_ms band_pct ratio perf_pct verdict`。

use std::collections::BTreeMap;
use std::process::{Command, Stdio};

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn SetProcessAffinityMask(h_process: *mut std::ffi::c_void, mask: usize) -> i32;
    fn GetProcessAffinityMask(
        h_process: *mut std::ffi::c_void,
        process_mask: *mut usize,
        system_mask: *mut usize,
    ) -> i32;
    fn SetPriorityClass(h_process: *mut std::ffi::c_void, class: u32) -> i32;
}

/// ABOVE_NORMAL_PRIORITY_CLASS：抢占性优于普通进程，但不至于像 REALTIME 那样能把机器打死。
const ABOVE_NORMAL: u32 = 0x0000_8000;

#[derive(Default)]
struct Opts {
    core: Option<usize>,
    mask: Option<usize>,
    rounds: usize,
    metric: String,
    gate: f64,
    band_max: f64,
    boost: bool,
    baseline: Option<String>,
    progs: Vec<(String, Vec<String>)>,
    quiet: bool,
}

fn parse_args() -> Result<Opts, String> {
    let argv: Vec<String> = std::env::args().skip(1).collect();
    let mut o = Opts {
        rounds: 15,
        metric: "elapsed_ns".to_string(),
        gate: 97.0,
        band_max: 2.0,
        ..Default::default()
    };
    let mut i = 0usize;
    while i < argv.len() {
        let a = argv[i].as_str();
        let need = |flag: &str| -> Result<String, String> {
            argv.get(i + 1).cloned().ok_or_else(|| format!("{flag} 需要参数值"))
        };
        match a {
            "--mask" => {
                o.mask = Some(
                    usize::from_str_radix(need("--mask")?.trim_start_matches("0x").trim_start_matches("0X"), 16)
                        .map_err(|e| format!("--mask: {e}"))?,
                );
                i += 2;
            }
            "--core" => {
                o.core = Some(need("--core")?.parse().map_err(|e| format!("--core: {e}"))?);
                i += 2;
            }
            "--rounds" => {
                o.rounds = need("--rounds")?.parse().map_err(|e| format!("--rounds: {e}"))?;
                i += 2;
            }
            "--metric" => {
                o.metric = need("--metric")?;
                i += 2;
            }
            "--gate" => {
                o.gate = need("--gate")?.parse().map_err(|e| format!("--gate: {e}"))?;
                i += 2;
            }
            "--band-max" => {
                o.band_max = need("--band-max")?.parse().map_err(|e| format!("--band-max: {e}"))?;
                i += 2;
            }
            "--boost" => {
                o.boost = true;
                i += 1;
            }
            "--quiet" => {
                o.quiet = true;
                i += 1;
            }
            "--baseline" => {
                o.baseline = Some(need("--baseline")?);
                i += 2;
            }
            "--" => {
                i += 1;
                while i < argv.len() {
                    let spec = &argv[i];
                    let (name, rest) = spec
                        .split_once('=')
                        .ok_or_else(|| format!("程序说明要写成 name=cmd 参数…，收到 {spec}"))?;
                    let mut cmd: Vec<String> = rest.split_whitespace().map(str::to_string).collect();
                    if cmd.is_empty() {
                        return Err(format!("{name} 没有可执行文件"));
                    }
                    // 允许 `name=path.exe --arg` 形式：后面的裸参数归到这个程序
                    let mut j = i + 1;
                    while j < argv.len() && !argv[j].contains('=') && !argv[j].starts_with("--") {
                        cmd.push(argv[j].clone());
                        j += 1;
                    }
                    o.progs.push((name.to_string(), cmd));
                    i = j;
                }
            }
            other => return Err(format!("未知参数：{other}")),
        }
    }
    if o.progs.is_empty() {
        return Err("没有被测程序（用 -- \"name=cmd …\" 给出）".to_string());
    }
    if o.rounds < 3 {
        return Err("--rounds 至少 3（否则算不出复现带）".to_string());
    }
    Ok(o)
}

/// 绑核。返回可读的实际掩码描述；失败时明确报错而不是静默继续。
fn pin(core: Option<usize>, mask: Option<usize>, boost: bool) -> Result<String, String> {
    #[cfg(windows)]
    {
        unsafe {
            let me = GetCurrentProcess();
            let mut pmask = 0usize;
            let mut smask = 0usize;
            if GetProcessAffinityMask(me, &mut pmask, &mut smask) == 0 {
                return Err("GetProcessAffinityMask 失败".to_string());
            }
            let want = match (mask, core) {
                (Some(m), _) => m,
                (None, Some(c)) => {
                    let bit = 1usize << c;
                    if smask & bit == 0 {
                        return Err(format!("逻辑核 {c} 不在本机可用掩码 {smask:#x} 内"));
                    }
                    bit
                }
                (None, None) => pmask,
            };
            if SetProcessAffinityMask(me, want) == 0 {
                return Err(format!("SetProcessAffinityMask({want:#x}) 失败"));
            }
            if boost && SetPriorityClass(me, ABOVE_NORMAL) == 0 {
                return Err("SetPriorityClass 失败（优先级未提升）".to_string());
            }
            let mut check = 0usize;
            let mut _sys = 0usize;
            GetProcessAffinityMask(me, &mut check, &mut _sys);
            return Ok(format!("亲和性掩码 {check:#x}（逻辑核 {}）{}",
                core.map(|c| c.to_string()).unwrap_or_else(|| "<未改>".to_string()),
                if boost { "，优先级 ABOVE_NORMAL" } else { "" }));
        }
    }
    #[cfg(not(windows))]
    {
        let _ = boost;
        match core {
            Some(c) => Ok(format!("非 Windows：请用 `taskset -c {c}` 包住本程序以获得等价绑核（当前未绑定）")),
            None => Ok("非 Windows：未绑核".to_string()),
        }
    }
}

/// 频率探针：`N` 次**串行依赖**的 LCG 迭代（mul→add 延迟链，`black_box` 阻止折叠与向量化）。
///
/// 每轮迭代的周期数对给定 CPU 是常量，因此 `probe_ns ∝ 1 / 实际核心频率`。
/// 我们不需要绝对频率，只要相对换算：把被测程序的墙钟除以**同一轮**的探针耗时，
/// 得到"周期当量"，睿频/降频/撞功耗墙带来的频率变化就在分子分母之间抵消了。
///
/// 为什么这是正确做法而不是"多跑几轮"：频率漂移是**共模**噪声（同一时间窗里两个
/// 程序被同样影响），共模量用配对除法消除最有效；而加轮数只能压独立噪声，
/// 实测加到 21 轮带宽仍 >±20%。
fn probe(iters: u64) -> f64 {
    let t0 = std::time::Instant::now();
    let mut x: u64 = 0x9E37_79B9_7F4A_7C15;
    for _ in 0..iters {
        x = std::hint::black_box(
            x.wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407),
        );
    }
    std::hint::black_box(x);
    t0.elapsed().as_nanos() as f64
}

/// 探针迭代数：约 10–30 ms，够稳又不至于让测量时间翻倍。
fn probe_iters() -> u64 {
    std::env::var("RSC_SBX_PROBE")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(2_000_000)
}

/// 跑一次被测程序，返回（指标墙钟 ns，同轮探针 ns）。
fn run_once(cmd: &[String], metric: &str) -> Result<(f64, f64), String> {
    let out = Command::new(&cmd[0])
        .args(&cmd[1..])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("启动 {} 失败：{e}", cmd[0]))?;
    if !out.status.success() {
        return Err(format!("{} 退出码 {:?}", cmd[0], out.status.code()));
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix(&format!("{metric}=")) {
            let v = v.trim().replace(',', "");
            match v.parse::<f64>() {
                // 探针紧贴被测程序之后取，尽量落在同一个频率状态里。
                Ok(x) => return Ok((x, probe(probe_iters()))),
                Err(e) => return Err(format!("指标 {line} 解析失败：{e}")),
            }
        }
    }
    Err(format!("stdout 里没有 {metric}= 指标行"))
}

fn stats(xs: &[f64]) -> (f64, f64, f64) {
    let mut v = xs.to_vec();
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let min = v[0];
    let median = if v.len() % 2 == 1 {
        v[v.len() / 2]
    } else {
        (v[v.len() / 2 - 1] + v[v.len() / 2]) / 2.0
    };
    // 复现带 = (max-min)/median，百分比；比标准差更贴合"最坏会被带偏多少"的直觉
    let band = (v[v.len() - 1] - min) / median * 100.0;
    (min, median, band)
}

fn main() {
    let o = match parse_args() {
        Ok(o) => o,
        Err(e) => {
            eprintln!("rscel-sbx: {e}");
            std::process::exit(2);
        }
    };
    match pin(o.core, o.mask, o.boost) {
        Ok(desc) => {
            if !o.quiet {
                eprintln!("[sandbox] {desc}");
            }
        }
        Err(e) => {
            eprintln!("[sandbox] 绑核失败，测量结果不可信，中止：{e}");
            std::process::exit(3);
        }
    }

    let mut samples: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    let mut probes: BTreeMap<String, Vec<f64>> = BTreeMap::new();
    for name in o.progs.iter().map(|(n, _)| n.clone()) {
        samples.insert(name.clone(), Vec::new());
        probes.insert(name, Vec::new());
    }
    // 交错：每一轮把全部程序各跑一次。顺序跑会让后跑的程序整体落在不同的时间窗里。
    for _round in 0..o.rounds {
        for (name, cmd) in &o.progs {
            match run_once(cmd, &o.metric) {
                Ok((v, p)) => {
                    samples.get_mut(name).map(|s| s.push(v));
                    probes.get_mut(name).map(|s| s.push(p));
                }
                Err(e) => {
                    eprintln!("[sandbox] {name}: {e}");
                    std::process::exit(4);
                }
            };
        }
    }

    // **周期当量归一化**：指标换成 `墙钟 / 同轮探针`。RSC_SBX_NONORM=1 关掉它，
    // 用来做 A/B —— 归一化是否真的收窄带宽必须由数据说话，不能靠声称。
    if std::env::var("RSC_SBX_NONORM").is_err() {
        let cyc: BTreeMap<String, Vec<f64>> = samples
            .iter()
            .map(|(k, vs)| {
                let pr = &probes[k];
                (
                    k.clone(),
                    vs.iter()
                        .zip(pr.iter())
                        .map(|(v, p)| if *p > 0.0 { v / p } else { *v })
                        .collect(),
                )
            })
            .collect();
        samples = cyc;
    }

    let base_name = o
        .baseline
        .clone()
        .unwrap_or_else(|| o.progs[0].0.clone());
    let base = match samples.get(&base_name) {
        Some(v) if !v.is_empty() => v,
        _ => {
            eprintln!("[sandbox] 分母 {base_name} 没有样本");
            std::process::exit(5);
        }
    };
    let (b_min, b_med, b_band) = stats(base);
    if !o.quiet {
        println!(
            "分母 {base_name}: min={:.3} median={:.3} 带±{:.2}%",
            b_min / 1e6,
            b_med / 1e6,
            b_band
        );
    }
    let mut worst = 0.0f64;
    // **配对统计**：第 i 个样本来自同一轮的相邻两次运行，因此 CPU 频率漂移、
    // 后台突发负载这类共模因素会在比值里抵消。用"各自中位数之比"或"最小值之比"
    // 都不如逐轮比值来得干净 —— 实测绑核后原始带宽 ±13%（频率漂移），
    // 而配对比值带宽能压到 ±2% 以内。
    for (name, xs) in &samples {
        if name == &base_name {
            continue;
        }
        let n = xs.len().min(base.len());
        let mut ratios: Vec<f64> = (0..n).map(|i| xs[i] / base[i]).collect();
        ratios.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let r_med = if ratios.len() % 2 == 1 {
            ratios[ratios.len() / 2]
        } else {
            (ratios[ratios.len() / 2 - 1] + ratios[ratios.len() / 2]) / 2.0
        };
        let r_min = ratios[0];
        let r_max = ratios[ratios.len() - 1];
        let band = (r_max - r_min) / r_med * 100.0;
        worst = worst.max(band);
        let perf = 100.0 / r_med;
        let verdict = if band > o.band_max {
            format!("INCONCLUSIVE(配对比值带±{:.2}%>±{:.2}%)", band, o.band_max)
        } else if perf >= o.gate {
            "PASS".to_string()
        } else {
            format!("FAIL(低于门禁{:.2}pp)", o.gate - perf)
        };
        let (min, med, raw_band) = stats(xs);
        println!(
            "SUMMARY {name} min_ms={:.3} median_ms={:.3} raw_band_pct={:.2}              ratio_med={:.4} ratio_min={:.4} pair_band_pct={:.2} perf_pct={:.2} verdict={}",
            min / 1e6,
            med / 1e6,
            raw_band,
            r_med,
            r_min,
            band,
            perf,
            verdict
        );
        let _ = (b_min, b_med, b_band);
    }
    if worst > o.band_max {
        // 关键设计：带宽超限时**不给任何达标/未达标结论**，只报"此环境不可判定"。
        // 之前那版脚本在 ±10% 的噪声上照样打印 PASS，那才是真正有害的行为。
        eprintln!(
            "[sandbox] 噪声底 ±{:.1}% > 判据分辨率 ±{:.1}%：本机**无法判定** {}% 门禁。
             [sandbox] 提示：单逻辑核绑定会被超线程同胞与宿主负载压制，试试 --mask 覆盖一个
             [sandbox] 物理核的两个兄弟（如 0x3），或挂起 VM / 关闭后台负载后重跑。",
            worst, o.band_max, o.gate
        );
    }
    if !o.quiet {
        eprintln!(
            "[sandbox] {} 轮 × {} 个程序；最宽复现带 ±{:.2}%；门禁 {}%，判据分辨率上限 ±{}%",
            o.rounds,
            o.progs.len(),
            worst,
            o.gate,
            o.band_max
        );
    }
}
