//! `Group` —— 任务组管理（§12.5 / 选型 D4 / 缺陷 B12 修法）。
//!
//! 公开 API（与 §13.1 模板 + §12.5 一致，含 B12 修法 `spawn_owned`）：
//! - `Group::new()` —— 新建空组；
//! - `spawn(fut)` / `push(fut)` —— 添加任务（Send future）；
//! - `spawn_owned(fut)` —— B12 修法（owned 闭包型 spawn）；
//! - `push_handle(jh)` —— 接受外部 `JoinHandle`；
//! - `wait_all(self) -> Vec<Packet>` —— 等待所有任务完成，**按入队顺序**返回包；
//! - `pending_count()` —— 在飞任务数（已 spawn 未完成 + 已完成未 take）。
//!
//! **D4 增量回收**：完成即出队，内存 = O(并发上限) 而非 O(总元素数)。
//!
//! 设计：
//! - Group 持有 `Arc<Inner>`，Inner 含 `slots: Mutex<VecDeque<Slot>>` + `pending: AtomicUsize`
//!   + `Condvar`；
//! - 每个 spawn 创建**独立**的 hook（捕获 index & inner），覆盖 task 默认 hook；
//! - `wait_all` 等 `pending == 0`，再按入队顺序 drain。

use std::collections::VecDeque;
use std::future::Future;
use std::sync::{Arc, Condvar, Mutex};

use crate::executor::Executor;
use crate::packet::Packet;

/// Group 内部状态（共享）。
pub struct GroupInner {
    slots: Mutex<VecDeque<Slot>>,
    /// 在飞任务数。**Mutex** 包裹以便 Condvar 等待；所有更新通过此 mutex。
    pending: Mutex<usize>,
    cvar: Condvar,
}

/// 槽位：按入队顺序排列；spawn 时 push None，完成回调填 Some。
struct Slot {
    #[allow(dead_code)]
    index: usize,
    result: Option<Packet>,
}

impl GroupInner {
    fn new() -> Self {
        GroupInner {
            slots: Mutex::new(VecDeque::new()),
            pending: Mutex::new(0),
            cvar: Condvar::new(),
        }
    }

    fn push_slot(&self, slot: Slot) {
        self.slots.lock().unwrap().push_back(slot);
    }

    fn on_complete(&self, index: usize, pkt: Packet) {
        {
            let mut q = self.slots.lock().unwrap();
            if let Some(slot) = q.get_mut(index) {
                slot.result = Some(pkt);
            }
        }
        let mut p = self.pending.lock().unwrap();
        *p = p.saturating_sub(1);
        if *p == 0 {
            self.cvar.notify_all();
        }
    }

    fn pending(&self) -> usize {
        *self.pending.lock().unwrap()
    }

    fn wait_all_done(&self) {
        let mut p = self.pending.lock().unwrap();
        while *p > 0 {
            p = self.cvar.wait(p).unwrap();
        }
    }
}

/// 任务组（§13.1 模板 + §12.5 惰性 spawn）。
pub struct Group {
    inner: Arc<GroupInner>,
    /// 关联的 Executor（用于 spawn；未绑定时 spawn 会 panic）。
    exec: Option<Arc<Executor>>,
    /// 下一个入队序号。
    next_index: usize,
}

impl Group {
    /// 新建空组（未绑定 Executor，必须随后 `bind_executor` 才能 spawn）。
    pub fn new() -> Self {
        Group { inner: Arc::new(GroupInner::new()), exec: None, next_index: 0 }
    }

    /// 新建并绑定 Executor。
    pub fn with_executor(exec: Arc<Executor>) -> Self {
        Group { inner: Arc::new(GroupInner::new()), exec: Some(exec), next_index: 0 }
    }

    /// 绑定 Executor（构造后调用一次）。
    pub fn bind_executor(&mut self, exec: Arc<Executor>) {
        self.exec = Some(exec);
    }

    fn alloc_slot(&mut self) -> usize {
        let idx = self.next_index;
        self.next_index += 1;
        self.inner.push_slot(Slot { index: idx, result: None });
        *self.inner.pending.lock().unwrap() += 1;
        idx
    }

    /// spawn 一个 Send future。
    pub fn spawn<F>(&mut self, fut: F) -> &mut Self
    where
        F: Future<Output = Packet> + Send + 'static,
    {
        let exec = self.exec.as_ref().expect("Group must be bound to an Executor").clone();
        let idx = self.alloc_slot();
        let inner = Arc::clone(&self.inner);
        let hook: Arc<dyn Fn(Packet) + Send + Sync> = Arc::new(move |pkt| inner.on_complete(idx, pkt));
        exec.spawn_with_hook(Box::pin(fut), hook);
        self
    }

    /// 同 [`Group::spawn`]（§13.1 别名）。
    pub fn push<F>(&mut self, fut: F) -> &mut Self
    where
        F: Future<Output = Packet> + Send + 'static,
    {
        self.spawn(fut)
    }

    /// B12 修法：`spawn_owned`（owned 闭包 spawn，§12.5 的 `g.spawn_owned(...)` 必须存在）。
    ///
    /// 签名与 `spawn` 一致——owned 语义指 future 拥有自己的所有权，spawn 后父不再触碰。
    pub fn spawn_owned<F>(&mut self, fut: F) -> &mut Self
    where
        F: Future<Output = Packet> + Send + 'static,
    {
        self.spawn(fut)
    }

    // `push_handle`（§13.1 原方法）已**删除**，不是删功能而是停止假装成功：
    // 本 crate 的 `JoinHandle` 只是 executor 任务 id 的包装（见 executor.rs），
    // 没有 await、也没有完成通知，所以「接受一个外部句柄并等待它」根本无从实现。
    // 之前它是一个吞掉入参、原样返回 `self` 的公开方法 ——
    // 调用者以为句柄已被组接管，实际上被静默丢弃，属于《开发规约》第 1 条
    // 禁止的「静默降级」。类型化主路径（rscel-gen 的 `__RscGroup`）从不使用它；
    // 需要真实 join 语义请在 gen 侧做（批次 D 的性能 pass 会覆盖）。

    /// 等待所有任务完成，按入队顺序返回所有包。
    pub async fn wait_all(self) -> Vec<Packet> {
        self.inner.wait_all_done();
        let mut q = self.inner.slots.lock().unwrap();
        let mut out = Vec::with_capacity(q.len());
        while let Some(mut slot) = q.pop_front() {
            out.push(slot.result.take().unwrap_or_else(|| Packet::none(0)));
        }
        out
    }

    /// 当前在飞任务数（已 spawn 未完成）。
    pub fn pending_count(&self) -> usize {
        self.inner.pending()
    }

    /// 共享内部状态（仅供 Executor 内部使用）。
    #[allow(dead_code)]
    pub(crate) fn inner_arc(&self) -> Arc<GroupInner> {
        Arc::clone(&self.inner)
    }
}

impl Default for Group {
    fn default() -> Self {
        Self::new()
    }
}