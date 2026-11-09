//! In-memory write buffer holding WAL entries not yet folded into segments.
//!
//! The memtable is the read-your-writes layer: every committed WAL record is
//! applied here before the write call returns, and reads consult the memtable
//! before any segment. It keeps the *resolved* effect of each operation —
//! the engine resolves `Patch` operations against the currently visible
//! document at write time and logs the resulting full upsert, so memtable
//! replay is a pure function of the WAL (deterministic recovery, no
//! cross-file dependencies during replay).
//!
//! Entries are keyed by document id in a `BTreeMap`, so draining for a
//! segment flush yields rows already in the strictly ascending id order
//! [`SegmentBuilder`](crate::segment::SegmentBuilder) requires.

use std::collections::BTreeMap;

use reef_types::{DocId, Document};

use crate::segment::SegmentRow;

/// Resolved state of one document in the memtable.
#[derive(Debug, Clone, PartialEq)]
pub enum MemEntry {
    Live(Document),
    Tombstone,
}

#[derive(Debug, Clone)]
struct VersionedEntry {
    entry: MemEntry,
    /// WAL sequence of the last operation that produced this state.
    wal_seq: u64,
}

/// Rough per-entry size estimate used for flush thresholds. Precision is not
/// required; it only needs to be monotone in actual memory use.
fn estimate_size(id: &DocId, entry: &MemEntry) -> usize {
    let base = 64 + serde_json::to_vec(id).map(|v| v.len()).unwrap_or(16);
    match entry {
        MemEntry::Tombstone => base,
        MemEntry::Live(doc) => {
            let vec_bytes = doc.vector.as_ref().map(|v| v.len() * 4).unwrap_or(0);
            let sparse_bytes = doc
                .sparse_vector
                .as_ref()
                .map(|s| s.indices.len() * 4 + s.values.len() * 4)
                .unwrap_or(0);
            let attr_bytes = serde_json::to_vec(&doc.attributes)
                .map(|v| v.len())
                .unwrap_or(0);
            base + vec_bytes + sparse_bytes + attr_bytes
        }
    }
}

#[derive(Debug, Default)]
pub struct Memtable {
    entries: BTreeMap<DocId, VersionedEntry>,
    min_wal_seq: Option<u64>,
    max_wal_seq: Option<u64>,
    approx_bytes: usize,
    live_count: usize,
    tombstone_count: usize,
}

