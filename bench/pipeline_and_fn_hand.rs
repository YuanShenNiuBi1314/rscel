// 场景 2 的第二份手写对照：**与生成物同构**的写法（docs/05 P0-3 要求的同构分母）。
//
// 为什么需要这一份：`pipeline_and_hand.rs` 把并发组当**普通函数**直接调用（没有
// supervisor 线程、没有 group 句柄、worker 是真实的两跳函数链），于是它和生成物之间
// 同时差着「转译质量」和「运行时脚手架」两种东西，报出来的百分比没法归因。本文件逐处
// 对齐 `rscel build bench/pipeline_and.rscel --release` 的生成物：
//
//   · `main` 只做 `let _ = cell_main()`；
//   · `jump and ... group g1` + `jump sync g1` = **另起一个线程跑组**、主线程阻塞等结果；
//     生成物用 `Arc<__RscGroup>` + `Condvar` + `catch_unwind`（`jump sync` 不能因为
//     supervisor 线程 panic 而永久挂死），**并且不 join supervisor** —— 结果一 publish
//     就返回。这里逐字照做，原因见文件末尾的 ③；
//   · 待办池 `Mutex<VecDeque>`，**索引与出队必须落在同一个临界区**（§6.4 入组顺序），
//     线程数 `limit.max(1).min(n)`，`keep = slots.min(n) = 0`，结果槽 0 个；
//   · worker 体 = 生成物把 `stage1` 按 P0-4 源码级展开后的逐字形状：`y = v*2+1` 算完
//     即丢（DSL 里 `w` 不 `return` stage1 的结果，所以两边这段算术都是死代码，
//     每元素真正的工作只剩「出队 + 判定」—— 这正是 P0-3 要量的那一段）。
//
// 与生成物的**有意**差异只有两处：
// ① 不带 `#[inline(always)]`/`#[inline]`（那是代码生成器的策略，人手写不会加）。
//    本批按单因素法验过它在**这个形状**里不收费：给 worker 补上提示，两窗读数
//    14.63 → 14.63ms、14.97 → 14.60ms（±0.4%，复现带内），与 `loop_sum` 的
//    V02→V03 +37% 相反 —— 每元素工作量由互斥锁出队主导，函数边界不占（数据在 docs/06）；
// ② 没有整段内嵌运行时的死代码（docs/06 的 V06 格实测差 0.1%，不是因素）。
//
// ③ **曾经是第三处差异，现在按"同构"定义消掉了**（2026-10-11，docs/05 P0-5 第 4 项）。
// 这一份原来用 `supervisor.join()` 取结果，而生成物的 `jump sync` **不 join supervisor**：
// 结果一 publish 就返回。既然本文件的约定是"与生成物同构"，这一处就不该留着当差异，
// 所以改成与运行时一致的 publish/wait。
//
// 换完的实测后果（`bash scripts/bench/orch.sh` 的 A 系列阶梯，同窗交错、四个 101 轮窗；
// 每格取自己最小值，单位 ms）：
//   A0_base 13.08 / 13.39 / 13.40 / 13.12   vs   GEN_real 12.91 / 13.22 / 13.44 / 13.07
//   差值 +1.3% / +1.3% / −0.3% / +0.4% → **四窗全部在复现带里**（读作打平，谁也不比谁快）。
// 改之前同一批阶梯里，分母（旧 F0）比生成物慢 +3.7% / +5.1% / +8.3%（第四个窗反向 −2.5%，
// 而那整窗带 >5%、按纪律作废）—— 也就是说 P0-3 记的那笔"生成物比同构手写**快** 4%–6%"
// 的反常读数，主要来自**分母自己带的形状差**，不是转译白赚；消掉它之后 `pipeline_and`
// 报出的百分比**下降**，属加严而非放松。
//
// 但**机制没有全归因干净**，别把上面读成"join 一项就值 5%"：反向探针（A4 = 只把交接换回
// `join()`）两个可用窗读到 +2.4%（带 ±1.0%/±1.2%）与 +5.1%（带 ±0.7%/±2.1%）——
// **方向复现了（join 那一侧更慢），幅度没复现**（正 −4.1%…−8.7%、反 +2.4%…+5.1%，
// 跨度比差值本身还大），第三个窗更是直接测废（A4 10.66ms，带 ±27.5%）。加上本批还抓到编排场景里
// **一次幸运运行把 `min` 定住**（`A3_hint` 读出 7.42ms、比同窗快 43%，14 次直采样复验却
// 与其余格同带；复验表在 docs/06 实验三的「补记」）—— 也就是说这几个百分点里，
// 有一部分是**统计量与调度的产物**，不是代码属性。能站住的结论只有两条：
// **同构性 = 结构理由（成立）；分母修正让门禁变严、旧 106% 不能当成绩（实测后果成立）；
// 单一机制 + 具体百分比 = 未证实，不写进 README。**
//
// ④ **本文件的元素数在 2026-10-11 从 20 万放大到 200 万**（docs/05 P0-5 第 5 项「换量法」①）：
//   放大前单次运行 ~13ms，编排场景的相位复现带 ±2.2%–7.8% 比要判的差值还宽。
//   **上面 ①②③ 里抄录的所有 ms 与百分比读数都是 20 万那一版的**，只能当历史证据读，
//   别拿去对今天的输出。
#![allow(dead_code, unused_assignments, non_snake_case)]

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

