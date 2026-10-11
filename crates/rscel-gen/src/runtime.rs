//! 嵌入生成物的类型化并发运行时（§14.1 + S1 选型 + S2 调用约定）。
//!
//! 生成物必须**零外部依赖**（§1.4 验收），所以不依赖 `rscel-runtime` 的
//! `Packet`/`Group`，而是把最小运行时原样嵌入每个生成文件。
//!
//! ## 为什么自己写而不复用 rscel-runtime
//!
//! S2 冻结决策：Cell 调用走**静态类型化** monomorphized（`cell_w(x) -> Option<Ret_w>`）。
//! 而 `rscel-runtime` 的 `Group::spawn` 签名是 `Future<Output = Packet>`——类型擦除，
//! 实测比类型化慢 69–89 倍（见 `docs/00-内存模型与选型报告.md` §2.2）。
//! 所以类型化路径必须有自己的并发原语；`rscel-runtime` 仅供 `--dyn-packets`
//! 后端（默认关闭）与运行时自身测试使用。
//!
//! ## 组成
//!
//! - `__rsc_block_on`：Condvar 阻塞式驱动一个 future（不忙等）。
//! - `__rsc_run_group`：有界并发执行一批任务，**结果只保留前 `slots` 个**。
//! - `__RscGroup<T>`：跨语句的组句柄。生成代码为每个 group 分配一个局部变量
//!   （`jump and` 赋值、`jump sync` 消费），因此**无需全局注册表，也无 `unsafe`**。
//!
//! ## 内存（D4 增量回收 + 诚实上界）
//!
//! - **派生侧（结果）** = O(并发上限 + slots)。线程数 = `min(limit, n)`，
//!   元素数 = N，slots = 该组被多包 receive 静态枚举出的最大下标+1。
//!   持有 `slots` 个 `Mutex<Option<T>>` 结果槽，**与 N 无关** —— A.3
//!   「一千万文件」场景下，**结果侧**内存恒定。
//! - **输入侧（原始 `Vec`）** = O(元素数 = N)。`__rsc_run_group` 用
//!   `items.into_iter().enumerate()` **移动**接管用户 `Vec`（不额外拷贝，
//!   不 Clone），但 `into_iter` 只在组跑完时归还，**所以组跑期间用户原 `Vec`
//!   不能被复用**。这是 P2-2 诚实的上界声明：输入侧仍是 O(N)，不是
//!   「与 N 无关」。早期版本用 `Vec<Option<T>>` 把每个元素从 8 字节涨到
//!   16 字节（P2-2 修前）已被避免 —— 不再做无谓的 2× 拷贝。
//!   输入集合本身是用户持有的数据，不计入派生内存。

/// 生成物顶部的运行时源码（直接拼到生成的 .rs 文件头部）。
///
/// 命名一律 `__rsc_` / `__Rsc` 前缀，避免与用户 DSL 透传的标识符冲突。
pub const RUNTIME_SOURCE: &str = r#"
// ===================== 内嵌类型化运行时（rscel-gen 生成）=====================

use std::sync::{Arc, Condvar, Mutex};

/// 从 panic payload 提取可读消息。
fn __rsc_panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = p.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = p.downcast_ref::<String>() {
        return s.clone();
    }
    "<非字符串 panic payload>".to_string()
}

