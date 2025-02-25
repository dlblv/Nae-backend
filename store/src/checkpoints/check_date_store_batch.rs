use std::sync::Arc;

use crate::balance::Balance;
use crate::batch::{max_batch, min_batch, Batch};
use crate::checkpoints::CheckpointTopology;
use crate::operations::Op;
use crate::{
  balance::BalanceForGoods,
  elements::{dt, first_day_current_month, Goods, Store, UUID_MAX, UUID_NIL},
  error::WHError,
};
use chrono::{DateTime, Utc};
use rocksdb::{BoundColumnFamily, IteratorMode, ReadOptions, DB};
use service::utils::time::timestamp_to_time;
use std::collections::HashMap;
use std::convert::TryFrom;
use uuid::Uuid;

const CF_NAME: &str = "cf_checkpoint_date_store_batch";

pub struct CheckDateStoreBatch {
  pub db: Arc<DB>,
}

impl CheckDateStoreBatch {
  pub fn cf_name() -> &'static str {
    CF_NAME
  }

  fn cf(&self) -> Result<Arc<BoundColumnFamily>, WHError> {
    if let Some(cf) = self.db.cf_handle(CheckDateStoreBatch::cf_name()) {
      Ok(cf)
    } else {
      Err(WHError::new("can't get CF"))
    }
  }

  pub fn key_to_data(k: Vec<u8>) -> Result<(DateTime<Utc>, Store, Goods, Batch), WHError> {
    // u64 8 bytes
    // Uuid 16 bytes

    let ts = u64::from_be_bytes(k[0..=7].try_into().unwrap());
    let date = timestamp_to_time(ts)?;

    let store = Uuid::from_slice(&k[8..=23])?;
    let goods = Uuid::from_slice(&k[24..=39])?;

    let batch_id = Uuid::from_slice(&k[48..=63])?;
    let ts = u64::from_be_bytes(k[40..=47].try_into().unwrap());
    let batch = Batch { id: batch_id, date: timestamp_to_time(ts)? };

    Ok((date, store, goods, batch))
  }
}

impl CheckpointTopology for CheckDateStoreBatch {
  fn key(&self, store: Store, goods: Goods, batch: Batch, date: DateTime<Utc>) -> Vec<u8> {
    (date.timestamp() as u64)
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(batch.to_bytes(&goods).iter())
      .map(|b| *b)
      .collect()
  }

  fn get_balance(&self, key: &Vec<u8>) -> Result<BalanceForGoods, WHError> {
    match self.db.get_cf(&self.cf()?, key)? {
      Some(v) => Ok(serde_json::from_slice(&v)?),
      None => Ok(BalanceForGoods::default()),
    }
  }

  fn set_balance(&self, key: &Vec<u8>, balance: BalanceForGoods) -> Result<(), WHError> {
    self
      .db
      .put_cf(&self.cf()?, key, serde_json::to_string(&balance)?)
      .map_err(|_| WHError::new("Can't put to database"))
  }

  fn del_balance(&self, key: &Vec<u8>) -> Result<(), WHError> {
    self.db.delete_cf(&self.cf()?, key)?;
    Ok(())
  }

