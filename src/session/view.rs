//! Text every frontend shows the same way, tagged with a tone rather than a
//! colour so each one can style it natively.

use crate::pending::{Entry, State};

use super::queue::EntryWork;
use super::{ConfirmKind, NoticeAction, Session};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    Plain,
    Strong,
    /// Something is happening, or the next step is ready to take.
    Active,
    Warn,
    Good,
    Bad,
    Muted,
}

/// How a stored state reads on the row, and the step that moves it along.
///
/// `has_row` is why this takes three arguments: the store keeps an entry at
/// `AwaitingImport` right up until the transfer lands, so a row created seconds
/// ago still claimed to be waiting for one — which read as the import having
/// silently done nothing.
pub fn stored_state_label(
    state: State,
    verdict: Option<&str>,
    has_row: bool,
) -> (String, Tone, Option<NoticeAction>) {
    let (label, tone, action) = match state {
        State::AwaitingImport if has_row => {
            ("imported", Tone::Active, Some(NoticeAction::CheckDownloads))
        }
        State::AwaitingImport => (
            "awaiting import",
            Tone::Warn,
            Some(NoticeAction::ImportDownloads),
        ),
        State::Matched => ("ready to apply", Tone::Good, None),
        State::Applied => ("applied", Tone::Good, None),
        State::Rejected => ("rejected", Tone::Bad, None),
        State::Expired => ("expired", Tone::Muted, None),
        State::Cancelled => ("cancelled", Tone::Muted, None),
    };
    // The verdict is why an entry is rejected or ready; hiding it would leave
    // the user guessing at the one thing that decided the outcome.
    let verdict = verdict
        .filter(|v| !v.is_empty())
        .map(|v| format!(" — {v}"))
        .unwrap_or_default();
    (format!("{label}{verdict}"), tone, action)
}

/// What a queue entry is waiting on, or what it decided.
pub fn entry_state(session: &Session, entry: &Entry) -> (String, Tone, Option<NoticeAction>) {
    if let Some(work) = session.queue.work_for(entry.id) {
        return match work {
            EntryWork::Failed(why) => (format!("failed — {why}"), Tone::Bad, None),
            EntryWork::Declined => (
                work.tag().to_string(),
                Tone::Muted,
                Some(NoticeAction::ImportDownloads),
            ),
            EntryWork::Ready { .. } | EntryWork::Planned(_) => {
                (work.tag().to_string(), Tone::Good, None)
            }
            _ => (work.tag().to_string(), Tone::Active, None),
        };
    }
    stored_state_label(
        entry.state,
        entry.verdict.as_deref(),
        session.queue.has_row(entry.id),
    )
}

/// The open confirmation: what it asks, and every write a yes makes.
pub struct ConfirmView {
    pub kind: ConfirmKind,
    pub title: String,
    pub accept: String,
    pub lines: Vec<(Tone, String)>,
}

impl Session {
    /// The newest open confirmation.
    pub fn confirm_view(&self) -> Option<ConfirmView> {
        self.confirm_view_for(self.confirm()?)
    }