#[inline(never)]
fn __rsc_block_on<F: std::future::Future<Output = T>, T>(fut: F) -> T {
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
    // future 固定在**栈上**，不用 `Box::pin`。
    //
    // 为什么这一行值 15%（`docs/06-控制变量实验.md` 实验八，同一窗、同一组 flag、
    // 同一校验值）：`Box::pin` 之后，poll 里的每一轮迭代都要穿过那个堆指针读写
    // future 的字段；`%r8`（Box 指针）可能与其它存储别名，LLVM 只能每轮重新载入并
    // 把状态写回堆 —— 实测热循环因此多出 5 条穿过堆指针的存储（`movq %rdi, 56(%r8)` /
    // `movb $0, 64(%r8)` …），而栈上 pin 的形状是同一批字段落在**固定帧偏移**上
    // （`movq %rdi, 24(%rbp)`）。量级 = 每轮约一个周期：`bench/async_step.rscel`
    // （5e7 轮）中位 81.80ms → 68.20ms（−16.6%，两窗同号 −16.6% / −14.1%，
    // 复现带 ≤5.1%）。`padd` 两格都是 0，所以这**不是**向量化开关，就是那次指针追逐。
    //
    // 代价照实写：future 的状态从堆搬到 block_on 的栈帧里。跨 await 存活的字段
    // 因此占用线程栈（Rust 默认线程栈 2MiB）。生成物里 Cell 的 future 只装 DSL 的
    // 局部（i64 / Arc / String 这类），而 `__rsc_block_on` 不递归、也不在
    // `async_recursion` 那种自回边上，所以栈占用是有界的；用户在 `extern rust` 里
    // 声明**大数组并跨 await 持有**时才会顶到这条边界 —— 那种写法本来也该自己装箱。
    // 之所以仍选栈：手写同形状 Rust 的 block_on 就是这个形状，而"生成物与手写同构"
    // 正是 §17.1 的口径。
    let mut fut = std::pin::pin!(fut);
    loop {
        // 先清标志再 poll，避免"poll 期间置位 → 随后无条件挂起"的丢失唤醒。
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

/// 并发组句柄：`jump and` 创建并 spawn，`jump sync` 阻塞取结果。
///
/// 结果类型是 `Result<Vec<Option<T>>, String>`：整组 panic 时存 `Err`，让
/// `wait()` 能带着**真实原因**报错，而不是无声挂死。
struct __RscGroup<T> {
    done: Mutex<Option<Result<Vec<Option<T>>, String>>>,
    cvar: Condvar,
}

impl<T> __RscGroup<T> {
    fn new() -> Self {
        __RscGroup { done: Mutex::new(None), cvar: Condvar::new() }
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
    /// 阻塞等待完成并取走结果（可重复调用，第二次返回空 vec）。
    fn wait(self: &Arc<Self>) -> Vec<Option<T>> {
        let mut d = self.done.lock().unwrap();
        while d.is_none() {
            d = self.cvar.wait(d).unwrap();
        }
        match d.take() {
            Some(Ok(v)) => v,
            // 整组 panic：把真实原因带到调用点，而不是让 `jump sync` 永久阻塞。
            Some(Err(msg)) => panic!("rscel: 并发组执行失败（派生实例 panic）: {msg}"),
            None => Vec::new(),
        }
    }
}

/// supervisor 侧入口：**保证 `g.finish*` 一定被调用**。
///
/// 没有这层保护时，工作线程的 panic 会让 `thread::scope` 重新 panic →
/// supervisor 线程死掉 → `done` 永远是 `None` → `wait()` 永久挂死。
/// 这里用 `catch_unwind` 包裹 `__rsc_run_group`：单实例 panic 已被
/// `__rsc_run_group` 内的 inner `catch_unwind` 隔离为 `None`（不影响整组），
/// 真正的 supervisor 级 panic（mutex 中毒 / OOM 等）才走到这里，通过
/// `g.finish_panicked` 把真实原因带回去，`wait()` 再 panic 报错。
fn __rsc_group_run<I, O, F, Fut>(
    items: Vec<I>,
    limit: usize,
    slots: usize,
    g: &Arc<__RscGroup<O>>,
    f: F,
) where
    I: Send,
    O: Send,
    F: Fn(I) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Option<O>>,
{
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        __rsc_run_group(items, limit, slots, f)
    }));
    match r {
        Ok(v) => g.finish(v),
        Err(p) => g.finish_panicked(__rsc_panic_msg(&p)),
    }
}