  fn key_latest_checkpoint_date(&self) -> Vec<u8> {
    [].iter()
      .chain(u64::MIN.to_be_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .chain(u64::MIN.to_be_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .map(|b| *b)
      .collect()
  }

  fn get_latest_checkpoint_date(&self) -> Result<DateTime<Utc>, WHError> {
    if let Some(bytes) = self.db.get_cf(&self.cf()?, self.key_latest_checkpoint_date())? {
      let date = serde_json::from_slice(&bytes)?;
      Ok(DateTime::parse_from_rfc3339(date)?.into()) // TODO store/read timestamp in binary format
    } else {
      dt("1970-01-01")
    }
  }

  fn set_latest_checkpoint_date(&self, date: DateTime<Utc>) -> Result<(), WHError> {
    Ok(self.db.put_cf(
      &self.cf()?,
      self.key_latest_checkpoint_date(),
      serde_json::to_string(&date)?,
    )?)
  }

  fn get_checkpoints_for_one_goods(
    &self,
    store: Store,
    goods: Goods,
    date: DateTime<Utc>,
  ) -> Result<Vec<Balance>, WHError> {
    let mut balances = Vec::new();

    let current_date = first_day_current_month(date);

    let latest_checkpoint_date = self.get_latest_checkpoint_date()?;

    let actual_date =
      if current_date > latest_checkpoint_date { latest_checkpoint_date } else { current_date };

    let ts = u64::try_from(actual_date.timestamp()).unwrap_or_default();

    let from: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(goods.as_bytes().iter())
      .chain(u64::MIN.to_be_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .map(|b| *b)
      .collect();
    let till: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(goods.as_bytes().iter())
      .chain(u64::MAX.to_be_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .map(|b| *b)
      .collect();

    let mut opts = ReadOptions::default();
    opts.set_iterate_range(from..till);

    let mut iter = self.db.iterator_cf_opt(&self.cf()?, opts, IteratorMode::Start);

    while let Some(res) = iter.next() {
      let (k, v) = res?;
      let b: BalanceForGoods = serde_json::from_slice(&v)?;
      // println!("BAL: {b:#?}");
      let (date, store, goods, batch) = CheckDateStoreBatch::key_to_data(k.to_vec())?;

      let balance = Balance { date, store, goods, batch, number: b };
      balances.push(balance);
    }

    Ok(balances)
  }

  fn get_checkpoint_for_goods_and_batch(
    &self,
    store: Store,
    goods: Goods,
    batch: &Batch,
    date: DateTime<Utc>,
  ) -> Result<Option<Balance>, WHError> {
    let current_date = first_day_current_month(date);

    let latest_checkpoint_date = self.get_latest_checkpoint_date()?;

    let ts = if current_date > latest_checkpoint_date {
      u64::try_from(latest_checkpoint_date.timestamp()).unwrap_or_default()
    } else {
      u64::try_from(current_date.timestamp()).unwrap_or_default()
    };

    let ts_batch = u64::try_from(batch.date.timestamp()).unwrap_or_default();

    let key: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(goods.as_bytes().iter())
      .chain(ts_batch.to_be_bytes().iter())
      .chain(batch.id.as_bytes().iter())
      .map(|b| *b)
      .collect();

    if let Some(v) = self.db.get(key)? {
      let b: BalanceForGoods = serde_json::from_slice(&v)?;

      Ok(Some(Balance { date, store, goods, batch: batch.clone(), number: b }))
    } else {
      Ok(None)
    }
  }

  fn get_checkpoints_for_one_goods_with_date(
    &self,
    store: Store,
    goods: Goods,
    date: DateTime<Utc>,
  ) -> Result<(DateTime<Utc>, HashMap<Uuid, BalanceForGoods>), WHError> {
    let mut balances: HashMap<Uuid, BalanceForGoods> = HashMap::new();
    balances.insert(goods, BalanceForGoods::default());

    let current_date = first_day_current_month(date);

    let latest_checkpoint_date = self.get_latest_checkpoint_date()?;

    let actual_date =
      if current_date > latest_checkpoint_date { latest_checkpoint_date } else { current_date };

    let ts = u64::try_from(actual_date.timestamp()).unwrap_or_default();

    let from: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .chain(u64::MIN.to_be_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .map(|b| *b)
      .collect();
    let till: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .chain(u64::MAX.to_be_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .map(|b| *b)
      .collect();

    let mut opts = ReadOptions::default();
    opts.set_iterate_range(from..till);

    let mut iter = self.db.iterator_cf_opt(&self.cf()?, opts, IteratorMode::Start);

    while let Some(res) = iter.next() {
      let (k, v) = res?;
      let b: BalanceForGoods = serde_json::from_slice(&v)?;

      let (_, _, g, _) = CheckDateStoreBatch::key_to_data(k.to_vec())?;

      balances.entry(g).and_modify(|bal| *bal += b);
    }

    Ok((actual_date, balances))
  }

  fn balances_for_store_goods(
    &self,
    date: DateTime<Utc>,
    store: Store,
    goods: Goods,
  ) -> Result<(DateTime<Utc>, HashMap<Batch, BalanceForGoods>), WHError> {
    let current_date = first_day_current_month(date);

    let latest_checkpoint_date = self.get_latest_checkpoint_date()?;

    let actual_date =
      if current_date > latest_checkpoint_date { latest_checkpoint_date } else { current_date };

    let ts = u64::try_from(actual_date.timestamp()).unwrap_or_default();

    let from: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .chain(u64::MIN.to_be_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .map(|b| *b)
      .collect();
    let till: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .chain(u64::MAX.to_be_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .map(|b| *b)
      .collect();

    let mut opts = ReadOptions::default();
    opts.set_iterate_range(from..till);

    let mut iter = self.db.iterator_cf_opt(&self.cf()?, opts, IteratorMode::Start);

    let mut balances: HashMap<Batch, BalanceForGoods> = HashMap::new();
    while let Some(res) = iter.next() {
      let (k, v) = res?;
      let balance: BalanceForGoods = serde_json::from_slice(&v)?;

      let (_, s, g, b) = CheckDateStoreBatch::key_to_data(k.to_vec())?;

      if s == store && g == goods {
        balances.insert(b, balance);
      }
    }

    Ok((actual_date, balances))
  }

  fn get_checkpoints_for_many_goods(
    &self,
    date: DateTime<Utc>,
    goods: &Vec<Goods>,
  ) -> Result<(DateTime<Utc>, HashMap<Uuid, BalanceForGoods>), WHError> {
    let mut balances: HashMap<Uuid, BalanceForGoods> =
      goods.into_iter().map(|key| (key.clone(), BalanceForGoods::default())).collect();

    let current_date = first_day_current_month(date);

    let latest_checkpoint_date = self.get_latest_checkpoint_date()?;

    let actual_date =
      if current_date > latest_checkpoint_date { latest_checkpoint_date } else { current_date };

    let ts = u64::try_from(actual_date.timestamp()).unwrap_or_default();

    let from: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(UUID_NIL.as_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .chain(u64::MIN.to_be_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .map(|b| *b)
      .collect();
    let till: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(UUID_MAX.as_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .chain(u64::MAX.to_be_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .map(|b| *b)
      .collect();

    let mut opts = ReadOptions::default();
    opts.set_iterate_range(from..till);

    let mut iter = self.db.iterator_cf_opt(&self.cf()?, opts, IteratorMode::Start);

    while let Some(res) = iter.next() {
      let (k, v) = res?;
      let b: BalanceForGoods = serde_json::from_slice(&v)?;

      let (_, _, g, _) = CheckDateStoreBatch::key_to_data(k.to_vec())?;

      balances.entry(g).and_modify(|bal| *bal += b);
    }

    Ok((actual_date, balances))
  }

  fn get_checkpoints_for_all(
    &self,
    date: DateTime<Utc>,
  ) -> Result<
    (DateTime<Utc>, HashMap<Store, HashMap<Goods, HashMap<Batch, BalanceForGoods>>>),
    WHError,
  > {
    let start_of_current_month_date = first_day_current_month(date);

    let latest_checkpoint_date = self.get_latest_checkpoint_date()?;

    let checkpoint_date = if start_of_current_month_date > latest_checkpoint_date {
      latest_checkpoint_date
    } else {
      start_of_current_month_date
    };

    let ts = u64::try_from(checkpoint_date.timestamp()).unwrap_or_default();

    let from: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(UUID_NIL.as_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .chain(u64::MIN.to_be_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .map(|b| *b)
      .collect();
    let till: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(UUID_MAX.as_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .chain(u64::MAX.to_be_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .map(|b| *b)
      .collect();

    let mut opts = ReadOptions::default();
    opts.set_iterate_range(from..till);

    let mut result = HashMap::with_capacity(10_000);

    let mut iter = self.db.iterator_cf_opt(&self.cf()?, opts, IteratorMode::Start);
    while let Some(res) = iter.next() {
      let (k, v) = res?;
      let stock: BalanceForGoods = serde_json::from_slice(&v)?;

      let (_, store, goods, batch) = CheckDateStoreBatch::key_to_data(k.to_vec())?;

      result
        .entry(store)
        .or_insert_with(|| HashMap::new())
        .entry(goods)
        .or_insert_with(|| HashMap::new())
        .insert(batch, stock);
    }

    Ok((checkpoint_date, result))
  }

  fn get_checkpoints_for_one_storage_before_date(
    &self,
    store: Store,
    date: DateTime<Utc>,
  ) -> Result<Vec<Balance>, WHError> {
    let mut balances = Vec::new();

    let current_date = first_day_current_month(date);

    let latest_checkpoint_date = self.get_latest_checkpoint_date()?;

    let ts = if current_date > latest_checkpoint_date {
      u64::try_from(latest_checkpoint_date.timestamp()).unwrap_or_default()
    } else {
      u64::try_from(current_date.timestamp()).unwrap_or_default()
    };

    let from: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(min_batch().iter())
      .map(|b| *b)
      .collect();
    let till: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(store.as_bytes().iter())
      .chain(max_batch().iter())
      .map(|b| *b)
      .collect();

    let mut opts = ReadOptions::default();
    opts.set_iterate_range(from..till);

    let mut iter = self.db.iterator_cf_opt(&self.cf()?, opts, IteratorMode::Start);

    while let Some(res) = iter.next() {
      let (k, v) = res?;
      let b: BalanceForGoods = serde_json::from_slice(&v)?;
      // println!("BAL: {b:#?}");
      let (date, store, goods, batch) = CheckDateStoreBatch::key_to_data(k.to_vec())?;

      let balance = Balance { date, store, goods, batch, number: b };
      balances.push(balance);
    }

    Ok(balances)
  }

  fn get_checkpoints_for_all_storages_before_date(
    &self,
    date: DateTime<Utc>,
  ) -> Result<Vec<Balance>, WHError> {
    let mut balances = Vec::new();

    let current_date = first_day_current_month(date);

    let latest_checkpoint_date = self.get_latest_checkpoint_date()?;

    let ts = if current_date > latest_checkpoint_date {
      u64::try_from(latest_checkpoint_date.timestamp()).unwrap_or_default()
    } else {
      u64::try_from(current_date.timestamp()).unwrap_or_default()
    };

    let from: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(UUID_NIL.as_bytes().iter())
      .chain(min_batch().iter())
      .map(|b| *b)
      .collect();

    let till: Vec<u8> = ts
      .to_be_bytes()
      .iter()
      .chain(UUID_MAX.as_bytes().iter())
      .chain(max_batch().iter())
      .map(|b| *b)
      .collect();

    let mut opts = ReadOptions::default();
    opts.set_iterate_range(from..till);

    let mut iter = self.db.iterator_cf_opt(&self.cf()?, opts, IteratorMode::Start);

    while let Some(res) = iter.next() {
      let (k, v) = res?;
      let b: BalanceForGoods = serde_json::from_slice(&v)?;
      // println!("BAL: {b:#?}");
      let (date, store, goods, batch) = CheckDateStoreBatch::key_to_data(k.to_vec())?;

      let balance = Balance { date, store, goods, batch, number: b };
      balances.push(balance);
    }

    Ok(balances)
  }
}
