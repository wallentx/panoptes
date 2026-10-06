//! Meaningful work milestones, shared by serial and parallel indexing workers.
use serde::{Deserialize, Serialize};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub stage: String,
    pub completed: u64,
    pub total: Option<u64>,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timing: Option<StageTiming>,
}

/// Worker-side wall time, including SQL within the named phase. VM steps are
/// approximate work counts, never a substitute for SQL elapsed time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct StageTiming {
    pub stage: String,
    pub elapsed_micros: u64,
    pub sqlite_vm_steps: u64,
}

impl Progress {
    pub fn message(&self) -> String {
        let counts = match self.total {
            Some(total) => format!(" {}/{total}", self.completed),
            None if self.completed > 0 => format!(" {}", self.completed),
            None => String::new(),
        };
        let detail = if self.detail.is_empty() {
            String::new()
        } else {
            format!(": {}", self.detail)
        };
        format!("{}{counts}{detail}", self.stage)
    }
}

struct Reporter {
    sink: Box<dyn FnMut(Progress) + Send>,
    last_seen: Option<Progress>,
    last_sent: Instant,
    phase: Option<(String, Instant, u64)>,
    sqlite_steps: u64,
}
static REPORTER: OnceLock<Mutex<Reporter>> = OnceLock::new();

/// One operation per isolated MCP worker, or one interactive CLI build process.
pub fn install(sink: impl FnMut(Progress) + Send + 'static) {
    let _ = REPORTER.set(Mutex::new(Reporter {
        sink: Box::new(sink),
        last_seen: None,
        last_sent: Instant::now(),
        phase: None,
        sqlite_steps: 0,
    }));
}

pub fn enabled() -> bool {
    REPORTER.get().is_some()
}

pub fn report(stage: &str, completed: usize, total: Option<usize>, detail: &str) {
    if !enabled() {
        return;
    }
    emit(Progress {
        stage: stage.into(),
        completed: completed as u64,
        total: total.map(|n| n as u64),
        detail: detail.into(),
        timing: None,
    });
}

/// SQLite invokes this only after executing work, never while its busy handler
/// waits for a lock. The counter is approximate VM instructions, not rows or %.
pub fn database_steps(completed: u64) {
    emit(Progress {
        stage: "Processing database".into(),
        completed,
        total: None,
        detail: "approximate SQLite VM steps".into(),
        timing: None,
    });
}

/// Emit the last phase once, including normally returned errors, before the
/// worker result. A killed worker cannot flush and must not fabricate completion.
pub fn finish() {
    let Some(reporter) = REPORTER.get() else {
        return;
    };
    let Ok(mut reporter) = reporter.lock() else {
        return;
    };
    let Some((stage, started, prior_steps)) = reporter.phase.take() else {
        return;
    };
    let timing = StageTiming {
        stage: stage.clone(),
        elapsed_micros: started.elapsed().as_micros().min(u64::MAX as u128) as u64,
        sqlite_vm_steps: reporter.sqlite_steps.saturating_sub(prior_steps),
    };
    (reporter.sink)(Progress {
        stage,
        completed: 0,
        total: None,
        detail: "phase timing".into(),
        timing: Some(timing),
    });
}

fn emit(mut progress: Progress) {
    let Some(reporter) = REPORTER.get() else {
        return;
    };
    let Ok(mut reporter) = reporter.lock() else {
        return;
    };
    if reporter.last_seen.as_ref() == Some(&progress) {
        return;
    }
    let stage_changed = reporter
        .last_seen
        .as_ref()
        .is_none_or(|previous| previous.stage != progress.stage);
    reporter.last_seen = Some(progress.clone());
    if progress.stage == "Processing database" {
        reporter.sqlite_steps = progress.completed;
    } else if reporter
        .phase
        .as_ref()
        .is_none_or(|(stage, _, _)| stage != &progress.stage)
    {
        let steps = reporter.sqlite_steps;
        if let Some((stage, started, prior_steps)) =
            reporter
                .phase
                .replace((progress.stage.clone(), Instant::now(), steps))
        {
            progress.timing = Some(StageTiming {
                stage,
                elapsed_micros: started.elapsed().as_micros().min(u64::MAX as u128) as u64,
                sqlite_vm_steps: steps.saturating_sub(prior_steps),
            });
        }
    }
    // Stage transitions and completion are useful immediately; rate-limit counts
    // within a stage. Repeating a waiting status never constitutes progress.
    if !stage_changed
        && reporter.last_sent.elapsed() < Duration::from_millis(100)
        && progress.total != Some(progress.completed)
    {
        return;
    }
    (reporter.sink)(progress);
    reporter.last_sent = Instant::now();
}
