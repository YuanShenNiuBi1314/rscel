// async 形状的同构手写对照（`bench/async_step.rscel` 的分母）—— 写法和
// `bench/loop_sum_opaque_fn_hand.rs` 一致：**逐处照抄生成物的结构**，只留两处已实测免费的
// 有意差异。为什么需要它：`docs/05` 的测量纪律规矩 2 要求"基准两边同构"，而 async 形状
// 此前**没有任何分母** —— 没有它就无法诚实回答"async 程序转译掉了多少"。
//
// 逐字照抄的部分（对照 `rscel build bench/async_step.rscel --release` 的生成物）：
//   · `#[inline(always)] fn cell_main() -> Option<RetMain>` 外壳，里面是
//     `{ let _ = <block_on>(cell_sum(..)); return Some(RetMain {}); }` 那个块表达式；
//   · 起始值在**进入 Cell 之前**过一次 `black_box`（生成物的调用点就是 `black_box(0i64)`/
//     `black_box(1i64)` 两个实参）；
//   · S2 类型化调用约定的**别名拷贝**（生成物 `v_a`/`v_b` ↔ 这里 `acc`/`k`）；
//   · `t0` 先取、`limit` 在计时区内再过一次 `black_box`；
//   · `let mut next_acc; let mut next_k;` 的 **hoisted 声明**形状；
//   · 每轮 `rsc_step(acc + k).await`（对应 DSL 透传的那次 await）；
//   · 执行器与内嵌运行时的 `__rsc_block_on` **同构**：同一个 `#[inline(never)]`、同一个
//     Condvar 挂起、同一个栈上 `pin!`。不同构的话这一格量的就成了"执行器值多少"，
//     而不是"转译值多少"（栈上 pin 那条是本批 P0-5 第 3 项落地的，见 docs/06 实验八）。
//
// 有意差异只剩两处，都在 `docs/06` 实验六/五里被单因素实测判成免费：
// ① 没有整段内嵌运行时的死代码（组调度那几百行）—— V08 四窗 +0.16% / 0.00% / −0.16% / 0.00%；
// ② `main` 里没有 `let __rsc_argv = std::env::args().skip(1).collect();` —— `GEN_noargv`
//    那一格实测在复现带内。
#![allow(dead_code, unused_assignments, non_snake_case)]

use std::sync::{Arc, Condvar, Mutex};

#[derive(Default)]
struct RetMain {}

#[derive(Default)]
struct RetSum {}

/// 与生成物 `__rsc_block_on` 同构的阻塞执行器（名字去掉 `__rsc_` 前缀）。
#[inline(never)]
fn block_on<F: std::future::Future<Output = T>, T>(fut: F) -> T {
    use std::task::{Context, Poll, Wake, Waker};

    struct Sig {
        flag: Mutex<bool>,
        cvar: Condvar,
    }
    struct ThreadWake {
        sig: Arc<Sig>,
    }
    impl Wake for ThreadWake {
        fn wake(self: Arc<Self>) {
            self.wake_by_ref();
        }
        fn wake_by_ref(self: &Arc<Self>) {
            let mut f = self.sig.flag.lock().unwrap();
            *f = true;
            drop(f);
            self.sig.cvar.notify_all();
        }
    }

    let sig = Arc::new(Sig { flag: Mutex::new(false), cvar: Condvar::new() });
    let waker = Waker::from(Arc::new(ThreadWake { sig: Arc::clone(&sig) }));
    let mut cx = Context::from_waker(&waker);
    let mut fut = std::pin::pin!(fut);
    loop {
        {
            let mut f = sig.flag.lock().unwrap();
            *f = false;
        }
        if let Poll::Ready(v) = fut.as_mut().poll(&mut cx) {
            return v;
        }
        let mut f = sig.flag.lock().unwrap();
        while !*f {
            f = sig.cvar.wait(f).unwrap();
        }
    }
}

/// DSL 里顶层 `extern rust` 透传的那个 async fn（用户代码，逐字照搬）。
async fn rsc_step(x: i64) -> i64 {
    std::hint::black_box(x)
}

#[inline(always)]
fn cell_main() -> Option<RetMain> {
    {
        let _ = block_on(cell_sum(std::hint::black_box(0i64), std::hint::black_box(1i64)));
        return Some(RetMain {});
    }
}

async fn cell_sum(a: i64, i: i64) -> Option<RetSum> {
    let mut acc: i64 = a;
    let mut k: i64 = i;
    let t0 = std::time::Instant::now();
    let limit: i64 = std::hint::black_box(1000000000i64);
    let mut next_acc;
    let mut next_k;
    loop {
        next_acc = rsc_step(acc + k).await;
        next_k = k + 1;
        if !(next_k <= limit) {
            break;
        }
        acc = next_acc;
        k = next_k;
    }
    println!("checksum={}", acc);
    println!("elapsed_ns={}", t0.elapsed().as_nanos());
    return Some(RetSum {});
}

fn main() {
    let _ = cell_main();
}
