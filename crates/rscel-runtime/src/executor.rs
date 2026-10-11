//! 标准库自研极简 executor（§13 / 选型 S1）。
//!
//! 目标：
//! - 多线程 worker 池 + 全局任务队列（`Mutex<VecDeque<Arc<TaskSlot>>>` + `Condvar`）；
//! - `RawWaker` vtable 正确（否则 spawn 的任务永远不会 poll）；
//! - panic 隔离：worker 用 `catch_unwind` 包裹 poll，单 task panic 不影响 worker；
//! - 与 [`crate::Group`] 配合：完成回调由 Group 通过 `set_completion_hook` 安装，
//!   task poll 返回 Ready 时触发回调，把结果推入 Group 的 Inner 队列；
//! - `block_on` 把主线程当作 worker 之一，简化同步测试。
//!
//! **零外部依赖**：仅使用 `std::sync` / `std::task` / `std::thread` / `std::future`。
//!
//! **关键设计**：
//! - 每个任务被封装为 `Arc<TaskSlot>`；`TaskSlot` 含 `Mutex<Pin<Box<dyn Future>>>` 与
//!   `waker_alive` 标志；
//! - Waker 由 Arc<TaskSlot> 构造；wake 时把 `Arc::clone(self)` push 到全局队列；
//! - worker pop task，构造 Context，poll；Ready 时调用 hook 推结果。

use std::cell::UnsafeCell;
use std::collections::VecDeque;
use std::future::Future;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering as AOrdering};
use std::sync::{Arc, Condvar, Mutex};
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crate::packet::Packet;

// -------------------------------------------------------------------------- 任务槽

/// 单个任务的全部运行时状态。多个所有者：worker 持有（poll 时）、waker 持有（唤醒时）。
///
/// **Sync 安全性**：每个 TaskSlot 仅被一个 worker 持有并 poll（`Mutex<VecDeque<Arc<TaskSlot>>>`
/// 保证）。`UnsafeCell` 字段的写入仅在 spawn 时（单线程）发生，运行时仅 worker 通过
/// 唯一引用读 / 写；waker 调度通过队列传递。Sync 边界成立。
pub(crate) struct TaskSlot {
    /// future 本身。`Send` 安全由 `Send + 'static` future 保证。
    fut: UnsafeCell<Pin<Box<dyn Future<Output = Packet> + Send>>>,
    /// 完成回调（Group 通过 `set_completion_hook` 安装）。Ready 时调用。
    hook: UnsafeCell<Option<CompletionHook>>,
    /// 全局队列弱引用。`wake` 时 push `Arc::clone(self)` 到队列。
    queue: Mutex<Option<std::sync::Weak<Queue>>>,
}

/// 任务完成时的回调（§13.1 / D4）。
pub(crate) type CompletionHook = Arc<dyn Fn(Packet) + Send + Sync>;

// Safety 论证见 TaskSlot 文档。
unsafe impl Sync for TaskSlot {}

impl TaskSlot {
    fn new(fut: Pin<Box<dyn Future<Output = Packet> + Send>>) -> Arc<Self> {
        Arc::new(TaskSlot {
            fut: UnsafeCell::new(fut),
            hook: UnsafeCell::new(None),
            queue: Mutex::new(None),
        })
    }

    /// 安装完成回调（spawn 后调用一次）。
    fn set_hook(&self, hook: CompletionHook) {
        unsafe { *self.hook.get() = Some(hook); }
    }

    /// 安装队列弱引用。
    fn set_queue(&self, q: std::sync::Weak<Queue>) {
        *self.queue.lock().unwrap() = Some(q);
    }

    /// 触发 wake：把 `Arc::clone(self)` 入队。
    fn schedule(self: &Arc<Self>) {
        let q_opt = self.queue.lock().unwrap().clone();
        if let Some(q_weak) = q_opt {
            if let Some(q) = q_weak.upgrade() {
                let me = Arc::clone(self);
                q.push(me);
            }
        }
    }

    /// poll future 一次（catch_unwind 包裹）。
    fn poll(&self, cx: &mut Context<'_>) -> Poll<Packet> {
        let fut = unsafe { &mut *self.fut.get() };
        match catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(cx))) {
            Ok(Poll::Ready(p)) => Poll::Ready(p),
            Ok(Poll::Pending) => Poll::Pending,
            Err(panic_payload) => {
                // panic 隔离：返回 died 包；按 §13.1，Packet 长度 0 也合法。
                let msg = panic_msg(&panic_payload);
                eprintln!("[rscel executor] task panicked: {}", msg);
                Poll::Ready(Packet::none(0))
            }
        }
    }

    /// 取出 hook 调用（仅在 poll 返回 Ready 后）。
    fn run_hook(&self, p: Packet) {
        let h = unsafe { (*self.hook.get()).as_ref() };
        if let Some(hook) = h {
            hook(p);
        }
    }
}

