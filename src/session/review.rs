//! A plan laid out as before and after, for a frontend that draws the change
//! rather than printing `Plan::render`.

use crate::analysis::{CueMark, Plan, cue_marks};
use crate::format::{format_bpm, format_length};

use super::{ConfirmKind, Session};

/// The analysis lock bit in `djmdContent.Analysed`.
const LOCKED: i64 = 0x80;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Change {
    pub label: &'static str,
    pub before: String,
    pub after: String,
}

impl Change {
    pub fn changed(&self) -> bool {
        self.before != self.after
    }
}

pub struct PlanReview {
    pub src_id: String,
    pub src_title: String,
    pub src_artist: String,
    pub dst_id: String,
    pub dst_title: String,
    pub dst_artist: String,
    pub changes: Vec<Change>,
    pub warnings: Vec<String>,
    /// The track's length once written, for placing cues on a timeline.
    pub length_ms: Option<i64>,
    pub cues_before: Vec<CueMark>,
    pub cues_after: Vec<CueMark>,
}

/// `src_key` and `dst_key` are key names; the plan only holds the key's id.
fn review(plan: &Plan, cues_before: Vec<CueMark>, src_key: &str, dst_key: &str) -> PlanReview {
    let (src, dst) = (&plan.src, &plan.dst);
    let mut cues_after = plan.cue_marks();
    if !plan.delete_existing_cues {
        cues_after.extend(cues_before.iter().copied());
        cues_after.sort_by_key(|c| c.ms);
    }
    let key_after = if plan.set_key_id.is_some() {
        src_key
    } else {
        dst_key
    };
    let length_after = plan.set_length.or(dst.length);
    let yes_no = |b: bool| if b { "yes" } else { "no" }.to_string();
    let name = |s: &Option<String>| s.clone().unwrap_or_default();
    let changes = vec![
        Change {
            label: "BPM",
            before: format_bpm(dst.bpm),
            after: format_bpm(plan.set_bpm.or(dst.bpm)),
        },
        Change {
            label: "Length",
            before: format_length(dst.length),
            after: format_length(length_after),
        },
        Change {
            label: "Key",
            before: dst_key.to_string(),
            after: key_after.to_string(),
        },
        Change {
            label: "Cues",
            before: dst.cue_count.to_string(),
            after: cues_after.len().to_string(),
        },
        Change {
            label: "Locked",
            before: yes_no(dst.analysed.unwrap_or(0) & LOCKED != 0),
            after: yes_no(plan.set_analysed & LOCKED != 0),
        },
    ];
    PlanReview {
        src_id: src.id.clone(),
        src_title: name(&src.title),
        src_artist: name(&src.artist),
        dst_id: dst.id.clone(),
        dst_title: name(&dst.title),
        dst_artist: name(&dst.artist),
        changes,
        warnings: plan.warnings.clone(),
        length_ms: length_after.map(|s| s * 1000),
        cues_before,
        cues_after,
    }
}

impl Session {
    /// The plans a transfer or force-apply confirmation would write.
    pub fn plan_reviews(&self, kind: ConfirmKind) -> Vec<PlanReview> {
        let plans: Vec<&Plan> = match kind {
            ConfirmKind::Transfer => self
                .confirmations
                .transfer()
                .map(|b| b.plans.iter().collect())
                .unwrap_or_default(),
            ConfirmKind::ForceApply => self
                .confirmations
                .force_apply()
                .map(|b| vec![&*b.plan])
                .unwrap_or_default(),
            ConfirmKind::ImportRows | ConfirmKind::ClearQueue => Vec::new(),
        };
        let key = |id: &str| self.row(id).map(|r| r.key.clone()).unwrap_or_default();
        plans
            .into_iter()
            .map(|p| {
                let before = cue_marks(&self.db, &p.dst.id).unwrap_or_default();
                review(p, before, &key(&p.src.id), &key(&p.dst.id))
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analysis::TrackHeader;

    fn header(id: &str, bpm: i64, analysed: i64, cue_count: i64) -> TrackHeader {
        TrackHeader {
            id: id.into(),
            uuid: format!("uuid-{id}"),
            title: Some(format!("title {id}")),
            artist: Some("artist".into()),
            bpm: Some(bpm),
            length: Some(200),
            analysed: Some(analysed),
            analysis_data_path: None,
            file_type: Some(5),
            cue_count,
            folder_path: None,
            org_folder_path: None,
        }
    }

    fn mark(ms: i64) -> CueMark {
        CueMark {
            ms,
            end_ms: None,
            kind: 0,
        }
    }

    fn change<'a>(r: &'a PlanReview, label: &str) -> &'a Change {
        r.changes.iter().find(|c| c.label == label).unwrap()
    }

    #[test]
    fn kept_cues_join_the_new_ones_in_time_order() {
        let plan = Plan::stub(
            header("s", 12800, 105, 1),
            header("d", 12000, 105, 2),
            &[(1500, -1, 1)],
            false,
        );
        let r = review(&plan, vec![mark(3000), mark(1000)], "Am", "Cm");
        let after: Vec<i64> = r.cues_after.iter().map(|c| c.ms).collect();
        assert_eq!(after, [1000, 1500, 3000]);
        assert_eq!(change(&r, "Cues").after, "3");
        assert_eq!(change(&r, "BPM").before, "120.00");
        assert_eq!(change(&r, "BPM").after, "128.00");
        assert!(!change(&r, "Length").changed());
    }

    #[test]
    fn replacing_drops_the_existing_cues() {
        let plan = Plan::stub(
            header("s", 12800, 105, 1),
            header("d", 12800, 105, 2),
            &[(1500, 9000, 2)],
            true,
        );
        let r = review(&plan, vec![mark(3000), mark(1000)], "Am", "Cm");
        assert_eq!(r.cues_after.len(), 1);
        assert_eq!(r.cues_after[0].end_ms, Some(9000));
        assert_eq!(
            (
                change(&r, "Cues").before.as_str(),
                change(&r, "Cues").after.as_str()
            ),
            ("2", "1")
        );
    }

    #[test]
    fn the_key_and_lock_follow_what_the_plan_sets() {
        let mut plan = Plan::stub(
            header("s", 12800, 105, 1),
            header("d", 12800, 105, 0),
            &[],
            false,
        );
        let r = review(&plan, vec![], "Am", "Cm");
        assert!(
            !change(&r, "Key").changed(),
            "no key id in the plan keeps the destination's"
        );
        plan.set_key_id = Some("7".into());
        plan.set_analysed = 105 | LOCKED;
        let r = review(&plan, vec![], "Am", "Cm");
        assert_eq!(change(&r, "Key").after, "Am");
        assert_eq!(
            (
                change(&r, "Locked").before.as_str(),
                change(&r, "Locked").after.as_str()
            ),
            ("no", "yes")
        );
    }
}
