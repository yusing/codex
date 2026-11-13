//! One app-server launch owns the family's counters, including opaque non-Codex actors.
//! Admission and routing belong to the embedding host; no history is read or persisted here.
use codex_protocol::feature_use::{FeatureUseCounts, FeatureUseEvent, FeatureUseFact};
use std::collections::{BTreeMap, HashSet};
use std::mem::{Discriminant, discriminant};

#[derive(Default)]
pub(crate) struct FeatureUse {
    counts: BTreeMap<String, FeatureUseCounts>,
    receipts: HashSet<(String, String, Discriminant<FeatureUseEvent>)>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_facts_count_once_for_the_initiating_actor() {
        let mut features = FeatureUse::default();
        let events = [
            FeatureUseEvent::ResetCompleted,
            FeatureUseEvent::SelfResetRequested { preload: true },
            FeatureUseEvent::SelfResetCompleted,
            FeatureUseEvent::SelfResetFailed,
            FeatureUseEvent::ChildAdmitted {
                preload: true,
                sources: true,
            },
            FeatureUseEvent::PreloadRejected,
            FeatureUseEvent::FinalDelivered { changes: true },
            FeatureUseEvent::MessageAccepted {
                sources: true,
                btw: true,
            },
            FeatureUseEvent::SideQuestionRejected,
            FeatureUseEvent::SideQuestionFailed,
            FeatureUseEvent::WaitRegistered {
                all: true,
                progress: true,
            },
            FeatureUseEvent::ProgressDelivered,
            FeatureUseEvent::AssignmentDelivered { sources: true },
        ];
        for (operation, event) in events.into_iter().enumerate() {
            let fact = FeatureUseFact {
                thread_id: "actor".into(),
                operation: operation.to_string(),
                event,
            };
            features.record(fact.clone());
            features.record(fact);
        }
        // Abort cleanup cannot turn an already committed completion into failure.
        features.record(FeatureUseFact {
            thread_id: "actor".into(),
            operation: "2".into(),
            event: FeatureUseEvent::SelfResetFailed,
        });
        let expected = FeatureUseCounts {
            compactions: 1,
            self_compactions: 1,
            compaction_preloads: 1,
            spawns: 1,
            spawn_preloads: 1,
            preload_rejections: 1,
            finals: 1,
            completion_changes: 1,
            messages: 1,
            side_questions: 1,
            side_question_rejections: 1,
            side_question_failures: 1,
            waits: 1,
            all_waits: 1,
            progress_waits: 1,
            progress_yields: 1,
            assignment_sources: 2,
            message_sources: 1,
            self_compactions_completed: 1,
            self_compaction_failures: 1,
        };
        for _ in 0..2 {
            let snapshot = features.read(vec!["actor".into(), "recipient".into()]);
            assert_eq!(snapshot["actor"], expected);
            assert_eq!(snapshot["recipient"], FeatureUseCounts::default());
        }
        // A new native launch has no counters to replay from previous sessions.
        assert_eq!(
            FeatureUse::default().read(vec!["actor".into()])["actor"],
            FeatureUseCounts::default()
        );
    }
}

impl FeatureUse {
    pub(crate) fn record(&mut self, fact: FeatureUseFact) {
        // A late abort can repeat a committed reset with a canceled disposition.
        // Self-reset completion and failure share one terminal receipt; BTW's
        // independent dispatch/rejection/failure facts still keep separate keys.
        let kind = match &fact.event {
            FeatureUseEvent::SelfResetCompleted | FeatureUseEvent::SelfResetFailed => {
                discriminant(&FeatureUseEvent::SelfResetCompleted)
            }
            event => discriminant(event),
        };
        if !self
            .receipts
            .insert((fact.thread_id.clone(), fact.operation, kind))
        {
            return;
        }
        let counts = self.counts.entry(fact.thread_id).or_default();
        match fact.event {
            FeatureUseEvent::ResetCompleted => counts.compactions += 1,
            FeatureUseEvent::SelfResetRequested { preload } => {
                counts.self_compactions += 1;
                counts.compaction_preloads += u64::from(preload);
            }
            FeatureUseEvent::SelfResetCompleted => counts.self_compactions_completed += 1,
            FeatureUseEvent::SelfResetFailed => counts.self_compaction_failures += 1,
            FeatureUseEvent::ChildAdmitted { preload, sources } => {
                counts.spawns += 1;
                counts.spawn_preloads += u64::from(preload);
                counts.assignment_sources += u64::from(sources);
            }
            FeatureUseEvent::PreloadRejected => counts.preload_rejections += 1,
            FeatureUseEvent::FinalDelivered { changes } => {
                counts.finals += 1;
                counts.completion_changes += u64::from(changes);
            }
            FeatureUseEvent::MessageAccepted { sources, btw } => {
                counts.messages += 1;
                counts.message_sources += u64::from(sources);
                counts.side_questions += u64::from(btw);
            }
            FeatureUseEvent::SideQuestionRejected => counts.side_question_rejections += 1,
            FeatureUseEvent::SideQuestionFailed => counts.side_question_failures += 1,
            FeatureUseEvent::WaitRegistered { all, progress } => {
                counts.waits += 1;
                counts.all_waits += u64::from(all);
                counts.progress_waits += u64::from(progress);
            }
            FeatureUseEvent::ProgressDelivered => counts.progress_yields += 1,
            FeatureUseEvent::AssignmentDelivered { sources } => {
                counts.assignment_sources += u64::from(sources)
            }
        }
    }

    pub(crate) fn read(&self, threads: Vec<String>) -> BTreeMap<String, FeatureUseCounts> {
        threads
            .into_iter()
            .map(|thread| {
                let counts = self.counts.get(&thread).cloned().unwrap_or_default();
                (thread, counts)
            })
            .collect()
    }
}
