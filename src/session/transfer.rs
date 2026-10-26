//! The transfer screen's batch: one source's analysis onto chosen destinations.

use crate::analysis::{self, Plan};
use crate::db::safety_preflight;

use super::{ConfirmKind, Session};

pub struct PendingBatch {
    pub plans: Vec<Plan>,
    pub failures: Vec<(String, String)>, // dst_id, error
}

impl Session {
    /// Plan copying `src_id` onto every destination, and ask.
    pub fn build_pending(&mut self, src_id: &str, mut dst_ids: Vec<String>) {
        if dst_ids.is_empty() {
            self.status.err("no destinations selected");
            return;
        }
        dst_ids.sort();

        let mut plans = Vec::new();
        let mut failures = Vec::new();
        for dst_id in dst_ids {
            match analysis::build_plan(&self.db, src_id, &dst_id, &self.copy_opts) {
                Ok(plan) => plans.push(plan),
                Err(e) => failures.push((dst_id, e.to_string())),
            }
        }

        self.pending = Some(PendingBatch { plans, failures });
        self.confirm = Some(ConfirmKind::Transfer);
    }

    /// Write the batch the user was shown. True when the writes were attempted,
    /// so the frontend knows to clear its destination selection.
    pub fn apply_pending(&mut self) -> bool {
        let Some(batch) = self.pending.take() else {
            self.confirm = None;
            return false;
        };

        if let Err(e) = safety_preflight(self.safety) {
            // Put the batch back so the user can read the confirmation again.
            self.pending = Some(batch);
            self.status.err(format!("{e}"));
            return false;
        }

        let total = batch.plans.len();
        let mut errs: Vec<String> = Vec::new();
        let mut backup_path = None;
        for plan in &batch.plans {
            match analysis::apply_plan(&mut self.db, plan) {
                Ok(path) => {
                    backup_path.get_or_insert(path);
                }
                Err(e) => {
                    let dst_label = plan
                        .dst
                        .title
                        .as_deref()
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&plan.dst.id);
                    errs.push(format!("\"{dst_label}\": {e}"));
                }
            }
        }
        let ok = total - errs.len();
        self.confirm = None;

        if let Err(e) = self.reload_rows() {
            self.status.warn(format!("reload after apply failed: {e}"));
        }

        let backup_hint = backup_path
            .map(|p| format!(" Backup: {}", p.display()))
            .unwrap_or_default();
        if errs.is_empty() {
            self.status.ok(format!("Applied {ok}/{total}.{backup_hint}"));
        } else {
            self.unresolved_errors = true;
            let extra = if errs.len() > 1 {
                format!(" (+{} more)", errs.len() - 1)
            } else {
                String::new()
            };
            self.status.err(format!(
                "Applied {ok}/{total}. Failed → {first}{extra}{backup_hint}",
                first = errs[0],
            ));
        }
        true
    }
}