/// 有界并发执行 `items` 的每一项，结果只保留前 `slots` 个（D4 / D5）。
///
/// - 线程数 = `min(limit.max(1), items.len())`；
/// - 元素从池中**移动**取出（不 Clone）；
/// - 索引 `>= slots` 的结果直接丢弃 → 内存 O(limit + slots)，与元素总数无关。
///
/// 输入类型 `I` 与输出类型 `O` 是**独立**的：worker 通常是"收 `i64`、回 `Option<Ret_x>`"。
///
/// **同步版**（默认路径）：闭包直接返回 `Option<O>`，每个元素**不经过任何 future**。
/// 之前的版本统一用 `Fut: Future` + `__rsc_block_on(f(item))`，于是每个元素都要
/// `Box::pin` 一个 future + 造一个 `Waker`（Arc 分配）再 poll 一次——对
/// 「一千万文件」这类每个元素只做几纳秒工作的场景，这点固定开销就是主要成本。
/// async 目标 Cell 走下面的 [`__rsc_run_group`]（保留 future 版本）。
/// （上面那句 `Box::pin` 说的是**当时**的形状：`__rsc_block_on` 现已改为把 future 固定在
/// 栈上，见上面那条注释；`Waker` 那一次 Arc 分配仍然在。）
fn __rsc_run_group_sync<I, O, F>(
    items: Vec<I>,
    limit: usize,
    slots: usize,
    f: F,
) -> Vec<Option<O>>
where
    I: Send,
    O: Send,
    F: Fn(I) -> Option<O> + Send + Sync,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let keep = slots.min(n);
    let nthreads = limit.max(1).min(n);

    // 待办池：`VecDeque` 直接 `pop_front` 移动出队——元素保持原大小。
    // （之前用 `Vec<Option<I>>` 把每个元素从 8 字节撑到 16 字节，
    //   并且在派生瞬间多分配一份等大的 Vec；1000 万元素 = 多 80 MB + 一次全量拷贝。）
    //
    // **索引与出队必须在同一个临界区里做**：`g.Wk` 的第 k 个槽位必须对应集合的
    // 第 k 个元素（§6.4 入组顺序），若把"取索引"和"取元素"拆成两把锁，
    // 两个线程之间就会出现索引与元素错位的竞态。
    let pool = Mutex::new(std::collections::VecDeque::<I>::from(items));
    let results: Vec<Mutex<Option<O>>> = (0..keep).map(|_| Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for _ in 0..nthreads {
            scope.spawn(|| {
                loop {
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
                    let r = f(item);
                    if i < keep {
                        *results[i].lock().unwrap() = r;
                    }
                }
            });
        }
    });

    results.into_iter().map(|m| m.into_inner().unwrap()).collect()
}

/// `__rsc_run_group_sync` 的 panic 安全包装（supervisor 侧，保证 `finish*` 必被调用）。
fn __rsc_group_run_sync<I, O, F>(
    items: Vec<I>,
    limit: usize,
    slots: usize,
    g: &Arc<__RscGroup<O>>,
    f: F,
) where
    I: Send,
    O: Send,
    F: Fn(I) -> Option<O> + Send + Sync,
{
    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        __rsc_run_group_sync(items, limit, slots, f)
    }));
    match r {
        Ok(v) => g.finish(v),
        Err(p) => g.finish_panicked(__rsc_panic_msg(&p)),
    }
}

/// 有界并发执行（**async 目标 Cell** 版本）：闭包返回 future，由 `__rsc_block_on` 驱动。
fn __rsc_run_group<I, O, F, Fut>(
    items: Vec<I>,
    limit: usize,
    slots: usize,
    f: F,
) -> Vec<Option<O>>
where
    I: Send,
    O: Send,
    F: Fn(I) -> Fut + Send + Sync,
    Fut: std::future::Future<Output = Option<O>>,
{
    let n = items.len();
    if n == 0 {
        return Vec::new();
    }
    let keep = slots.min(n);
    let nthreads = limit.max(1).min(n);

    // 待办池：单个 Mutex 保护，元素移动出队（不 Clone）。
    let pool = Mutex::new((items.into_iter().map(Some).collect::<Vec<_>>(), 0usize));
    // 结果槽：只分配 keep 个（keep <= slots）。
    let results: Vec<Mutex<Option<O>>> = (0..keep).map(|_| Mutex::new(None)).collect();

    std::thread::scope(|scope| {
        for _ in 0..nthreads {
            scope.spawn(|| {
                loop {
                    // 取下一个待办元素（移动语义）。
                    let taken = {
                        let mut g = pool.lock().unwrap();
                        if g.1 >= n {
                            None
                        } else {
                            let i = g.1;
                            g.1 += 1;
                            g.0.get_mut(i).and_then(Option::take).map(|v| (i, v))
                        }
                    };
                    let Some((i, item)) = taken else { break };
                    let r = __rsc_block_on(f(item));
                    if i < keep {
                        *results[i].lock().unwrap() = r;
                    }
                    // i >= keep：结果直接丢弃（D4）。
                }
            });
        }
    });

    results.into_iter().map(|m| m.into_inner().unwrap()).collect()
}
"#;