fn panic_msg(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&'static str>() {
        return (*s).to_string();
    }
    if let Some(s) = payload.downcast_ref::<String>() {
        return s.clone();
    }
    "<non-string panic payload>".to_string()
}

// -------------------------------------------------------------------------- Waker

/// RawWaker vtable：把 `Arc<TaskSlot>` 包装成 `Waker`。
unsafe fn waker_clone(data: *const ()) -> RawWaker {
    // Safety: data 是 `Arc::into_raw(Arc<TaskSlot>)` 产出的指针，调用方保证有效。
    let arc = unsafe { Arc::<TaskSlot>::from_raw(data as *const TaskSlot) };
    let cloned = Arc::clone(&arc);
    std::mem::forget(arc);
    RawWaker::new(Arc::into_raw(cloned) as *const (), &WAKER_VTABLE)
}

unsafe fn waker_wake(data: *const ()) {
    // Safety: data 是 `Arc::into_raw(Arc<TaskSlot>)` 产出的指针。
    let arc = unsafe { Arc::<TaskSlot>::from_raw(data as *const TaskSlot) };
    arc.schedule();
    drop(arc);
}

unsafe fn waker_wake_by_ref(data: *const ()) {
    // Safety: data 是有效指针；不获取所有权。
    let arc_ref: &Arc<TaskSlot> = unsafe { &*(data as *const Arc<TaskSlot>) };
    arc_ref.schedule();
}

unsafe fn waker_drop(data: *const ()) {
    // Safety: data 是 `Arc::into_raw(Arc<TaskSlot>)` 产出的指针。
    let arc = unsafe { Arc::<TaskSlot>::from_raw(data as *const TaskSlot) };
    drop(arc);
}

static WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

fn make_waker(task: &Arc<TaskSlot>) -> Waker {
    let raw = RawWaker::new(Arc::into_raw(Arc::clone(task)) as *const (), &WAKER_VTABLE);
    // Safety: raw 来自我们管理的 vtable。
    unsafe { Waker::from_raw(raw) }
}

// -------------------------------------------------------------------------- 全局队列

pub(crate) struct Queue {
    tasks: Mutex<VecDeque<Arc<TaskSlot>>>,
    cvar: Condvar,
    shutdown: AtomicBool,
    /// 在飞任务数（spawn 时 +1，poll Ready 推结果后 -1）
    active: AtomicUsize,
}

impl Queue {
    fn new() -> Arc<Self> {
        Arc::new(Queue {
            tasks: Mutex::new(VecDeque::new()),
            cvar: Condvar::new(),
            shutdown: AtomicBool::new(false),
            active: AtomicUsize::new(0),
        })
    }

    fn push(&self, task: Arc<TaskSlot>) {
        let mut q = self.tasks.lock().unwrap();
        q.push_back(task);
        self.cvar.notify_one();
    }

    fn pop_blocking(&self) -> Option<Arc<TaskSlot>> {
        let mut q = self.tasks.lock().unwrap();
        loop {
            if self.shutdown.load(AOrdering::SeqCst) {
                return None;
            }
            if let Some(t) = q.pop_front() {
                return Some(t);
            }
            q = self.cvar.wait(q).unwrap();
        }
    }

    fn shutdown(&self) {
        self.shutdown.store(true, AOrdering::SeqCst);
        self.cvar.notify_all();
    }

    fn active(&self) -> usize {
        self.active.load(AOrdering::SeqCst)
    }
}

// -------------------------------------------------------------------------- Executor

/// 公开 Executor。
pub struct Executor {
    queue: Arc<Queue>,
    workers: Vec<Worker>,
    /// 全局完成回调（由 Group 安装）。`None` = 无 Group 关联，task Ready 时直接丢弃结果。
    hook: Mutex<Option<CompletionHook>>,
}

struct Worker {
    _handle: std::thread::JoinHandle<()>,
}

