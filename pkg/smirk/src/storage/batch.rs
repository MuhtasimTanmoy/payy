use std::{collections::HashMap, hash::Hash};

use borsh::{BorshDeserialize, BorshSerialize};
use rocksdb::WriteBatch;
use wire_message::WireMessage;

use crate::{
    hash_cache::KnownHash,
    storage::format::{ValueFormat, ValueV2},
    Batch,
};

use super::{
    format::{KeyFormat, KeyV2},
    Error, Persistent,
};

impl<const DEPTH: usize, V> Persistent<DEPTH, V> {
    /// Insert a [`Batch`] into this [`Persistent`] tree
    ///
    /// ```rust
    /// # use smirk::*;
    /// # use smirk::storage::*;
    /// # let dir = tempdir::TempDir::new("smirk_doctest").unwrap();
    /// # let path = dir.path().join("db");
    /// let mut persistent = Persistent::<64, ()>::new(&path).unwrap();
    /// let batch = batch! { 1, 2, 3 };
    ///
    /// persistent.insert_batch(batch).unwrap();
    ///
    /// assert!(persistent.tree().contains_element(Element::new(1)));
    /// assert!(persistent.tree().contains_element(Element::new(2)));
    /// assert!(persistent.tree().contains_element(Element::new(3)));
    /// ```
    pub fn insert_batch(&mut self, batch: Batch<DEPTH, V>) -> Result<(), Error>
    where
        V: BorshSerialize + BorshDeserialize + Send + Sync + 'static + Clone,
    {
        if batch.is_empty() {
            return Ok(());
        }

        let new_kv_pairs: HashMap<_, _> = batch.entries().cloned().collect();

        let old_hashes = std::thread::spawn({
            let tree = self.tree.clone();
            move || {
                let mut hashes = tree.known_hashes();
                hashes.sort();
                hashes
            }
        });

        self.tree.insert_batch(batch)?;

        let mut new_hashes = self.tree.known_hashes();
        new_hashes.sort();

        let old_hashes = old_hashes.join().unwrap();

        // This assumes old_hashes and new_hashes have no duplicates,
        // which should be reasonable.
        let (hashes_to_remove, hashes_to_insert) =
            diff_sorted_unique_arrays(&old_hashes, &new_hashes);

        let mut write_batch = WriteBatch::default();

        for (key, value) in new_kv_pairs {
            // insert the v2 key
            let new_key = KeyFormat::V2(KeyV2::Element(key));
            let value = ValueFormat::V2(ValueV2::Metadata(value.into()));
            write_batch.put(new_key.to_bytes().unwrap(), value.to_bytes().unwrap());

            // make sure we don't end up with the v1 and v2 key for the same element at the same
            // time
            let old_key = KeyFormat::V1(key);
            write_batch.delete(old_key.to_bytes().unwrap());
        }

        for KnownHash { left, right, .. } in hashes_to_remove {
            let key = KeyFormat::V2(KeyV2::KnownHash { left, right });
            write_batch.delete(key.to_bytes().unwrap());
        }

        for KnownHash {
            left,
            right,
            result,
        } in hashes_to_insert
        {
            let key = KeyFormat::V2(KeyV2::KnownHash { left, right });
            let value = ValueFormat::<V>::V2(ValueV2::KnownHash(result));
            write_batch.put(key.to_bytes().unwrap(), value.to_bytes().unwrap());
        }

        self.db.write(write_batch)?;

        // TODO: handle case where rocksdb fails with pending list

        Ok(())
    }
}

/// Returns the difference between two sorted arrays.
///
/// Requires that the arrays are sorted and contain unique elements.
fn diff_sorted_unique_arrays<T: PartialOrd + Clone + Hash + Eq>(
    old: &[T],
    new: &[T],
) -> (Vec<T>, Vec<T>) {
    #[cfg(not(test))]
    {
        use std::collections::HashSet;

        // dev assert, diff_sorted_unique_arrays can't handle duplicates
        debug_assert!(
            HashSet::<_>::from_iter(old.iter().cloned()).len() == old.len(),
            "old_hashes has duplicates"
        );
        debug_assert!(
            HashSet::<_>::from_iter(new.iter().cloned()).len() == new.len(),
            "new_hashes has duplicates"
        );
    }

    let mut removed = Vec::new();
    let mut added = Vec::new();
    let mut i = 0;
    let mut j = 0;

    while i < old.len() && j < new.len() {
        match old[i].partial_cmp(&new[j]) {
            Some(std::cmp::Ordering::Less) => {
                removed.push(old[i].clone());
                i += 1;
            }
            Some(std::cmp::Ordering::Greater) => {
                added.push(new[j].clone());
                j += 1;
            }
            Some(std::cmp::Ordering::Equal) => {
                i += 1;
                j += 1;
            }
            None => unreachable!(),
        }
    }

    // Add remaining elements in old to removed
    while i < old.len() {
        removed.push(old[i].clone());
        i += 1;
    }

    // Add remaining elements in new to added
    while j < new.len() {
        added.push(new[j].clone());
        j += 1;
    }

    (removed, added)
}

#[cfg(test)]
mod tests {
    #[test]
    fn diff_sorted_unique_arrays() {
        let vec1 = vec![1, 2, 3, 4, 5];
        let vec2 = vec![2, 3, 4, 6, 7];

        let (removed, added) = super::diff_sorted_unique_arrays(&vec1, &vec2);

        assert_eq!(removed, vec![1, 5]);
        assert_eq!(added, vec![6, 7]);
    }

    #[test]
    fn diff_sorted_unique_arrays_empty() {
        let vec1 = vec![1, 2, 3, 4, 5];
        let vec2 = vec![];

        let (removed, added) = super::diff_sorted_unique_arrays(&vec1, &vec2);

        assert_eq!(removed, vec![1, 2, 3, 4, 5]);
        assert_eq!(added, vec![]);
    }

    #[test]
    fn diff_sorted_unique_arrays_same() {
        let vec1 = vec![1, 2, 3, 4, 5];
        let vec2 = vec![1, 2, 3, 4, 5];

        let (removed, added) = super::diff_sorted_unique_arrays(&vec1, &vec2);

        assert_eq!(removed, vec![]);
        assert_eq!(added, vec![]);
    }

    #[test]
    fn diff_sorted_unique_arrays_no_overlap() {
        let vec1 = vec![1, 2, 3];
        let vec2 = vec![4, 5, 6];

        let (removed, added) = super::diff_sorted_unique_arrays(&vec1, &vec2);

        assert_eq!(removed, vec![1, 2, 3]);
        assert_eq!(added, vec![4, 5, 6]);
    }

    #[test]
    fn diff_sorted_unique_arrays_dups_quirk() {
        let vec1 = vec![1, 2, 3, 4, 5];
        let vec2 = vec![1, 1, 2, 3, 4, 5];

        let (removed, added) = super::diff_sorted_unique_arrays(&vec1, &vec2);

        assert_eq!(removed, vec![]);
        // Ideally, we would get vec![], but the current implementation has a quirk around
        // duplicates due to performance reasons
        // assert_eq!(added, vec![]);
        // So, instead we get:
        assert_eq!(added, vec![1]);
    }
}