    pub fn confirm_view_for(&self, kind: ConfirmKind) -> Option<ConfirmView> {
        if !self.confirmations.is_open(kind) {
            return None;
        }
        let mut lines = Vec::new();
        let (title, accept) = match kind {
            ConfirmKind::Transfer => {
                let batch = self.confirmations.transfer()?;
                if let Some(p) = batch.plans.first() {
                    let title = p.src.title.as_deref().unwrap_or("?");
                    let artist = p.src.artist.as_deref().unwrap_or("?");
                    lines.push((
                        Tone::Plain,
                        format!("src: \"{title}\" — {artist}   ({})", p.src.id),
                    ));
                }
                for p in &batch.plans {
                    let title = p.dst.title.as_deref().unwrap_or("?");
                    let artist = p.dst.artist.as_deref().unwrap_or("?");
                    lines.push((
                        Tone::Strong,
                        format!("→ \"{title}\" — {artist}   ({})", p.dst.id),
                    ));
                    let bpm = match (p.set_bpm, p.dst.bpm) {
                        (Some(s), Some(d)) if s != d => {
                            format!("BPM {:.2} → {:.2}", d as f64 / 100.0, s as f64 / 100.0)
                        }
                        _ => "BPM ≈".into(),
                    };
                    let len = if p.set_length.is_some() {
                        "len ≈"
                    } else {
                        "len skipped"
                    };
                    lines.push((
                        Tone::Plain,
                        format!(
                            "    {bpm}   cues {} → {}   {len}",
                            p.dst.cue_count, p.src.cue_count
                        ),
                    ));
                    for w in &p.warnings {
                        lines.push((Tone::Warn, format!("    warning: {w}")));
                    }
                }
                if !batch.failures.is_empty() {
                    lines.push((Tone::Warn, "Skipped:".into()));
                    for (dst_id, err) in &batch.failures {
                        lines.push((Tone::Plain, format!("  {dst_id}: {err}")));
                    }
                }
                (
                    format!("Apply analysis to {} track(s)?", batch.plans.len()),
                    "Apply".to_string(),
                )
            }
            ConfirmKind::ImportRows => {
                let batch = self.confirmations.import_rows()?;
                for new in &batch.rows {
                    lines.push((Tone::Good, format!("{} — {}", new.title, new.file_name)));
                    for detail in crate::import::render(new).lines() {
                        lines.push((Tone::Muted, format!("  {detail}")));
                    }
                }
                (
                    format!(
                        "Create {} new row(s) in your rekordbox collection?",
                        batch.rows.len()
                    ),
                    "Insert".to_string(),
                )
            }
            ConfirmKind::ForceApply => {
                let batch = self.confirmations.force_apply()?;
                lines.push((Tone::Bad, batch.verdict.summary()));
                lines.push((
                    Tone::Warn,
                    "Nothing proves these are the same recording at the same alignment. \
                     Cues and the beat grid may land anywhere. master.db is backed up first."
                        .into(),
                ));
                lines.extend(
                    batch
                        .plan
                        .render()
                        .lines()
                        .map(|l| (Tone::Plain, l.to_string())),
                );
                for w in &batch.plan.warnings {
                    lines.push((Tone::Warn, format!("warning: {w}")));
                }
                (
                    "Transfer without the fingerprint check?".to_string(),
                    "Apply Unverified".to_string(),
                )
            }
            ConfirmKind::ClearQueue => {
                lines.push((
                    Tone::Plain,
                    "This cannot be undone. The downloaded files stay on disk, and nothing in \
                     your rekordbox collection is touched — only the queue is emptied."
                        .into(),
                ));
                (
                    format!(
                        "Forget all {} queued transfer(s)?",
                        self.queue.entries.len()
                    ),
                    "Clear Queue".to_string(),
                )
            }
        };
        Some(ConfirmView {
            kind,
            title,
            accept,
            lines,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_queue_state_reads_as_something() {
        // All six, so a state added later cannot render as a blank row.
        for state in [
            State::AwaitingImport,
            State::Matched,
            State::Applied,
            State::Rejected,
            State::Expired,
            State::Cancelled,
        ] {
            let (text, _, _) = stored_state_label(state, None, false);
            assert!(!text.is_empty(), "{state:?} renders as nothing");
        }
        // The verdict is appended when there is one, and never as a bare dash.
        let (with, _, _) =
            stored_state_label(State::Rejected, Some("duration differs by 9s"), false);
        assert!(with.contains("rejected") && with.contains("9s"));
        let (without, _, _) = stored_state_label(State::Rejected, Some(""), false);
        assert_eq!(without, "rejected");
    }

    #[test]
    fn an_imported_row_stops_claiming_to_be_awaiting_import() {
        // The bug: the store holds an entry at AwaitingImport until the transfer
        // lands, so a row created seconds ago still read "awaiting import" and
        // the import looked like it had done nothing.
        let (waiting, _, import) = stored_state_label(State::AwaitingImport, None, false);
        let (imported, _, check) = stored_state_label(State::AwaitingImport, None, true);
        assert_ne!(waiting, imported);
        assert!(waiting.contains("awaiting"));
        assert!(
            !imported.contains("awaiting"),
            "still says awaiting: {imported}"
        );
        // Both say what moves them along.
        assert_eq!(import, Some(NoticeAction::ImportDownloads));
        assert_eq!(check, Some(NoticeAction::CheckDownloads));
    }
}