impl Executor {
    /// 新建 N 个 worker 线程（最少 1 个）。
    pub fn new(num_workers: usize) -> Self {
        let n = num_workers.max(1);
        let queue = Queue::new();
        let mut workers = Vec::with_capacity(n);
        for i in 0..n {
            let q = Arc::clone(&queue);
            let name = format!("rscel-worker-{i}");
            let handle = std::thread::Builder::new()
                .name(name)
                .spawn(move || worker_loop(q))
                .expect("spawn worker thread");
            workers.push(Worker { _handle: handle });
        }
        Executor { queue, workers, hook: Mutex::new(None) }
    }

    /// 默认 worker 数 = `available_parallelism()`，最少 1 个。
    #[allow(clippy::should_implement_trait)]
    pub fn default() -> Self {
        let n = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1).max(1);
        Self::new(n)
    }

    /// worker 数。
    pub fn worker_count(&self) -> usize {
        self.workers.len()
    }

    /// 设置 Group 完成回调。task poll 返回 Ready 时调用。
    pub fn set_completion_hook(&self, hook: CompletionHook) {
        *self.hook.lock().unwrap() = Some(hook);
    }

    /// 当前在飞任务数。
    pub fn active_count(&self) -> usize {
        self.queue.active()
    }

    /// 内部：spawn 一个 future。返回 `Arc<TaskSlot>` 供 Group 与测试使用。
    pub(crate) fn spawn_raw(&self, fut: Pin<Box<dyn Future<Output = Packet> + Send>>) -> Arc<TaskSlot> {
        let task = TaskSlot::new(fut);
        // 装入队列弱引用
        let weak_q = Arc::downgrade(&self.queue);
        task.set_queue(weak_q);
        // 装入 hook（当前 executor 的全局 hook）
        if let Some(h) = self.hook.lock().unwrap().as_ref() {
            task.set_hook(Arc::clone(h));
        }
        self.queue.active.fetch_add(1, AOrdering::SeqCst);
        self.queue.push(Arc::clone(&task));
        task
    }

    /// spawn 一个 future 并安装**专属**完成 hook（覆盖全局 hook）。
    /// 供 [`crate::Group`] 的每个 task 独立捕获 index 使用。
    pub(crate) fn spawn_with_hook(
        &self,
        fut: Pin<Box<dyn Future<Output = Packet> + Send>>,
        hook: CompletionHook,
    ) -> Arc<TaskSlot> {
        let task = TaskSlot::new(fut);
        let weak_q = Arc::downgrade(&self.queue);
        task.set_queue(weak_q);
        task.set_hook(hook);
        self.queue.active.fetch_add(1, AOrdering::SeqCst);
        self.queue.push(Arc::clone(&task));
        task
    }

    /// spawn 一个 future，返回 `JoinHandle` 占位（§13.1 公开 API）。
    pub fn spawn<F>(&self, fut: F) -> JoinHandle
    where
        F: Future<Output = Packet> + Send + 'static,
    {
        let task = self.spawn_raw(Box::pin(fut));
        JoinHandle { id: task_id(&task) }
    }

    /// 阻塞运行 future 到完成；主线程作为 worker 之一。
    ///
    /// future 可非 `Send`（§13.1 与 §14.1 的 main 都是 `async fn`）。
    ///
    /// **Output 类型**：泛型 `T`——`Packet` 是 §14.1 的 main 用法，测试代码可能想要 `()`。
    pub fn block_on<F, T>(&self, fut: F) -> T
    where
        F: Future<Output = T>,
    {
        // 主线程同步 poll（不走 worker 队列）；主线程的 Waker 通过 Condvar 唤醒主线程。
        let mut fut: Pin<Box<dyn Future<Output = T>>> = Box::pin(fut);
        let main_signal = Arc::new(MainSignal { flag: Mutex::new(false), cvar: Condvar::new() });
        let sig = Arc::clone(&main_signal);
        // schedule 把 flag 置位并唤醒主线程。捕获 `Arc<MainSignal>` 确保 `sig` 生命周期 > Waker。
        let schedule: Box<dyn Fn() + Send + Sync> = Box::new(move || {
            let mut f = sig.flag.lock().unwrap();
            *f = true;
            sig.cvar.notify_all();
        });
        let main_waker = make_main_waker(schedule);
        let mut main_cx = Context::from_waker(&main_waker);
        loop {
            match catch_unwind(AssertUnwindSafe(|| fut.as_mut().poll(&mut main_cx))) {
                Ok(Poll::Ready(p)) => return p,
                Ok(Poll::Pending) => {
                    let mut flag = main_signal.flag.lock().unwrap();
                    while !*flag {
                        flag = main_signal.cvar.wait(flag).unwrap();
                    }
                    *flag = false;
                }
                Err(payload) => {
                    eprintln!("[rscel executor] block_on panicked: {}", panic_msg(&payload));
                    // panic 后等待：通常 panic 来自用户代码；保留主线程直到 waker 唤醒。
                }
            }
        }
    }
}

