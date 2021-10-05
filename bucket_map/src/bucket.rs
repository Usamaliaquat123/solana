use crate::bucket_item::BucketItem;
use crate::bucket_map::BucketMapError;
use crate::bucket_stats::BucketMapStats;
use crate::bucket_storage::{BucketStorage, Uid, UID_UNLOCKED};
use crate::index_entry::IndexEntry;
use crate::{MaxSearch, RefCount};
use rand::thread_rng;
use rand::Rng;
use solana_measure::measure::Measure;
use solana_sdk::pubkey::Pubkey;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::marker::PhantomData;
use std::ops::RangeBounds;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

#[derive(Default)]
pub struct ReallocatedItems {
    // Some if the index was reallocated
    // u64 is random associated with the new index
    pub index: Option<(u64, BucketStorage)>,
    // Some for a data bucket reallocation
    // u64 is data bucket index
    pub data: Option<(u64, BucketStorage)>,
}

#[derive(Default)]
pub struct Reallocated {
    /// > 0 if reallocations are encoded
    pub active_reallocations: AtomicUsize,

    /// actual reallocated bucket
    /// mutex because bucket grow code runs with a read lock
    pub items: Mutex<ReallocatedItems>,
}

impl Reallocated {
    /// specify that a reallocation has occurred
    pub fn add_reallocation(&self) {
        assert_eq!(
            0,
            self.active_reallocations.fetch_add(1, Ordering::Relaxed),
            "Only 1 reallocation can occur at a time"
        );
    }
    /// Return true IFF a reallocation has occurred.
    /// Calling this takes conceptual ownership of the reallocation encoded in the struct.
    pub fn get_reallocated(&self) -> bool {
        self.active_reallocations
            .compare_exchange(1, 0, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
    }
}

// >= 2 instances of BucketStorage per 'bucket' in the bucket map. 1 for index, >= 1 for data
pub struct Bucket<T> {
    drives: Arc<Vec<PathBuf>>,
    //index
    pub index: BucketStorage,
    //random offset for the index
    random: u64,
    //storage buckets to store SlotSlice up to a power of 2 in len
    pub data: Vec<BucketStorage>,
    _phantom: PhantomData<T>,
    stats: Arc<BucketMapStats>,

    pub reallocated: Reallocated,
}

impl<T: Clone + Copy> Bucket<T> {
    pub fn new(
        drives: Arc<Vec<PathBuf>>,
        max_search: MaxSearch,
        stats: Arc<BucketMapStats>,
    ) -> Self {
        let index = BucketStorage::new(
            Arc::clone(&drives),
            1,
            std::mem::size_of::<IndexEntry>() as u64,
            max_search,
            Arc::clone(&stats.index),
        );
        Self {
            random: thread_rng().gen(),
            drives,
            index,
            data: vec![],
            _phantom: PhantomData::default(),
            stats,
            reallocated: Reallocated::default(),
        }
    }

    pub fn bucket_len(&self) -> u64 {
        self.index.used.load(Ordering::Relaxed)
    }

    pub fn keys(&self) -> Vec<Pubkey> {
        let mut rv = vec![];
        for i in 0..self.index.capacity() {
            if self.index.uid(i) == UID_UNLOCKED {
                continue;
            }
            let ix: &IndexEntry = self.index.get(i);
            rv.push(ix.key);
        }
        rv
    }

    pub fn items_in_range<R>(&self, range: &Option<&R>) -> Vec<BucketItem<T>>
    where
        R: RangeBounds<Pubkey>,
    {
        let mut result = Vec::with_capacity(self.index.used.load(Ordering::Relaxed) as usize);
        for i in 0..self.index.capacity() {
            let ii = i % self.index.capacity();
            if self.index.uid(ii) == UID_UNLOCKED {
                continue;
            }
            let ix: &IndexEntry = self.index.get(ii);
            let key = ix.key;
            if range.map(|r| r.contains(&key)).unwrap_or(true) {
                let val = ix.read_value(self);
                result.push(BucketItem {
                    pubkey: key,
                    ref_count: ix.ref_count(),
                    slot_list: val.map(|(v, _ref_count)| v.to_vec()).unwrap_or_default(),
                });
            }
        }
        result
    }

    pub fn find_entry(&self, key: &Pubkey) -> Option<(&IndexEntry, u64)> {
        Self::bucket_find_entry(&self.index, key, self.random)
    }

