//! Packet 测试矩阵（§13.5 / 任务书 §2.5）。

use rscel_runtime::{KeyIdx, Packet, VarKind};

#[test]
fn new_has_correct_slot_count_and_no_died() {
    let p = Packet::new(8);
    assert_eq!(p.total_slots(), 8);
    assert!(!p.is_died());
    assert_eq!(p.filled_slots(), 0);
}

#[test]
fn set_take_i64_roundtrip() {
    let p = Packet::new(2).set(0, 42_i64).set(1, 7_i64);
    assert_eq!(p.kind_at(0), VarKind::Pod);
    let mut p = p;
    let v = p.take(0).unwrap();
    assert_eq!(v.kind(), VarKind::Pod);
}

#[test]
fn set_take_f64_and_bool() {
    let mut p = Packet::new(3).set(0, 1.5_f64).set(1, true).set(2, false);
    assert_eq!(p.kind_at(0), VarKind::Pod);
    assert_eq!(p.kind_at(1), VarKind::Pod);
    assert!(p.contains(1));
    p.take(0).unwrap();
    p.take(1).unwrap();
    assert!(p.take(2).is_some());
}

#[test]
fn none_marks_died() {
    let p = Packet::none(4);
    assert!(p.is_died());
    assert_eq!(p.total_slots(), 4);
}

#[test]
fn take_returns_none_for_unset_slot() {
    let mut p = Packet::new(4);
    assert!(p.take(2).is_none());
    assert!(!p.contains(2));
}

#[test]
fn box_handle_via_set_ref() {
    use std::sync::Arc;
    let data: Arc<Vec<u8>> = Arc::new(vec![1u8, 2, 3, 4]);
    let p = Packet::new(1).set_ref(0, data.clone());
    assert_eq!(p.kind_at(0), VarKind::SharedArc);
    let mut p = p;
    let h = p.take(0).unwrap();
    assert!(h.is_shared());
}

#[test]
fn set_then_take_clears_slot() {
    let mut p = Packet::new(1).set(0, 99_u64);
    let _ = p.take(0).unwrap();
    assert!(p.take(0).is_none());
    assert_eq!(p.filled_slots(), 0);
}

#[test]
fn set_with_growing_index_extends_slots() {
    // set 索引超过 total_slots 时自动扩容（B1 修法同款策略）
    let p = Packet::new(2).set(5, 42_i64);
    assert_eq!(p.total_slots(), 6);
    assert!(p.contains(5));
}

#[test]
fn multiple_keys_distinct_slots() {
    let mut p = Packet::new(8)
        .set(0, 1_i64)
        .set(3, Box::new("hello".to_string()))
        .set(7, 2.5_f64);
    assert_eq!(p.kind_at(0), VarKind::Pod);
    assert_eq!(p.kind_at(3), VarKind::OwnedBox);
    assert_eq!(p.kind_at(7), VarKind::Pod);
    assert_eq!(p.filled_slots(), 3);
    p.take(0).unwrap();
    p.take(3).unwrap();
    p.take(7).unwrap();
    assert_eq!(p.filled_slots(), 0);
}

#[test]
fn take_ref_returns_arc_dyn_any() {
    use std::sync::Arc;
    let data: Arc<String> = Arc::new("world".into());
    let p = Packet::new(2).set_ref(0, data.clone());
    let any_ref = p.take_ref(0).unwrap();
    let s = any_ref.downcast_ref::<String>().unwrap();
    assert_eq!(s, "world");
}

#[test]
fn take_ref_on_pod_returns_u64_ref() {
    let p = Packet::new(1).set(0, 0xDEAD_BEEF_u64);
    let any_ref = p.take_ref(0).unwrap();
    let v = any_ref.downcast_ref::<u64>().unwrap();
    assert_eq!(*v, 0xDEAD_BEEF_u64);
}

#[test]
fn mark_died_after_creation() {
    let mut p = Packet::new(4).set(0, 1_i64);
    assert!(!p.is_died());
    p.mark_died();
    assert!(p.is_died());
}

#[test]
fn b1_repro_vec_with_capacity_not_vec_macro() {
    // B1 修法实测：构造 N 槽位 Packet 编译通过且 N = 4 时 length 正确。
    let n: usize = 1_000_000;
    let p = Packet::new(n);
    assert_eq!(p.total_slots(), n);
    assert!(!p.is_died());
}

#[test]
fn keyidx_type_alias() {
    // 确保 KeyIdx 类型别名可用
    let _: KeyIdx = 7u16;
}