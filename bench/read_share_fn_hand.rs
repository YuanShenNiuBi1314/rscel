// 场景 3 的第二份手写对照：**与生成物同构**的写法（docs/05 P0-3 要求的同构分母）。
//
// 逐处对齐 `rscel build bench/read_share.rscel --release` 的生成物：
//   · `main` 只调 `cell_main()`；
//   · `read with(payload)` = supervisor 侧先 `Arc::new(payload)`，再为**每组**
//     `Arc::clone` 一份交给 spawn 出来的线程（生成物里的 `__rsc_extra_0`），
//     闭包按引用共享它；
//   · **每元素一次 `Arc::clone`**：生成物是 `cell_consumer(__rsc_it, __rsc_extra_0.clone())`，
//     这是 `jump and` 的语义要求（P1-3 记过：N 个实例各要一份值），手写侧同形；
//   · 待办池 `Mutex<VecDeque>` + 索引与出队同一个临界区 + `keep = 0`；
//   · `jump sync` = 主线程在 group 句柄上 publish/wait，**不 join supervisor**
//     （与生成物 `Arc<__RscGroup>` + Condvar 同形）。为什么不该留着当差异、以及换完
//     实测到什么程度，全部写在 `bench/pipeline_and_fn_hand.rs` 文件头的 ③ 里
//     （2026-10-11，docs/05 P0-5 第 4 项：结构理由是"同构"定义本身；实测后果是分母与
//     生成物打平、报出的百分比下降；**单一机制未证实**，反向探针没复现出可复称的幅度）。
//   · worker 体逐字同形：`black_box(payload.as_bytes()[7]) ^ black_box(x as u8)`，
//     `Arc<String>` **按值**进函数再丢弃（生成物的 `let v_b: Arc<String> = payload;`）。
//
// 与生成物的有意差异同 `pipeline_and_fn_hand.rs`：不带内联提示、没有内嵌运行时死代码。
// 元素数 2026-10-11 从 5 万同步放大到 50 万（`pipeline_and_fn_hand.rs` 头部的 ④ 说明了
// 为什么放大，以及"该文件里抄录的历史 ms 读数属于放大前"这件事）。
#![allow(dead_code, unused_assignments, non_snake_case)]

use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

struct RetConsumer {}

/// 与生成物 `cell_consumer` 同形：入参 `Arc<String>` 按值进来、函数结束时释放。
fn consumer(x: i64, payload: Arc<String>) -> Option<RetConsumer> {
    let _h = std::hint::black_box(payload.as_bytes()[7]) ^ std::hint::black_box(x as u8);
    Some(RetConsumer {})
}

/// 与生成物 `__rsc_run_group_sync` 同形，闭包位换成直接的 `Arc::clone` + 调用。
fn run_group_sync(
    items: Vec<i64>,
    limit: usize,
    slots: usize,
    shared: &Arc<String>,
) -> Vec<Option<RetConsumer>> {
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let keep = slots.min(n);
    let nthreads = limit.max(1).min(n);

    let pool = Mutex::new(VecDeque::<i64>::from(items));
    let results: Vec<Mutex<Option<RetConsumer>>> = (0..keep).map(|_| Mutex::new(None)).collect();

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
                let r = consumer(item, Arc::clone(shared));
                if i < keep {
                    *results[i].lock().unwrap() = r;
                }
            });
        }
    });

    results.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

/// 与生成物 `__RscGroup` 同形：publish/wait，不等 supervisor 线程退出（理由见文件头）。
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

/// 与生成物 `__rsc_group_run_sync` 同形。
fn group_run_sync(items: Vec<i64>, limit: usize, slots: usize, shared: &Arc<String>,
                  g: &Arc<Group<RetConsumer>>) {
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        run_group_sync(items, limit, slots, shared)
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

/// 与生成物 `cell_main` 同形。
fn cell_main() {
    let t0 = std::time::Instant::now();
    let payload = "rscel read-only payload for arc benchmark".repeat(64);
    let items: Vec<i64> = (1..=500_000i64).collect();
    let spins = items.len();
    let payload = Arc::new(payload);
    let extra = Arc::clone(&payload);
    let g = Arc::new(Group::<RetConsumer>::new());
    let g2 = Arc::clone(&g);
    std::thread::spawn(move || group_run_sync(items, 8, 0, &extra, &g2));
    let res: Vec<Option<RetConsumer>> = g.wait();
    let _ = res;
    println!("spins={}", spins);
    println!("elapsed_ns={}", t0.elapsed().as_nanos());
}

fn main() {
    cell_main();
}