impl Memtable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Number of distinct document ids buffered (live + tombstoned).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn live_count(&self) -> usize {
        self.live_count
    }

    pub fn tombstone_count(&self) -> usize {
        self.tombstone_count
    }

    /// Approximate heap footprint; drives flush thresholds.
    pub fn approx_bytes(&self) -> usize {
        self.approx_bytes
    }

    /// Inclusive WAL sequence range covered by this memtable, if non-empty.
    pub fn wal_range(&self) -> Option<(u64, u64)> {
        match (self.min_wal_seq, self.max_wal_seq) {
            (Some(lo), Some(hi)) => Some((lo, hi)),
            _ => None,
        }
    }

    fn note_seq(&mut self, wal_seq: u64) {
        self.min_wal_seq = Some(self.min_wal_seq.map_or(wal_seq, |m| m.min(wal_seq)));
        self.max_wal_seq = Some(self.max_wal_seq.map_or(wal_seq, |m| m.max(wal_seq)));
    }

    fn insert(&mut self, id: DocId, entry: MemEntry, wal_seq: u64) {
        let new_size = estimate_size(&id, &entry);
        let is_live = matches!(entry, MemEntry::Live(_));
        let prev = self
            .entries
            .insert(id.clone(), VersionedEntry { entry, wal_seq });
        match prev {
            Some(old) => {
                self.approx_bytes = self
                    .approx_bytes
                    .saturating_sub(estimate_size(&id, &old.entry));
                match old.entry {
                    MemEntry::Live(_) => self.live_count -= 1,
                    MemEntry::Tombstone => self.tombstone_count -= 1,
                }
            }
            None => {}
        }
        self.approx_bytes += new_size;
        if is_live {
            self.live_count += 1;
        } else {
            self.tombstone_count += 1;
        }
        self.note_seq(wal_seq);
    }

    /// Apply a resolved upsert (full document replacement).
    pub fn upsert(&mut self, wal_seq: u64, doc: Document) {
        let id = doc.id.clone();
        self.insert(id, MemEntry::Live(doc), wal_seq);
    }

    /// Apply a delete. Tombstones are retained (not just removed from the
    /// map) because they must shadow older segment rows at flush time.
    pub fn delete(&mut self, wal_seq: u64, id: DocId) {
        self.insert(id, MemEntry::Tombstone, wal_seq);
    }

    /// Read-your-writes lookup. `Some(Tombstone)` means "definitely deleted";
    /// `None` means "not buffered here — consult segments".
    pub fn get(&self, id: &DocId) -> Option<&MemEntry> {
        self.entries.get(id).map(|v| &v.entry)
    }

    /// WAL sequence that last touched a buffered id (debugging/repair).
    pub fn entry_seq(&self, id: &DocId) -> Option<u64> {
        self.entries.get(id).map(|v| v.wal_seq)
    }

    /// Iterate buffered entries in ascending id order.
    pub fn iter(&self) -> impl Iterator<Item = (&DocId, &MemEntry, u64)> {
        self.entries.iter().map(|(id, v)| (id, &v.entry, v.wal_seq))
    }

    /// Snapshot as segment rows (ascending id order), cloning contents.
    /// The memtable stays intact so reads continue to be served while the
    /// flush is uploading; the engine drops/swaps it only after the manifest
    /// commit succeeds.
    pub fn to_segment_rows(&self) -> Vec<SegmentRow> {
        self.entries
            .iter()
            .map(|(id, v)| match &v.entry {
                MemEntry::Live(doc) => SegmentRow::Live(doc.clone()),
                MemEntry::Tombstone => SegmentRow::Tombstone(id.clone()),
            })
            .collect()
    }

    /// Reset to empty (after a successful flush + manifest commit).
    pub fn clear(&mut self) {
        self.entries.clear();
        self.min_wal_seq = None;
        self.max_wal_seq = None;
        self.approx_bytes = 0;
        self.live_count = 0;
        self.tombstone_count = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_types::Value;
    use std::collections::BTreeMap as Map;

    fn doc(id: &str, tag: i64) -> Document {
        let mut attributes = Map::new();
        attributes.insert("tag".to_string(), Value::Int(tag));
        Document {
            id: DocId::from(id),
            vector: Some(vec![1.0, 2.0]),
            sparse_vector: None,
            attributes,
        }
    }

    #[test]
    fn upsert_delete_and_read_your_writes() {
        let mut mt = Memtable::new();
        assert!(mt.is_empty());
        assert_eq!(mt.wal_range(), None);

        mt.upsert(1, doc("a", 1));
        mt.upsert(2, doc("b", 2));
        assert_eq!(mt.len(), 2);
        assert_eq!(mt.live_count(), 2);
        assert_eq!(mt.wal_range(), Some((1, 2)));

        match mt.get(&DocId::from("a")) {
            Some(MemEntry::Live(d)) => assert_eq!(d.attributes.get("tag"), Some(&Value::Int(1))),
            other => panic!("expected live, got {other:?}"),
        }

        // Overwrite keeps the count stable and updates content.
        mt.upsert(3, doc("a", 10));
        assert_eq!(mt.len(), 2);
        assert_eq!(mt.live_count(), 2);
        match mt.get(&DocId::from("a")) {
            Some(MemEntry::Live(d)) => assert_eq!(d.attributes.get("tag"), Some(&Value::Int(10))),
            other => panic!("expected live, got {other:?}"),
        }
        assert_eq!(mt.entry_seq(&DocId::from("a")), Some(3));

        // Delete converts to tombstone, never removes the key.
        mt.delete(4, DocId::from("a"));
        assert_eq!(mt.len(), 2);
        assert_eq!(mt.live_count(), 1);
        assert_eq!(mt.tombstone_count(), 1);
        assert!(matches!(mt.get(&DocId::from("a")), Some(MemEntry::Tombstone)));

        // Unknown id means "consult segments".
        assert!(mt.get(&DocId::from("zzz")).is_none());
        assert_eq!(mt.wal_range(), Some((1, 4)));
    }

    #[test]
    fn size_accounting_tracks_overwrites() {
        let mut mt = Memtable::new();
        mt.upsert(1, doc("a", 1));
        let after_one = mt.approx_bytes();
        assert!(after_one > 0);

        mt.upsert(2, doc("b", 2));
        let after_two = mt.approx_bytes();
        assert!(after_two > after_one);

        // Overwriting an entry with similar content should not grow
        // unboundedly.
        for seq in 3..50 {
            mt.upsert(seq, doc("a", seq as i64));
        }
        let after_overwrites = mt.approx_bytes();
        assert!(
            after_overwrites < after_two * 2,
            "overwrites leaked size accounting: {after_overwrites} vs {after_two}"
        );

        // Tombstones are cheaper than live docs.
        mt.delete(50, DocId::from("a"));
        assert!(mt.approx_bytes() < after_overwrites);

        mt.clear();
        assert_eq!(mt.approx_bytes(), 0);
        assert!(mt.is_empty());
        assert_eq!(mt.wal_range(), None);
    }

    #[test]
    fn segment_rows_come_out_sorted_with_tombstones() {
        let mut mt = Memtable::new();
        mt.upsert(1, doc("delta", 1));
        mt.upsert(2, doc("alpha", 2));
        mt.delete(3, DocId::from("charlie"));
        mt.upsert(4, doc("bravo", 4));

        let rows = mt.to_segment_rows();
        assert_eq!(rows.len(), 4);
        // Strictly ascending by id, as SegmentBuilder requires.
        for pair in rows.windows(2) {
            assert!(pair[0].id() < pair[1].id());
        }
        assert_eq!(rows.iter().filter(|r| r.is_tombstone()).count(), 1);

        // Snapshot does not consume the memtable.
        assert_eq!(mt.len(), 4);
    }
}
