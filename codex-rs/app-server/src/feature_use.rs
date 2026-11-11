//! One app-server launch owns the family's counters, including opaque non-Codex actors.
//! Admission and routing belong to the embedding host; no history is read or persisted here.
use codex_app_server_protocol::{FeatureUseCounts, FeatureUseEvent, FeatureUseFact};
use std::collections::{BTreeMap, HashSet};
use std::mem::{Discriminant, discriminant};

#[derive(Default)]
pub(crate) struct FeatureUse {
    counts: BTreeMap<String, FeatureUseCounts>,
    receipts: HashSet<(String, String, Discriminant<FeatureUseEvent>)>,
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
