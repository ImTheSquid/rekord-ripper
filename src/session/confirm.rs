//! Confirmations waiting on a yes.
//!
//! Several can wait at once: a download's import can come up while a transfer
//! is being reviewed, and neither may silently replace the other. A modal
//! frontend shows the newest; the others wait behind it.

use super::queue::{ForceBatch, ImportBatch};
use super::transfer::PendingBatch;

/// Which confirmation, and therefore what a yes means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ConfirmKind {
    /// The transfer screen's cp batch.
    Transfer,
    /// Creating `djmdContent` rows for queued downloads.
    ImportRows,
    /// Transferring one queued entry with the fingerprint gate skipped.
    ForceApply,
    /// Emptying the whole download queue.
    ClearQueue,
}

/// What a yes writes, snapshotted when the confirmation opened.
pub enum Batch {
    Transfer(PendingBatch),
    ImportRows(ImportBatch),
    ForceApply(ForceBatch),
    ClearQueue,
}

impl Batch {
    pub fn kind(&self) -> ConfirmKind {
        match self {
            Self::Transfer(_) => ConfirmKind::Transfer,
            Self::ImportRows(_) => ConfirmKind::ImportRows,
            Self::ForceApply(_) => ConfirmKind::ForceApply,
            Self::ClearQueue => ConfirmKind::ClearQueue,
        }
    }
}

/// Open confirmations, oldest first. At most one of each kind: reopening one
/// replaces its batch, because the newer snapshot is the one being asked about.
#[derive(Default)]
pub struct Confirmations(Vec<Batch>);

impl Confirmations {
    pub fn open(&mut self, batch: Batch) {
        self.0.retain(|b| b.kind() != batch.kind());
        self.0.push(batch);
    }

    pub fn newest(&self) -> Option<ConfirmKind> {
        self.0.last().map(Batch::kind)
    }

    pub fn kinds(&self) -> impl Iterator<Item = ConfirmKind> + '_ {
        self.0.iter().map(Batch::kind)
    }

    pub fn is_open(&self, kind: ConfirmKind) -> bool {
        self.0.iter().any(|b| b.kind() == kind)
    }

    pub fn take(&mut self, kind: ConfirmKind) -> Option<Batch> {
        let i = self.0.iter().position(|b| b.kind() == kind)?;
        Some(self.0.remove(i))
    }

    pub fn transfer(&self) -> Option<&PendingBatch> {
        self.0.iter().find_map(|b| match b {
            Batch::Transfer(p) => Some(p),
            _ => None,
        })
    }

    pub fn import_rows(&self) -> Option<&ImportBatch> {
        self.0.iter().find_map(|b| match b {
            Batch::ImportRows(p) => Some(p),
            _ => None,
        })
    }

    pub fn force_apply(&self) -> Option<&ForceBatch> {
        self.0.iter().find_map(|b| match b {
            Batch::ForceApply(p) => Some(p),
            _ => None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn transfer() -> Batch {
        Batch::Transfer(PendingBatch {
            plans: vec![],
            failures: vec![("1".into(), "x".into())],
        })
    }

    #[test]
    fn a_second_kind_waits_behind_rather_than_replacing() {
        // The bug this guards: one slot meant an import opened by a finished
        // download replaced the transfer being reviewed, and cancelling either
        // threw both away.
        let mut c = Confirmations::default();
        c.open(transfer());
        c.open(Batch::ClearQueue);
        assert_eq!(c.newest(), Some(ConfirmKind::ClearQueue));
        assert!(c.take(ConfirmKind::ClearQueue).is_some());
        assert_eq!(c.newest(), Some(ConfirmKind::Transfer));
        assert_eq!(c.transfer().unwrap().failures.len(), 1);
    }

    #[test]
    fn reopening_a_kind_replaces_its_batch_and_makes_it_newest() {
        let mut c = Confirmations::default();
        c.open(transfer());
        c.open(Batch::ClearQueue);
        c.open(Batch::Transfer(PendingBatch {
            plans: vec![],
            failures: vec![],
        }));
        assert_eq!(c.kinds().count(), 2);
        assert_eq!(c.newest(), Some(ConfirmKind::Transfer));
        assert!(c.transfer().unwrap().failures.is_empty());
    }

    #[test]
    fn taking_a_kind_that_is_not_open_is_nothing() {
        let mut c = Confirmations::default();
        assert!(c.take(ConfirmKind::ImportRows).is_none());
        assert_eq!(c.newest(), None);
    }
}
