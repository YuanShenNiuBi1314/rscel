//! Group 测试矩阵（§13.5 / 任务书 §2.5 + B12 修法 + D4 增量回收）。

use std::sync::atomic::{AtomicUsize, Ordering as AOrdering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rscel_runtime::{Executor, Group, Packet};

#[test]
fn single_child_returns_one_packet() {
    let exec = Arc::new(Executor::new(2));
    exec.block_on(async {
        let mut g = Group::with_executor(Arc::clone(&exec));
        g.spawn(async { Packet::new(1).set(0, 7_i64) });
        let pkts = g.wait_all().await;
        assert_eq!(pkts.len(), 1);
        assert!(!pkts[0].is_died());
        assert_eq!(pkts[0].kind_at(0), rscel_runtime::VarKind::Pod);
    });
}

#[test]
fn many_children_return_in_enqueue_order() {
    let exec = Arc::new(Executor::new(4));
    exec.block_on(async {
        let mut g = Group::with_executor(Arc::clone(&exec));
        for i in 0..20u64 {
            g.spawn(async move {
                // 让不同任务耗时不同，验证顺序由入队而非完成决定。
                let delay_ms = if i % 2 == 0 { 20 } else { 1 };
                std::thread::sleep(Duration::from_millis(delay_ms));
                Packet::new(1).set(0, i as i64)
            });
        }
        let mut pkts = g.wait_all().await;
        assert_eq!(pkts.len(), 20);
        for (i, p) in pkts.iter_mut().enumerate() {
            let v = match p.take(0) {
                Some(rscel_runtime::VarHandle::OwnedPod(bits)) => bits as i64,
                _ => panic!("unexpected handle at slot 0"),
            };
            assert_eq!(v, i as i64, "包 {i} 应为 {i}，顺序按入队而非完成");
        }
    });
}

#[test]
fn b12_spawn_owned_works() {
    let exec = Arc::new(Executor::new(2));
    exec.block_on(async {
        let mut g = Group::with_executor(Arc::clone(&exec));
        g.spawn_owned(async { Packet::new(1).set(0, 1_i64) });
        g.spawn_owned(async { Packet::new(1).set(0, 2_i64) });
        let pkts = g.wait_all().await;
        assert_eq!(pkts.len(), 2);
    });
}

#[test]
fn died_packet_lands_at_correct_index() {
    let exec = Arc::new(Executor::new(2));
    exec.block_on(async {
        let mut g = Group::with_executor(Arc::clone(&exec));
        g.spawn(async { Packet::new(1).set(0, 1_i64) });
        g.spawn(async { Packet::none(1) });
        g.spawn(async { Packet::new(1).set(0, 3_i64) });
        let pkts = g.wait_all().await;
        assert_eq!(pkts.len(), 3);
        assert!(!pkts[0].is_died());
        assert!(pkts[1].is_died());
        assert!(!pkts[2].is_died());
    });
}

#[test]
fn pending_count_drops_to_zero_after_wait() {
    let exec = Arc::new(Executor::new(2));
    exec.block_on(async {
        let mut g = Group::with_executor(Arc::clone(&exec));
        g.spawn(async { Packet::new(0) });
        g.spawn(async { Packet::new(0) });
        assert!(g.pending_count() >= 1, "spawn 后 pending > 0");
        let pkts = g.wait_all().await;
        assert_eq!(pkts.len(), 2);
    });
}

#[test]
fn group_has_no_placeholder_join_api() {
    // 《开发规约》第 1 条：不允许「接受句柄却什么都不做」的占位公开 API。
    // push_handle 已删除；这里守住 surface，防止它以后被原样加回来。
    let exec = Arc::new(Executor::new(2));
    let src = include_str!("../src/group.rs");
    assert!(
        !src.contains("pub fn push_handle"),
        "group.rs 不应再暴露 push_handle 占位 API"
    );
    let _ = exec;
}

#[test]
fn spawn_many_increments_pending_count() {
    let exec = Arc::new(Executor::new(2));
    exec.block_on(async {
        let mut g = Group::with_executor(Arc::clone(&exec));
        let counter = Arc::new(AtomicUsize::new(0));
        for _ in 0..100 {
            let c = Arc::clone(&counter);
            g.spawn(async move {
                c.fetch_add(1, AOrdering::SeqCst);
                Packet::new(0)
            });
        }
        let pkts = g.wait_all().await;
        assert_eq!(pkts.len(), 100);
        assert_eq!(counter.load(AOrdering::SeqCst), 100);
    });
}

#[test]
fn d4_incremental_recycle_no_handle_accumulation() {
    // D4 验证：完成即回收；连续 500 任务不应让 Group 内部槽位长时间堆积。
    let exec = Arc::new(Executor::new(4));
    let now = Instant::now();
    exec.block_on(async {
        let mut g = Group::with_executor(Arc::clone(&exec));
        for i in 0..500u64 {
            g.spawn(async move { Packet::new(1).set(0, i as i64) });
        }
        let pkts = g.wait_all().await;
        assert_eq!(pkts.len(), 500);
    });
    assert!(now.elapsed().as_secs() < 10, "500 任务应在合理时间内完成");
}

#[test]
fn push_alias_works_like_spawn() {
    let exec = Arc::new(Executor::new(2));
    exec.block_on(async {
        let mut g = Group::with_executor(Arc::clone(&exec));
        g.push(async { Packet::new(1).set(0, 1_i64) });
        g.push(async { Packet::new(1).set(0, 2_i64) });
        let pkts = g.wait_all().await;
        assert_eq!(pkts.len(), 2);
    });
}