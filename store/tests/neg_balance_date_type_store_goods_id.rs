use uuid::Uuid;
use tempfile::TempDir;
use store::wh_storage::WHStorage;
use store::elements::{dt, Batch, OpMutation, InternalOperation, Mode, AgregationStoreGoods};
use store::balance::{BalanceForGoods, BalanceDelta};

const G1: Uuid = Uuid::from_u128(1);

#[test]
fn store_test_neg_balance_date_type_store_goods_id() {
    let tmp_dir = TempDir::new().expect("Can't create tmp dir in test_get_neg_balance");

    let wh = WHStorage::open(&tmp_dir.path()).unwrap();
    let mut db = wh.database;

    let op_d = dt("2022-10-10").expect("test_get_neg_balance");
    let check_d = dt("2022-10-11").expect("test_get_neg_balance");
    let w1 = Uuid::new_v4();
    let party = Batch { id: Uuid::new_v4(), date: op_d };

    let id1 = Uuid::from_u128(101);

    let ops = vec![OpMutation::new(
        id1,
        op_d,
        w1,
        None,
        G1,
        party.clone(),
        None,
        Some(InternalOperation::Issue(2.into(), 2000.into(), Mode::Manual)),
    )];

    db.record_ops(&ops).expect("test_get_neg_balance");

    let agr = AgregationStoreGoods {
        store: Some(w1),
        goods: Some(G1),
        batch: Some(party.clone()),
        open_balance: BalanceForGoods::default(),
        receive: BalanceDelta::default(),
        issue: BalanceDelta { qty: (-2).into(), cost: (-2000).into() },
        close_balance: BalanceForGoods { qty: (-2).into(), cost: (-2000).into() },
    };

    let res = db.get_report(w1, op_d, check_d).unwrap();
    assert_eq!(res.items.1[0], agr);

    tmp_dir.close().expect("Can't close tmp dir in test_get_neg_balance");
}