    fn find_entry_mut(&self, key: &Pubkey) -> Option<(&mut IndexEntry, u64)> {
        Self::bucket_find_entry_mut(&self.index, key, self.random)
    }

    fn bucket_find_entry_mut<'a>(
        index: &'a BucketStorage,
        key: &Pubkey,
        random: u64,
    ) -> Option<(&'a mut IndexEntry, u64)> {
        let ix = Self::bucket_index_ix(index, key, random);
        for i in ix..ix + index.max_search() {
            let ii = i % index.capacity();
            if index.uid(ii) == UID_UNLOCKED {
                continue;
            }
            let elem: &mut IndexEntry = index.get_mut(ii);
            if elem.key == *key {
                return Some((elem, ii));
            }
        }
        None
    }

    fn bucket_find_entry<'a>(
        index: &'a BucketStorage,
        key: &Pubkey,
        random: u64,
    ) -> Option<(&'a IndexEntry, u64)> {
        let ix = Self::bucket_index_ix(index, key, random);
        for i in ix..ix + index.max_search() {
            let ii = i % index.capacity();
            if index.uid(ii) == UID_UNLOCKED {
                continue;
            }
            let elem: &IndexEntry = index.get(ii);
            if elem.key == *key {
                return Some((elem, ii));
            }
        }
        None
    }

    fn bucket_create_key(
        index: &BucketStorage,
        key: &Pubkey,
        elem_uid: Uid,
        random: u64,
        ref_count: u64,
    ) -> Result<u64, BucketMapError> {
        let ix = Self::bucket_index_ix(index, key, random);
        for i in ix..ix + index.max_search() {
            let ii = i as u64 % index.capacity();
            if index.uid(ii) != UID_UNLOCKED {
                continue;
            }
            index.allocate(ii, elem_uid).unwrap();
            let mut elem: &mut IndexEntry = index.get_mut(ii);
            elem.key = *key;
            elem.ref_count = ref_count;
            elem.storage_offset = 0;
            elem.storage_capacity_when_created_pow2 = 0;
            elem.num_slots = 0;
            //debug!(                "INDEX ALLOC {:?} {} {} {}",                key, ii, index.capacity, elem_uid            );
            return Ok(ii);
        }
        Err(BucketMapError::IndexNoSpace(index.capacity_pow2))
    }

    pub fn addref(&mut self, key: &Pubkey) -> Option<RefCount> {
        let (elem, _) = self.find_entry_mut(key)?;
        elem.ref_count += 1;
        Some(elem.ref_count)
    }

    pub fn unref(&mut self, key: &Pubkey) -> Option<RefCount> {
        let (elem, _) = self.find_entry_mut(key)?;
        elem.ref_count -= 1;
        Some(elem.ref_count)
    }

    fn create_key(&self, key: &Pubkey, ref_count: u64) -> Result<u64, BucketMapError> {
        Self::bucket_create_key(
            &self.index,
            key,
            IndexEntry::key_uid(key),
            self.random,
            ref_count,
        )
    }

    pub fn read_value(&self, key: &Pubkey) -> Option<(&[T], RefCount)> {
        //debug!("READ_VALUE: {:?}", key);
        let (elem, _) = self.find_entry(key)?;
        elem.read_value(self)
    }

    pub fn try_write(
        &mut self,
        key: &Pubkey,
        data: &[T],
        ref_count: u64,
    ) -> Result<(), BucketMapError> {
        let best_fit_bucket = IndexEntry::data_bucket_from_num_slots(data.len() as u64);
        if self.data.get(best_fit_bucket as usize).is_none() {
            // fail early if the data bucket we need doesn't exist - we don't want the index entry partially allocated
            return Err(BucketMapError::DataNoSpace((best_fit_bucket, 0)));
        }
        let index_entry = self.find_entry_mut(key);
        let (elem, elem_ix) = match index_entry {
            None => {
                let ii = self.create_key(key, ref_count)?;
                let elem = self.index.get_mut(ii); // get_mut here is right?
                (elem, ii)
            }
            Some(res) => {
                if ref_count != res.0.ref_count {
                    res.0.ref_count = ref_count;
                }
                res
            }
        };
        let elem_uid = self.index.uid(elem_ix);
        let bucket_ix = elem.data_bucket_ix();
        let current_bucket = &self.data[bucket_ix as usize];
        if best_fit_bucket == bucket_ix && elem.num_slots > 0 {
            //in place update
            let elem_loc = elem.data_loc(current_bucket);
            let slice: &mut [T] = current_bucket.get_mut_cell_slice(elem_loc, data.len() as u64);
            //let elem: &mut IndexEntry = self.index.get_mut(elem_ix);
            assert!(current_bucket.uid(elem_loc) == elem_uid);
            elem.num_slots = data.len() as u64;
            slice.clone_from_slice(data);
            Ok(())
        } else {
            //need to move the allocation to a best fit spot
            let best_bucket = &self.data[best_fit_bucket as usize];
            let cap_power = best_bucket.capacity_pow2;
            let cap = best_bucket.capacity();
            let pos = thread_rng().gen_range(0, cap);
            for i in pos..pos + self.index.max_search() {
                let ix = i % cap;
                if best_bucket.uid(ix) == UID_UNLOCKED {
                    let elem_loc = elem.data_loc(current_bucket);
                    if elem.num_slots > 0 {
                        current_bucket.free(elem_loc, elem_uid);
                    }
                    // elem: &mut IndexEntry = self.index.get_mut(elem_ix);
                    elem.storage_offset = ix;
                    elem.storage_capacity_when_created_pow2 = best_bucket.capacity_pow2;
                    elem.num_slots = data.len() as u64;
                    //debug!(                        "DATA ALLOC {:?} {} {} {}",                        key, elem.data_location, best_bucket.capacity, elem_uid                    );
                    if elem.num_slots > 0 {
                        best_bucket.allocate(ix, elem_uid).unwrap();
                        let slice = best_bucket.get_mut_cell_slice(ix, data.len() as u64);
                        slice.copy_from_slice(data);
                    }
                    return Ok(());
                }
            }
            Err(BucketMapError::DataNoSpace((best_fit_bucket, cap_power)))
        }
    }

    pub fn delete_key(&mut self, key: &Pubkey) {
        if let Some((elem, elem_ix)) = self.find_entry(key) {
            let elem_uid = self.index.uid(elem_ix);
            if elem.num_slots > 0 {
                let data_bucket = &self.data[elem.data_bucket_ix() as usize];
                let loc = elem.data_loc(data_bucket);
                //debug!(                    "DATA FREE {:?} {} {} {}",                    key, elem.data_location, data_bucket.capacity, elem_uid                );
                data_bucket.free(loc, elem_uid);
            }
            //debug!("INDEX FREE {:?} {}", key, elem_uid);
            self.index.free(elem_ix, elem_uid);
        }
    }

    pub fn grow_index(&self, current_capacity: u8) {
        if self.index.capacity_pow2 == current_capacity {
            let mut m = Measure::start("grow_index");
            //debug!("GROW_INDEX: {}", current_capacity);
            let increment = 1;
            for i in increment.. {
                //increasing the capacity by ^4 reduces the
                //likelyhood of a re-index collision of 2^(max_search)^2
                //1 in 2^32
                let index = BucketStorage::new_with_capacity(
                    Arc::clone(&self.drives),
                    1,
                    std::mem::size_of::<IndexEntry>() as u64,
                    // *2 causes rapid growth of index buckets
                    self.index.capacity_pow2 + i, // * 2,
                    self.index.max_search,
                    Arc::clone(&self.stats.index),
                );
                let random = thread_rng().gen();
                let mut valid = true;
                for ix in 0..self.index.capacity() {
                    let uid = self.index.uid(ix);
                    if UID_UNLOCKED != uid {
                        let elem: &IndexEntry = self.index.get(ix);
                        let ref_count = 0; // ??? TODO
                        let new_ix =
                            Self::bucket_create_key(&index, &elem.key, uid, random, ref_count);
                        if new_ix.is_err() {
                            valid = false;
                            break;
                        }
                        let new_ix = new_ix.unwrap();
                        let new_elem: &mut IndexEntry = index.get_mut(new_ix);
                        *new_elem = *elem;
                        /*
                        let dbg_elem: IndexEntry = *new_elem;
                        assert_eq!(
                            Self::bucket_find_entry(&index, &elem.key, random).unwrap(),
                            (&dbg_elem, new_ix)
                        );
                        */
                    }
                }
                if valid {
                    let sz = index.capacity();
                    {
                        let mut max = self.stats.index.max_size.lock().unwrap();
                        *max = std::cmp::max(*max, sz);
                    }
                    let mut items = self.reallocated.items.lock().unwrap();
                    items.index = Some((random, index));
                    self.reallocated.add_reallocation();
                    break;
                }
            }
            m.stop();
            self.stats.index.resizes.fetch_add(1, Ordering::Relaxed);
            self.stats
                .index
                .resize_us
                .fetch_add(m.as_us(), Ordering::Relaxed);
        }
    }

    pub fn apply_grow_index(&mut self, random: u64, index: BucketStorage) {
        self.random = random;
        self.index = index;
    }

    fn elem_size() -> u64 {
        std::mem::size_of::<T>() as u64
    }

    pub fn apply_grow_data(&mut self, ix: usize, bucket: BucketStorage) {
        if self.data.get(ix).is_none() {
            for i in self.data.len()..ix {
                // insert empty data buckets
                self.data.push(BucketStorage::new(
                    Arc::clone(&self.drives),
                    1 << i,
                    Self::elem_size(),
                    self.index.max_search,
                    Arc::clone(&self.stats.data),
                ))
            }
            self.data.push(bucket);
        } else {
            self.data[ix] = bucket;
        }
    }

    /// grow a data bucket
    /// The application of the new bucket is deferred until the next write lock.
    pub fn grow_data(&self, data_index: u64, current_capacity: u8) {
        let new_bucket = self.index.grow(
            self.data.get(data_index as usize),
            current_capacity + 1,
            1 << data_index,
            Self::elem_size(),
        );
        self.reallocated.add_reallocation();
        let mut items = self.reallocated.items.lock().unwrap();
        items.data = Some((data_index, new_bucket));
    }

    fn bucket_index_ix(index: &BucketStorage, key: &Pubkey, random: u64) -> u64 {
        let uid = IndexEntry::key_uid(key);
        let mut s = DefaultHasher::new();
        uid.hash(&mut s);
        //the locally generated random will make it hard for an attacker
        //to deterministically cause all the pubkeys to land in the same
        //location in any bucket on all validators
        random.hash(&mut s);
        let ix = s.finish();
        ix % index.capacity()
        //debug!(            "INDEX_IX: {:?} uid:{} loc: {} cap:{}",            key,            uid,            location,            index.capacity()        );
    }

    /// grow the appropriate piece. Note this takes an immutable ref.
    /// The actual grow is set into self.reallocated and applied later on a write lock
    pub fn grow(&self, err: BucketMapError) {
        match err {
            BucketMapError::DataNoSpace((data_index, current_capacity)) => {
                //debug!("GROWING SPACE {:?}", (data_index, current_capacity));
                self.grow_data(data_index, current_capacity);
            }
            BucketMapError::IndexNoSpace(current_capacity) => {
                //debug!("GROWING INDEX {}", sz);
                self.grow_index(current_capacity);
            }
        }
    }

    /// if a bucket was resized previously with a read lock, then apply that resize now
    pub fn handle_delayed_grows(&mut self) {
        if self.reallocated.get_reallocated() {
            // swap out the bucket that was resized previously with a read lock
            let mut items = ReallocatedItems::default();
            std::mem::swap(&mut items, &mut self.reallocated.items.lock().unwrap());

            if let Some((random, bucket)) = items.index.take() {
                self.apply_grow_index(random, bucket);
            } else {
                // data bucket
                let (i, new_bucket) = items.data.take().unwrap();
                self.apply_grow_data(i as usize, new_bucket);
            }
        }
    }

    pub fn insert(&mut self, key: &Pubkey, value: (&[T], RefCount)) {
        let (new, refct) = value;
        loop {
            let rv = self.try_write(key, new, refct);
            match rv {
                Ok(_) => return,
                Err(err) => {
                    self.grow(err);
                    self.handle_delayed_grows();
                }
            }
        }
    }

    pub fn update<F>(&mut self, key: &Pubkey, updatefn: F)
    where
        F: Fn(Option<(&[T], RefCount)>) -> Option<(Vec<T>, RefCount)>,
    {
        let current = self.read_value(key);
        let new = updatefn(current);
        if new.is_none() {
            self.delete_key(key);
            return;
        }
        let (new, refct) = new.unwrap();
        self.insert(key, (&new, refct));
    }
}