struct RetW {}

/// 与生成物 `cell_w` 同形：`jump stage1` 的 body 已被展开进这里，结果无人消费。
fn w(x: i64) -> Option<RetW> {
    let y = x * 2 + 1;
    let _ = y;
    Some(RetW {})
}

/// 与生成物 `__rsc_run_group_sync` 同形（slots = 0 → 结果全丢，D4 增量回收）。
fn run_group_sync(items: Vec<i64>, limit: usize, slots: usize) -> Vec<Option<RetW>> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let keep = slots.min(n);
    let nthreads = limit.max(1).min(n);

    let pool = Mutex::new(VecDeque::<i64>::from(items));
    let results: Vec<Mutex<Option<RetW>>> = (0..keep).map(|_| Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for _ in 0..nthreads {
            scope.spawn(|| loop {
                let taken = {
                    let mut p = pool.lock().unwrap();
                    if p.is_empty() {
                        None
                    } else {
                        let i = n - p.len();
                        p.pop_front().map(|item| (i, item))
                    }
                };
                let Some((i, item)) = taken else { break };
                let r = w(item);
                if i < keep {
                    *results[i].lock().unwrap() = r;
                }
            });
        }
    });

    results.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

/// 与生成物 `__RscGroup` 同形：`jump sync` 用 publish/wait 交接，**不等 supervisor 线程
/// 退出**；外层 `catch_unwind` 让 supervisor 级 panic 变成带原因的报错而不是永久挂死。
/// 这两件事都是每组一次的开销，不在每元素热路径上 —— 但它值 5%–9%（见文件头 ③）。
struct Group<T> {
    done: Mutex<Option<Result<Vec<Option<T>>, String>>>,
    cvar: Condvar,
}

impl<T> Group<T> {
    fn new() -> Self {
        Group { done: Mutex::new(None), cvar: Condvar::new() }
    }
    fn finish(self: &Arc<Self>, v: Vec<Option<T>>) {
        {
            let mut d = self.done.lock().unwrap();
            *d = Some(Ok(v));
        }
        self.cvar.notify_all();
    }
    fn finish_panicked(self: &Arc<Self>, msg: String) {
        {
            let mut d = self.done.lock().unwrap();
            *d = Some(Err(msg));
        }
        self.cvar.notify_all();
    }
    fn wait(self: &Arc<Self>) -> Vec<Option<T>> {
        let mut d = self.done.lock().unwrap();
        while d.is_none() {
            d = self.cvar.wait(d).unwrap();
        }
        match d.take() {
            Some(Ok(v)) => v,
            Some(Err(msg)) => panic!("并发组执行失败（派生实例 panic）: {msg}"),
            None => Vec::new(),
        }
    }
}

/// 与生成物 `__rsc_group_run_sync` 同形：supervisor 侧跑组并把结果 publish 给句柄。
fn group_run_sync(items: Vec<i64>, limit: usize, slots: usize, g: &Arc<Group<RetW>>) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_group_sync(items, limit, slots)
    }));
    match r {
        Ok(v) => g.finish(v),
        Err(p) => {
            let msg = p
                .downcast_ref::<&'static str>()
                .map(|s| (*s).to_string())
                .or_else(|| p.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<非字符串 panic payload>".to_string());
            g.finish_panicked(msg);
        }
    }
}

/// 与生成物 `cell_main` 同形：派生一个 supervisor 线程跑组，主线程在 group 句柄上等结果。
fn cell_main() {
    let t0 = std::time::Instant::now();
    let items: Vec<i64> = (1..=2_000_000i64).collect();
    let total = items.len();
    let g = Arc::new(Group::<RetW>::new());
    let g2 = Arc::clone(&g);
    std::thread::spawn(move || group_run_sync(items, 8, 0, &g2));
    let res: Vec<Option<RetW>> = g.wait();
    let _ = res;
    println!("done={}", total);
    println!("elapsed_ns={}", t0.elapsed().as_nanos());
}

/// 与生成物同形：生成物的 `main` 还采集了 argv（本场景不使用），人手写不需要。
fn main() {
    cell_main();
}
