use std::fmt::Debug;
use std::marker::PhantomData;
use rocksdb::{AsColumnFamilyRef, DBIteratorWithThreadMode, DBWithThreadMode, Direction, IteratorMode, MultiThreaded, ReadOptions};
use crate::animo::{Txn, Object, Operation, AOperation, ObjectInTopology, OperationInTopology, AOperationInTopology, AObjectInTopology, AObject};
use crate::error::DBError;
use crate::rocksdb::{FromBytes, FromKVBytes, Snapshot};

pub struct OpsManager();

pub struct LightIterator<'a,O>(DBIteratorWithThreadMode<'a, DBWithThreadMode<MultiThreaded>>, PhantomData<O>);

impl<'a,O: FromBytes<O>> Iterator for LightIterator<'a,O> {
    type Item = (Vec<u8>, O);

    fn next(&mut self) -> Option<(Vec<u8>, O)> {
        match self.0.next() {
            None => None,
            Some((k, v)) => {
                // debug!("next {:?} {:?}", v, k);
                let record = O::from_bytes(&*v).unwrap();
                Some((k.to_vec(), record))
            }
        }
    }
}

pub struct HeavyIterator<'a,O>(DBIteratorWithThreadMode<'a, DBWithThreadMode<MultiThreaded>>, PhantomData<O>);

impl<'a,O: FromKVBytes<O>> Iterator for HeavyIterator<'a,O> {
    type Item = (Vec<u8>, O);

    fn next(&mut self) -> Option<(Vec<u8>, O)> {
        match self.0.next() {
            None => None,
            Some((k, v)) => {
                // debug!("next {:?} {:?}", v, k);
                let record = O::from_kv_bytes(&*k, &*v).unwrap();
                Some((k.to_vec(),record))
            }
        }
    }
}

pub fn preceding<'a>(s: &'a Snapshot, cf_handle: &impl AsColumnFamilyRef, key: Vec<u8>) -> DBIteratorWithThreadMode<'a, DBWithThreadMode<MultiThreaded>> {
    s.pit.iterator_cf_opt(
        cf_handle,
        ReadOptions::default(),
        IteratorMode::From(key.as_slice(), Direction::Reverse)
    )
}

pub fn following_light<'a,O>(s: &'a Snapshot, cf_handle: &impl AsColumnFamilyRef, key: &Vec<u8>) -> LightIterator<'a,O> {
    let it = s.pit.iterator_cf_opt(
        cf_handle,
        ReadOptions::default(),
        IteratorMode::From(key.as_slice(), Direction::Forward)
    );
    LightIterator(it, PhantomData)
}

fn following_heavy<'a,O>(s: &'a Snapshot, cf_handle: &impl AsColumnFamilyRef, key: &Vec<u8>) -> HeavyIterator<'a,O> {
    let it = s.pit.iterator_cf_opt(
        cf_handle,
        ReadOptions::default(),
        IteratorMode::From(key.as_slice(), Direction::Forward)
    );
    HeavyIterator(it, PhantomData)
}

pub struct BetweenLightIterator<'a,O>(LightIterator<'a,O>, Vec<u8>);

impl<'a,O:FromBytes<O>> Iterator for BetweenLightIterator<'a,O> {
    type Item = (Vec<u8>, O);

    fn next(&mut self) -> Option<(Vec<u8>, O)> {
        match self.0.next() {
            None => None,
            Some((k, v)) => {
                if &k <= &self.1 {
                    Some((k, v))
                } else {
                    None
                }
            }
        }
    }
}

pub struct BetweenHeavyIterator<'a,O>(HeavyIterator<'a,O>, Vec<u8>);

impl<'a,O:FromKVBytes<O>> Iterator for BetweenHeavyIterator<'a,O> {
    type Item = (Vec<u8>, O);

    fn next(&mut self) -> Option<(Vec<u8>, O)> {
        match self.0.next() {
            None => None,
            Some((k, v)) => {
                if &k <= &self.1 {
                    Some((k,v))
                } else {
                    None
                }
            }
        }
    }
}

impl OpsManager {

    // pub(crate) fn ops_preceding<'a, O: FromBytes<O>>(&self, s: &'a Snapshot, position: &Vec<u8>) -> Result<ItemsIterator<'a,O>, DBError> {
    //     Ok(preceding(s, &s.cf_operations(), position))
    // }

