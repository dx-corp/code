//! Pending observations live under the existing private outbox. One OS file
//! lease per producer prevents restart recovery from stealing a live turn.
//! A terminal update precedes promotion; retries retain the same event UUID.
//! This cannot account for events a process never delivered to its collector.
use super::*;
use std::collections::HashMap;

pub(crate) struct TurnJournal {
    outbox: PathBuf,
    directory: PathBuf,
    _lease: std::fs::File,
    records: HashMap<String, (PathBuf, Uuid)>,
}

impl TurnJournal {
    pub(crate) fn open() -> Option<Self> {
        if first_party_telemetry_disabled() {
            return None;
        }
        let journal = Self::open_at(first_party_outbox_dir())?;
        schedule_first_party_outbox_drain();
        Some(journal)
    }

    fn open_at(outbox: PathBuf) -> Option<Self> {
        let journals = outbox.join("pending");
        ensure_private_directory(&journals)?;
        Self::recover(&outbox, &journals);
        let directory = journals.join(Uuid::new_v4().to_string());
        ensure_private_directory(&directory)?;
        let lease = OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(directory.join("lease"))
            .ok()?;
        lease.try_lock().ok()?;
        Some(Self {
            outbox,
            directory,
            _lease: lease,
            records: HashMap::new(),
        })
    }

    /// A `flock` belongs to the open file description, and `fork` copies that
    /// description into the child. `O_CLOEXEC` closes the child's copy only at
    /// `exec`, so any process this host spawns -- a bash tool, an MCP server,
    /// an LSP, a subagent -- holds every open journal lease for the width of
    /// its own fork-to-exec window. During that window a dropped producer's
    /// lease still reports `WouldBlock`, and recovery used to read that as "a
    /// live producer owns this journal" and abandon the pending record. A probe
    /// against this API with eight spawning threads lost the record in 245 of
    /// 300 trials. The window is microseconds wide, so re-reading it over a
    /// bounded interval separates a transient inherited descriptor from a
    /// producer that is genuinely still running.
    ///
    /// The budget covers the whole recovery pass, not one journal. A lease a
    /// live producer holds never unlocks, and `recover` runs on the
    /// process-start path, so retrying several sibling journals in turn would
    /// charge startup for every Maestro process already running.
    const LEASE_RECLAIM_BUDGET: Duration = Duration::from_millis(60);
    const LEASE_RECLAIM_BACKOFF: Duration = Duration::from_millis(5);

    fn claim_lease(lease: &std::fs::File, deadline: Instant) -> bool {
        loop {
            if lease.try_lock().is_ok() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Self::LEASE_RECLAIM_BACKOFF);
        }
    }

    fn recover(outbox: &Path, journals: &Path) {
        let Ok(entries) = fs::read_dir(journals) else {
            return;
        };
        let deadline = Instant::now() + Self::LEASE_RECLAIM_BUDGET;
        for entry in entries.flatten() {
            if !entry.file_type().is_ok_and(|t| t.is_dir()) {
                continue;
            }
            let directory = entry.path();
            let lease = match OpenOptions::new()
                .read(true)
                .write(true)
                .open(directory.join("lease"))
            {
                Ok(lease) => lease,
                Err(error) => {
                    tracing::warn!(
                        journal = %directory.display(),
                        %error,
                        "telemetry journal lease could not be opened; pending records stay unsent"
                    );
                    continue;
                }
            };
            if !Self::claim_lease(&lease, deadline) {
                tracing::debug!(
                    journal = %directory.display(),
                    "telemetry journal lease is held; leaving it to its owner"
                );
                continue;
            }
            for path in outbox_paths(&directory) {
                if read_bounded_outbox_record(&path).is_none() {
                    tracing::warn!(
                        record = %path.display(),
                        "telemetry journal record is unreadable; leaving it in place"
                    );
                    continue;
                }
                if promote(outbox, &path).is_none() {
                    tracing::warn!(
                        record = %path.display(),
                        "telemetry journal record could not be promoted to the outbox"
                    );
                }
            }
            if outbox_paths(&directory).is_empty() {
                let _ = fs::remove_file(directory.join("lease"));
                let _ = fs::remove_dir(directory);
            }
        }
    }

    fn write(&mut self, event: &CanonicalTurnEvent) -> Option<PathBuf> {
        let scope = event.identity_scope.as_ref().filter(|s| s.is_complete())?;
        let mut cloud = canonical_cloud_projection(event)?;
        if !cloud.is_server_valid() {
            return None;
        }
        let (path, id) = self
            .records
            .entry(event.turn_id.clone())
            .or_insert_with(|| {
                let id = Uuid::new_v4();
                (
                    self.directory.join(format!(
                        "{:020}_{id}.json",
                        chrono::Utc::now().timestamp_micros()
                    )),
                    id,
                )
            });
        cloud.event_id = *id;
        cloud.delivery = Some(delivery_snapshot(&self.outbox, scope));
        let record = FirstPartyOutboxRecord {
            identity_scope: scope.clone(),
            event: cloud.into(),
        };
        let bytes = serde_json::to_vec(&record).ok()?;
        if bytes.len() > OUTBOX_MAX_EVENT_BYTES {
            return None;
        }
        let written = with_outbox_lock(&self.outbox, || {
            if record
                .event
                .experiment_assignment()
                .is_some_and(|a| !crate::experiments::permits(scope, a))
            {
                let _ = fs::remove_file(&*path);
                return None;
            }
            if crate::path_utils::atomic_private_write(path, &bytes).is_err() {
                let _ = delivery_accounting::update(&self.outbox, scope, |a| {
                    a.write_failures = a.write_failures.saturating_add(1);
                });
                return None;
            }
            Some(())
        });
        written?;
        Some(path.clone())
    }

    pub(crate) fn observe(&mut self, events: &[CanonicalTurnEvent]) {
        for event in events {
            if event.identity_scope.is_some() && self.write(event).is_none() {
                tracing::warn!("telemetry pending observation could not be persisted");
            }
        }
    }

    pub(crate) fn finish(&mut self, event: &CanonicalTurnEvent) {
        record_canonical_turn_event_inner(event, false);
        if let Some(path) = self.write(event) {
            if promote(&self.outbox, &path).is_some() {
                self.records.remove(&event.turn_id);
                schedule_first_party_outbox_drain();
            }
        } else if event.identity_scope.is_some() {
            tracing::warn!("telemetry terminal observation could not be persisted");
        }
    }
}

