// 场景 3 的手写等价对照：Arc<String> 在 8 个线程间共享，每元素一次 Arc::clone + 一次读取。
// 元素数与 bench/read_share.rscel 同步（2026-10-11 起为 50 万，放大理由见该文件头）。
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

fn consumer(x: i64, data: &Arc<String>) {
    let _h = std::hint::black_box(data.as_bytes()[7]) ^ std::hint::black_box(x as u8);
}

fn run(items: Vec<i64>, limit: usize, data: &Arc<String>) {
    let n = items.len();
    let pool = Mutex::new(VecDeque::from(items));
    let nthreads = limit.max(1).min(n);
    std::thread::scope(|scope| {
        for _ in 0..nthreads {
            scope.spawn(|| loop {
                let taken = {
                    let mut p = pool.lock().unwrap();
                    if p.is_empty() {
                        None
                    } else {
                        p.pop_front().map(|item| (n - p.len() - 1, item))
                    }
                };
                let Some((_, item)) = taken else { break };
                let shared = Arc::clone(data);
                consumer(item, &shared);
            });
        }
    });
}

fn main() {
    let t0 = std::time::Instant::now();
    let payload = Arc::new("rscel read-only payload for arc benchmark".repeat(64));
    let items: Vec<i64> = (1..=500_000i64).collect();
    let spins = items.len();
    run(items, 8, &payload);
    println!("spins={}", spins);
    println!("elapsed_ns={}", t0.elapsed().as_nanos());
}