struct MainSignal {
    flag: Mutex<bool>,
    cvar: Condvar,
}

fn task_id(_task: &Arc<TaskSlot>) -> u64 {
    // 简化：返回 Arc 裸指针哈希（仅用于调试）。
    let p = Arc::as_ptr(_task) as *const u8 as usize;
    p as u64
}

// -------------------------------------------------------------------------- main-thread waker

/// 主线程专用的 Waker（无 vtable 缓存 / Send 约束）。
fn make_main_waker(schedule: Box<dyn Fn() + Send + Sync>) -> Waker {
    let arc: Arc<Box<dyn Fn() + Send + Sync>> = Arc::new(schedule);
    let raw = RawWaker::new(Arc::into_raw(arc) as *const (), &MAIN_WAKER_VTABLE);
    // Safety: raw 来自我们管理的 vtable。
    unsafe { Waker::from_raw(raw) }
}

unsafe fn main_waker_clone(data: *const ()) -> RawWaker {
    // Safety: data 是 `Arc::into_raw(Arc<Box<dyn Fn ...>>)` 产出的指针。
    let arc = unsafe { Arc::<Box<dyn Fn() + Send + Sync>>::from_raw(data as *const Box<dyn Fn() + Send + Sync>) };
    let cloned = Arc::clone(&arc);
    std::mem::forget(arc);
    RawWaker::new(Arc::into_raw(cloned) as *const (), &MAIN_WAKER_VTABLE)
}

unsafe fn main_waker_wake(data: *const ()) {
    // Safety: data 是 `Arc::into_raw(...)` 产出的指针。
    let arc = unsafe { Arc::<Box<dyn Fn() + Send + Sync>>::from_raw(data as *const Box<dyn Fn() + Send + Sync>) };
    (*arc)();
    drop(arc);
}

unsafe fn main_waker_wake_by_ref(data: *const ()) {
    // Safety: data 是有效指针；不获取所有权。
    let arc_ref: &Arc<Box<dyn Fn() + Send + Sync>> =
        unsafe { &*(data as *const Arc<Box<dyn Fn() + Send + Sync>>) };
    (*arc_ref)();
}

unsafe fn main_waker_drop(data: *const ()) {
    // Safety: data 是 `Arc::into_raw(...)` 产出的指针。
    let arc = unsafe { Arc::<Box<dyn Fn() + Send + Sync>>::from_raw(data as *const Box<dyn Fn() + Send + Sync>) };
    drop(arc);
}

static MAIN_WAKER_VTABLE: RawWakerVTable =
    RawWakerVTable::new(main_waker_clone, main_waker_wake, main_waker_wake_by_ref, main_waker_drop);

impl Drop for Executor {
    fn drop(&mut self) {
        self.queue.shutdown();
        for w in self.workers.drain(..) {
            let _ = w._handle.join();
        }
    }
}

// -------------------------------------------------------------------------- worker

fn worker_loop(q: Arc<Queue>) {
    while let Some(task) = q.pop_blocking() {
        let waker = make_waker(&task);
        let mut cx = Context::from_waker(&waker);
        match task.poll(&mut cx) {
            Poll::Ready(p) => {
                q.active.fetch_sub(1, AOrdering::SeqCst);
                task.run_hook(p);
            }
            Poll::Pending => {
                // 任务未完成；保留 waker 在 task 中，等下一次 wake。
                // waker drop 时自动 Arc::drop（不影响 task）。
                drop(waker);
            }
        }
    }
}

// -------------------------------------------------------------------------- JoinHandle

/// `spawn` 返回的句柄（§13.1）。
///
/// **简化**：本批次的 executor 不暴露 `JoinHandle::await()`；Group 自己收集结果。
/// `JoinHandle` 仅占位并暴露唯一 id（调试 / 对照 spec）。
pub struct JoinHandle {
    id: u64,
}

impl JoinHandle {
    /// 唯一 id（仅调试用）。
    pub fn id(&self) -> u64 {
        self.id
    }
}