fn promote(outbox: &Path, path: &Path) -> Option<()> {
    with_outbox_lock(outbox, || {
        let destination = outbox.join(path.file_name()?);
        let record = read_bounded_outbox_record(path)?;
        if record
            .event
            .experiment_assignment()
            .is_some_and(|a| !crate::experiments::permits(&record.identity_scope, a))
        {
            let _ = fs::remove_file(path);
            return None;
        }
        let scope = record.identity_scope;
        fs::rename(path, &destination).ok()?;
        let _ =
            delivery_accounting::update(outbox, &scope, |a| a.queued = a.queued.saturating_add(1));
        trim_outbox_paths_to_capacity(outbox_paths(outbox), OUTBOX_CAPACITY, Some(&destination))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn experiments_withdrawal_prevents_pending_turn_records_from_reappearing() {
        let _guard = crate::config::test_process_env_lock();
        struct Restore(Vec<(&'static str, Option<std::ffi::OsString>)>);
        impl Drop for Restore {
            fn drop(&mut self) {
                for (key, value) in self.0.drain(..) {
                    match value {
                        Some(value) => std::env::set_var(key, value),
                        None => std::env::remove_var(key),
                    }
                }
            }
        }
        let keys = [
            "MAESTRO_HOME",
            "MAESTRO_TELEMETRY",
            "PLAYWRIGHT_TELEMETRY",
            "MAESTRO_INTERNAL_TELEMETRY_DISABLED",
            "EVALOPS_INTERNAL_TELEMETRY_DISABLED",
        ];
        let _restore = Restore(
            keys.iter()
                .map(|key| (*key, std::env::var_os(key)))
                .collect(),
        );
        let temp = tempfile::tempdir().unwrap();
        for key in keys {
            std::env::remove_var(key);
        }
        std::env::set_var("MAESTRO_HOME", temp.path());
        let consent = crate::experiments::set_enabled(true).unwrap();
        let mut event = super::super::tests::canonical_event(TurnStatus::Success);
        let scope = event.identity_scope.as_ref().unwrap().clone();
        let assignment = scope.experiment_assignment(&consent.installation_seed, consent.revision);
        event.diagnostics = Some(super::super::super::operation::OperationDiagnostics {
            completion_observed: true,
            experiment: Some(
                maestro_runtime_contracts::experiments::ExperimentObservation {
                    assignment: assignment.clone(),
                    locally_applied: true,
                    runtime_version: "test-1".into(),
                },
            ),
            ..Default::default()
        });
        let outbox = first_party_outbox_dir();
        let mut producer = TurnJournal::open_at(outbox.clone()).unwrap();
        let pending = producer.write(&event).expect("enrolled pending record");
        assert!(pending.exists());
        crate::experiments::set_enabled(false).unwrap();
        assert!(!pending.exists());
        assert!(producer.write(&event).is_none());
        producer.observe(&[event.clone()]);
        producer.finish(&event);
        assert!(!pending.exists());
        assert!(outbox_paths(&outbox).is_empty());
        crate::experiments::set_enabled(true).unwrap();
        assert!(
            !crate::experiments::permits(&scope, &assignment),
            "re-opt-in must not revive revoked records"
        );
        assert!(producer.write(&event).is_none());
    }

    #[test]
    fn recovery_reclaims_a_lease_a_forked_child_transiently_inherited() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        // Every process this host spawns inherits a copy of the open file
        // description behind each live journal lease, and holds the flock until
        // it reaches exec. Recovery must not read that as a live producer.
        let stop = Arc::new(AtomicBool::new(false));
        let spawners: Vec<_> = (0..4)
            .map(|_| {
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        let _ = std::process::Command::new("/bin/true").status();
                    }
                })
            })
            .collect();

        let mut abandoned = 0;
        for _ in 0..40 {
            let root = tempfile::tempdir().unwrap();
            let outbox = root.path().join("outbox");
            let mut producer = TurnJournal::open_at(outbox.clone()).unwrap();
            let event = super::super::tests::canonical_event(TurnStatus::Success);
            producer.write(&event).unwrap();
            drop(producer);
            TurnJournal::recover(&outbox, &outbox.join("pending"));
            if outbox_paths(&outbox).is_empty() {
                abandoned += 1;
            }
        }

        stop.store(true, Ordering::Relaxed);
        for spawner in spawners {
            let _ = spawner.join();
        }
        assert_eq!(
            abandoned, 0,
            "recovery abandoned pending records whose producer had already exited"
        );
    }

    #[test]
    fn recovery_preserves_a_terminal_update_before_promotion() {
        let root = tempfile::tempdir().unwrap();
        let outbox = root.path().join("outbox");
        let mut producer = TurnJournal::open_at(outbox.clone()).unwrap();
        let mut event = super::super::tests::canonical_event(TurnStatus::Success);
        event.diagnostics = Some(crate::telemetry::operation::OperationDiagnostics {
            completion_observed: false,
            ..Default::default()
        });
        let path = producer.write(&event).unwrap();
        let id = read_bounded_outbox_record(&path).unwrap().event.event_id();
        event.diagnostics.as_mut().unwrap().completion_observed = true;
        assert_eq!(producer.write(&event), Some(path));
        drop(producer);
        TurnJournal::recover(&outbox, &outbox.join("pending"));
        let paths = outbox_paths(&outbox);
        assert_eq!(paths.len(), 1);
        let record = read_bounded_outbox_record(&paths[0]).unwrap();
        assert_eq!(record.event.event_id(), id);
        let FirstPartyTelemetryEvent::Turn(turn) = record.event else {
            panic!("turn")
        };
        assert!(turn.diagnostics.unwrap().completion_observed);
        let scope = event.identity_scope.unwrap();
        assert_eq!(
            delivery_accounting::read(&outbox, &scope).unwrap().queued,
            1
        );
    }

    #[test]
    fn recovery_skips_live_producers_and_reuses_the_durable_event_id() {
        let root = tempfile::tempdir().unwrap();
        let outbox = root.path().join("outbox");
        let mut producer = TurnJournal::open_at(outbox.clone()).unwrap();
        let mut event = super::super::tests::canonical_event(TurnStatus::Error);
        event.diagnostics = Some(crate::telemetry::operation::OperationDiagnostics {
            completion_observed: false,
            ..Default::default()
        });
        let path = producer.write(&event).unwrap();
        let id = read_bounded_outbox_record(&path).unwrap().event.event_id();
        let other = TurnJournal::open_at(outbox.clone()).unwrap();
        assert!(
            outbox_paths(&outbox).is_empty(),
            "live producer retains ownership"
        );
        drop(producer);
        TurnJournal::recover(&outbox, &outbox.join("pending"));
        let paths = outbox_paths(&outbox);
        assert_eq!(paths.len(), 1);
        let record = read_bounded_outbox_record(&paths[0]).unwrap();
        assert_eq!(record.event.event_id(), id);
        let FirstPartyTelemetryEvent::Turn(turn) = record.event else {
            panic!("turn")
        };
        assert!(!turn.diagnostics.unwrap().completion_observed);
        assert_eq!(turn.reported_cost_usd, None);
        TurnJournal::recover(&outbox, &outbox.join("pending"));
        assert_eq!(outbox_paths(&outbox).len(), 1);
        drop(other);
    }
}
