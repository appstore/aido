use super::*;

#[test]
fn iso_dates_match_calendar() {
    assert_eq!(civil_from_days(0), (1970, 1, 1));
    assert_eq!(civil_from_days(20705), (2026, 9, 9));
}

fn pending_with_chain() -> PendingRun {
    PendingRun {
        run_id: "20260919-120000.000".into(),
        task: Some("summarize|translate".into()),
        created_at: "2026-09-19T12:00:00Z".into(),
        summary: RunSummary::default(),
        record_history: true,
        chain_artifacts: vec![Artifact {
            id: "summarize".into(),
            kind: MediaKind::Text,
            mime: "text/plain".into(),
            format: "text".into(),
            bytes: b"stage one".to_vec(),
            provenance: crate::domain::Provenance::Request { index: 0 },
        }],
        chain_stages: vec![RunSummary {
            task: Some("summarize".into()),
            ..Default::default()
        }],
    }
}

#[test]
fn cancelled_record_keeps_completed_chain_stages() {
    let record = cancelled_record(&pending_with_chain(), CancelReason::CtrlC);
    assert_eq!(record.generation, GenerationStatus::Cancelled);
    // The paid-for upstream artifact travels with its bytes, and the
    // stage list describes the run; the interrupted final stage owns
    // none of it (last_stage_len 0 — cancelled never redelivers).
    assert_eq!(record.artifacts.len(), 1);
    assert_eq!(record.artifacts[0].text(), Some("stage one"));
    assert_eq!(record.stages.len(), 1);
    assert_eq!(record.stages[0].task.as_deref(), Some("summarize"));
    assert_eq!(record.last_stage_len, 0);
    assert!(
        record.warnings[0].contains("kept in this record"),
        "{:?}",
        record.warnings
    );
}

#[test]
fn cancelled_record_without_chain_progress_keeps_the_bare_shape() {
    let pending = PendingRun {
        chain_artifacts: Vec::new(),
        chain_stages: Vec::new(),
        ..pending_with_chain()
    };
    let record = cancelled_record(&pending, CancelReason::CtrlC);
    assert_eq!(record.generation, GenerationStatus::Cancelled);
    assert!(record.artifacts.is_empty() && record.stages.is_empty());
    assert_eq!(
        record.warnings,
        vec![CancelReason::CtrlC.history_warning().to_string()]
    );
}
