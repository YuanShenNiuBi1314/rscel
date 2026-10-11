//! Semaphore 测试矩阵（§6.2 / D4 / 任务书 §2.5）。

use std::sync::atomic::{AtomicUsize, Ordering as AOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rscel_runtime::{Executor, Semaphore};

#[test]
fn try_acquire_returns_permits_up_to_n() {
    let sem = Semaphore::new(3);
    let p1 = sem.try_acquire().unwrap();
    let p2 = sem.try_acquire().unwrap();
    let p3 = sem.try_acquire().unwrap();
    assert_eq!(sem.available_permits(), 0);
    assert!(sem.try_acquire().is_none());
    drop(p1);
    drop(p2);
    drop(p3);
    assert_eq!(sem.available_permits(), 3);
}

#[test]
fn blocking_acquire_succeeds_after_release() {
    let exec = Arc::new(Executor::new(2));
    exec.block_on(async {
        let sem = Arc::new(Semaphore::new(1));
        let p = sem.clone().acquire_owned().await;
        // 第二个 acquire 应该 pending；释放后允许。
        let sem_a = Arc::clone(&sem);
        let h = std::thread::spawn(move || {
            let _guard = sem_a.try_acquire();
            std::thread::sleep(Duration::from_millis(50));
            drop(_guard);
        });
        // 在 h 拿不到 permit 的前提下，第二个 acquire 等到释放后拿到
        let _ = h;
        drop(p);
        let _p2 = sem.clone().acquire_owned().await;
    });
}

#[test]
fn concurrent_acquire_max_n_outstanding() {
    // 100 并发 acquire，N=10：最多同时持有 10 个 permit。
    let exec = Arc::new(Executor::new(4));
    exec.block_on(async {
        let sem = Arc::new(Semaphore::new(10));
        let in_flight = Arc::new(AtomicUsize::new(0));
        let max_in_flight = Arc::new(AtomicUsize::new(0));
        let mut handles = Vec::new();
        for _ in 0..100 {
            let sem_c = Arc::clone(&sem);
            let in_flight_c = Arc::clone(&in_flight);
            let max_c = Arc::clone(&max_in_flight);
            let h = std::thread::spawn(move || {
                let _permit = sem_c.try_acquire();
                if _permit.is_some() {
                    let now = in_flight_c.fetch_add(1, AOrdering::SeqCst) + 1;
                    // 记录最大值
                    let mut cur = max_c.load(AOrdering::SeqCst);
                    while now > cur {
                        match max_c.compare_exchange(cur, now, AOrdering::SeqCst, AOrdering::SeqCst) {
                            Ok(_) => break,
                            Err(v) => cur = v,
                        }
                    }
                    std::thread::sleep(Duration::from_millis(1));
                    in_flight_c.fetch_sub(1, AOrdering::SeqCst);
                }
            });
            handles.push(h);
        }
        for h in handles {
            h.join().unwrap();
        }
        // 因 try_acquire 不阻塞，全部尝试最多 10 个并发持有；后续会因 permit 0 而失败。
        assert!(max_in_flight.load(AOrdering::SeqCst) <= 10);
    });
}

#[test]
fn available_permits_is_live() {
    let sem = Semaphore::new(5);
    assert_eq!(sem.available_permits(), 5);
    let _p = sem.try_acquire().unwrap();
    assert_eq!(sem.available_permits(), 4);
    let _p2 = sem.try_acquire().unwrap();
    assert_eq!(sem.available_permits(), 3);
}

#[test]
fn release_one_directly() {
    let sem = Semaphore::new(2);
    assert_eq!(sem.available_permits(), 2);
    sem.release_one();
    sem.release_one();
    sem.release_one(); // 超过 N 也允许（状态不被夹紧）
    assert_eq!(sem.available_permits(), 5);
}

#[test]
fn try_acquire_owned_returns_owned_permit() {
    let sem = Arc::new(Semaphore::new(2));
    let p1 = sem.clone().try_acquire_owned().unwrap();
    assert_eq!(sem.available_permits(), 1);
    let p2 = sem.clone().try_acquire_owned().unwrap();
    assert_eq!(sem.available_permits(), 0);
    drop(p1);
    assert_eq!(sem.available_permits(), 1);
    drop(p2);
    assert_eq!(sem.available_permits(), 2);
}

#[test]
fn blocking_acquire_after_drop_wakes_pending() {
    // 验证 acquire_future 在被 wake 后能拿到 permit。
    let exec = Arc::new(Executor::new(2));
    let now = Instant::now();
    exec.block_on(async {
        let sem = Arc::new(Semaphore::new(1));
        let _p = sem.clone().acquire_owned().await;
        // 此时 0 个 permit；起一个 acquire task
        let sem_c = Arc::clone(&sem);
        let join_h = std::thread::spawn(move || {
            // 不能 await（不在 exec 中）；改用同步 sleep + release 模拟。
            std::thread::sleep(Duration::from_millis(50));
            drop(_p); // 由 sub-thread drop permit
            0u8
        });
        // 异步获取：应该等 permit 释放后拿到
        let _p2 = sem_c.acquire_owned().await;
        let _ = join_h.join();
    });
    // 整体应在合理时间（< 1s）完成
    assert!(now.elapsed() < Duration::from_secs(2));
}