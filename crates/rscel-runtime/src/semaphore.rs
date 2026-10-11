//! 计数信号量（§6.2 / D4 配套）。
//!
//! 用途：`jump and T over coll concurrent_limit=N` 的惰性 spawn 限流（§12.5）。
//!
//! **实现**：`std::sync::Mutex<State>` + Waker 队列（Rust 1.56+ 的 `Notify` 风格手动实现，
//! 零外部依赖）。这是 `tokio::sync::Semaphore` 的简化版。
//!
//! **正确性**：每次 `acquire` 必须严格地**对一次**释放；permit drop 时归还并唤醒下一个
//! 等待者。100 个并发 acquire 场景：最多 N 个 permit 同时存活。

use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

/// 计数信号量（共享，内部状态通过 `Arc` 在 permit 间共享）。
#[derive(Clone)]
pub struct Semaphore {
    inner: Arc<Inner>,
}

struct Inner {
    state: Mutex<State>,
}

struct State {
    permits: usize,
    waiters: VecDeque<Waker>,
}

impl Semaphore {
    /// 构造 N 个 permit 的信号量。
    pub fn new(n: usize) -> Self {
        Semaphore { inner: Arc::new(Inner { state: Mutex::new(State { permits: n, waiters: VecDeque::new() }) }) }
    }

    /// 异步获取一个 permit（借用型）。
    pub async fn acquire(&self) -> SemaphorePermit<'_> {
        AcquireFuture { inner: self.inner.clone() }.await;
        SemaphorePermit { inner: self.inner.clone(), _marker: std::marker::PhantomData }
    }

    /// 异步获取 owned permit（适合跨 spawn）。
    pub async fn acquire_owned(self: Arc<Self>) -> OwnedSemaphorePermit {
        AcquireFuture { inner: Arc::clone(&self.inner) }.await;
        OwnedSemaphorePermit { inner: Arc::clone(&self.inner) }
    }

    /// 非阻塞尝试。返回 `Some(permit)` 或 `None`。
    pub fn try_acquire(&self) -> Option<SemaphorePermit<'_>> {
        let mut s = self.inner.state.lock().unwrap();
        if s.permits > 0 {
            s.permits -= 1;
            Some(SemaphorePermit { inner: self.inner.clone(), _marker: std::marker::PhantomData })
        } else {
            None
        }
    }

    /// 非阻塞尝试 owned 版。
    pub fn try_acquire_owned(self: Arc<Self>) -> Option<OwnedSemaphorePermit> {
        let mut s = self.inner.state.lock().unwrap();
        if s.permits > 0 {
            s.permits -= 1;
            Some(OwnedSemaphorePermit { inner: Arc::clone(&self.inner) })
        } else {
            None
        }
    }

    /// 当前可用 permit 数（实时，仅供观测/测试）。
    pub fn available_permits(&self) -> usize {
        self.inner.state.lock().unwrap().permits
    }

    /// 直接释放一个 permit（不经过 permit drop）。供 panic 隔离 / 测试使用。
    pub fn release_one(&self) {
        let mut s = self.inner.state.lock().unwrap();
        s.permits += 1;
        let waker = s.waiters.pop_front();
        drop(s);
        if let Some(w) = waker {
            w.wake();
        }
    }
}

/// 借用型 permit（生命周期受 `Semaphore` 引用约束）。
pub struct SemaphorePermit<'a> {
    inner: Arc<Inner>,
    // 仅为类型系统持有生命周期；不直接读。
    _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> Drop for SemaphorePermit<'a> {
    fn drop(&mut self) {
        release(self.inner.clone());
    }
}

impl<'a> std::fmt::Debug for SemaphorePermit<'a> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SemaphorePermit").finish()
    }
}

/// 拥有型 permit（`Send + 'static`，适合 spawn 进 future）。
pub struct OwnedSemaphorePermit {
    inner: Arc<Inner>,
}

impl Drop for OwnedSemaphorePermit {
    fn drop(&mut self) {
        release(self.inner.clone());
    }
}

impl std::fmt::Debug for OwnedSemaphorePermit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OwnedSemaphorePermit").finish()
    }
}

fn release(inner: Arc<Inner>) {
    let mut s = inner.state.lock().unwrap();
    s.permits += 1;
    let waker = s.waiters.pop_front();
    drop(s);
    if let Some(w) = waker {
        w.wake();
    }
}

/// acquire 异步 future：第一次 poll 立刻尝试；之后依赖 waker 被 release 唤醒。
struct AcquireFuture {
    inner: Arc<Inner>,
}

impl Future for AcquireFuture {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        // 快速路径：尝试获取一个 permit
        {
            let mut s = self.inner.state.lock().unwrap();
            if s.permits > 0 {
                s.permits -= 1;
                return Poll::Ready(());
            }
            // 注册到等待队列（覆盖之前的注册，确保使用最新的 waker）
            let new_waker = cx.waker().clone();
            s.waiters.retain(|w| !w.will_wake(&new_waker));
            s.waiters.push_back(new_waker);
            Poll::Pending
        }
    }
}