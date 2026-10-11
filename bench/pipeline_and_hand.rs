// 场景 2 的手写等价对照：8 线程有界并发、Mutex + VecDeque 出队（与生成物的
// `__rsc_run_group_sync` 同结构），每个元素走一次"两跳"函数调用链。
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};

fn stage1(v: i64) -> Option<i64> {
    let y = v * 2 + 1;
    Some(y)
}

fn w(x: i64) -> Option<i64> {
    stage1(x)
}

/// 与生成物同形的组：结果只保留前 `slots` 个（这里 slots = 0，等价丢弃全部结果）。
fn run_group(items: Vec<i64>, limit: usize, slots: usize) -> Vec<Option<i64>> {
    let n = items.len();
    let keep = slots.min(n);
    let nthreads = limit.max(1).min(n);
    let pool = Mutex::new(VecDeque::from(items));
    let results: Vec<Mutex<Option<i64>>> = (0..keep).map(|_| Mutex::new(None)).collect();
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

fn main() {
    let t0 = std::time::Instant::now();
    let items: Vec<i64> = (1..=2_000_000i64).collect();
    let total = items.len();
    let g = Arc::new((Mutex::new(()), Condvar::new()));
    let _ = g;
    let res = run_group(items, 8, 0);
    println!("done={}", total + res.len());
    println!("elapsed_ns={}", t0.elapsed().as_nanos());
}