    pub(crate) fn ops_following<'a, O:FromBytes<O>>(&self, s: &'a Snapshot, position: &Vec<u8>) -> LightIterator<'a,O> {
        following_light(s, &s.cf_operations(), position)
    }

    pub(crate) fn get_closest_light_value<'a,O: FromBytes<O>>(&self, s: &'a Snapshot, position: Vec<u8>) -> Option<(Vec<u8>, O)> {
        LightIterator(
            preceding(s, &s.cf_values(), position),
            PhantomData
        ).next()
    }

    pub(crate) fn get_closest_memo<'a,O:FromKVBytes<O>>(&self, s: &'a Snapshot, position: Vec<u8>) -> Option<O> {
        let mut it = HeavyIterator(preceding(s, &s.cf_values(), position), PhantomData);
        if let Some((_,value)) = it.next() {
            Some(value)
        } else {
            None
        }
    }

    pub(crate) fn memos_after<'a,O>(&self, s: &'a Snapshot, position: &Vec<u8>) -> LightIterator<'a,O> {
        following_light(s, &s.cf_values(), position)
    }

    pub(crate) fn ops_between_light<'a,O>(&self, s: &'a Snapshot, from: Vec<u8>, till: Vec<u8>) -> BetweenLightIterator<'a,O> {
        let it = following_light(s, &s.cf_operations(), &from);
        BetweenLightIterator(it, till)
    }

    pub(crate) fn ops_between_heavy<'a,O>(&self, s: &'a Snapshot, from: Vec<u8>, till: Vec<u8>) -> BetweenHeavyIterator<'a,O> {
        BetweenHeavyIterator(
            following_heavy(s, &s.cf_operations(), &from),
            till
        )
    }

    pub(crate) fn write_ops<BO,BV,TO,TV>(&self, tx: &mut Txn, ops: Vec<TO>) -> Result<(), DBError>
    where
        BV: Object<BO>,
        BO: Operation<BV>,

        TV: ObjectInTopology<BV,BO,TO>,
        TO: OperationInTopology<BV,BO,TV>,
    {
        let s = tx.s;
        let ops_manager = s.rf.ops_manager.clone();

        for op in ops {
            // calculate delta for propagation
            let delta_op: BO = if let Some(current) = tx.get_operation::<BV,BO,TV,TO>(&op)? {
                current.delta_between(&op.operation())
            } else {
                op.operation()
            };

            // store
            tx.put_operation::<BV,BO,TV,TO>(&op)?;

            // propagation
            for (position, value) in ops_manager.memos_after::<BV>(s, &op.position()) {
                // TODO get dependents and notify them

                debug!("update value {:?} {:?}", value, position);

                let value = value.apply(&delta_op)?;

                // store updated memo
                tx.update_value(&position, &value)?;
            }
        }

        Ok(())
    }

    pub(crate) fn write_aggregation_delta<BV,BO,TV,TO>(&self, tx: &mut Txn, op: TO) -> Result<(), DBError>
        where
            BV: AObject<BO> + Debug,
            BO: AOperation<BV> + Debug,
            TV: AObjectInTopology<BV,BO,TO> + Debug,
            TO: AOperationInTopology<BV,BO,TV> + Debug,
    {
        let s = tx.s;
        let ops_manager = s.rf.ops_manager.clone();

        let local_topology_position = op.position();
        let local_topology_checkpoint = op.position_of_aggregation()?;

        debug!("propagate delta {:?} at {:?}", op, local_topology_position);

        // propagation
        for (position, value) in ops_manager.memos_after::<BV>(s, &local_topology_position) {
            // TODO get dependents and notify them

            debug!("next memo {:?} at {:?}", value, position);

            let value = value.apply_aggregation(&op.operation())?;

            // store updated memo
            tx.update_value(&position, &value)?;
        }

        // make sure checkpoint exist
        match tx.value::<BO>(&local_topology_checkpoint)? {
            None => {
                let value = op.to_value();
                // store checkpoint
                tx.put_value(&value)?;
            }
            Some(_) => {} // exist, updated above
        }

        Ok(())
    }
}