use std::sync::Arc;

use super::{
  CheckpointTopology, Db, KeyValueStore, Op, OpMutation, OrderedTopology, Store, WHError, UUID_MAX,
  UUID_NIL,
};
use crate::store::{first_day_current_month, new_get_aggregations, Balance, Report};
use chrono::{DateTime, Utc};
use rocksdb::{BoundColumnFamily, ColumnFamilyDescriptor, IteratorMode, Options, ReadOptions, DB};

const CF_NAME: &str = "cf_date_type_store_batch_id";

pub struct DateTypeStoreGoodsId {
  pub db: Arc<DB>,
}

impl DateTypeStoreGoodsId {
  pub fn cf_name() -> &'static str {
    CF_NAME
  }

  fn cf(&self) -> Result<Arc<BoundColumnFamily>, WHError> {
    if let Some(cf) = self.db.cf_handle(DateTypeStoreGoodsId::cf_name()) {
      Ok(cf)
    } else {
      Err(WHError::new("can't get CF"))
    }
  }
}

impl OrderedTopology for DateTypeStoreGoodsId {
  fn get_ops(
    &self,
    storage: Store,
    from_date: DateTime<Utc>,
    till_date: DateTime<Utc>,
  ) -> Result<Vec<Op>, WHError> {
    let from_date = from_date.timestamp() as u64;
    let from: Vec<u8> = from_date
      .to_be_bytes()
      .iter()
      .chain(0_u8.to_be_bytes().iter())
      .chain(UUID_NIL.as_bytes().iter())
      .map(|b| *b)
      .collect();

    let till_date = till_date.timestamp() as u64;
    let till: Vec<u8> = till_date
      .to_be_bytes()
      .iter()
      .chain(u8::MAX.to_be_bytes().iter())
      .chain(UUID_MAX.as_bytes().iter())
      .map(|b| *b)
      .collect();

    let mut options = ReadOptions::default();
    options.set_iterate_range(from..till);

    let mut res = Vec::new();

    for item in self.db.iterator_cf_opt(&self.cf()?, options, IteratorMode::Start) {
      let (_, value) = item?;
      let op = serde_json::from_slice(&value)?;
      res.push(op);
    }

    Ok(res)
  }

  fn put_op(&self, op: &OpMutation) -> Result<(), WHError> {
    Ok(self.db.put_cf(
      &self.cf()?,
      op.key(&DateTypeStoreGoodsId::cf_name().to_string())?,
      op.value()?,
    )?)
  }

  fn create_cf(&self, opts: Options) -> ColumnFamilyDescriptor {
    ColumnFamilyDescriptor::new(DateTypeStoreGoodsId::cf_name(), opts)
  }

  fn data_update(&self, op: &OpMutation) -> Result<(), WHError> {
    if op.before.is_none() {
      self.put_op(op)
    } else {
      if let Ok(Some(bytes)) = self
        .db
        .get_cf(&self.cf()?, op.key(&DateTypeStoreGoodsId::cf_name().to_string())?)
      {
        let o: OpMutation = serde_json::from_slice(&bytes)?;
        if op.before == o.after {
          self.put_op(op)
        } else {
          return Err(WHError::new("Wrong 'before' state in operation"));
        }
      } else {
        return Err(WHError::new("There is no such operation in db"));
      }
    }
  }

  fn get_report(
    &self,
    db: &Db,
    storage: Store,
    from_date: DateTime<Utc>,
    till_date: DateTime<Utc>,
  ) -> Result<Report, WHError> {
    let balances = db.get_checkpoints_before_date(storage, from_date)?;

    let ops = self.get_ops(storage, first_day_current_month(from_date), till_date)?;

    let items = new_get_aggregations(balances, ops, from_date);

    Ok(Report { from_date, till_date, items })
  }
}
