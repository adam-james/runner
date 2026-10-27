//! A run's supervised delegation, audit-trail reads and execution lease.
use crate::audit;
use crate::db::{Database, Run};
use host::delegation::{Delegation, Reply};
use host::trails::Trail;
use std::sync::Arc;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};

pub struct ExecutionContext {
    db: Database,
    run: Run,
    capacity: Arc<Semaphore>,
    lease: Mutex<Option<OwnedSemaphorePermit>>,
}

impl ExecutionContext {
    pub(crate) fn new(
        db: Database,
        run: Run,
        capacity: Arc<Semaphore>,
        permit: OwnedSemaphorePermit,
    ) -> Self {
        Self {
            db,
            run,
            capacity,
            lease: Mutex::new(Some(permit)),
        }
    }

    async fn collect(&self, id: &str) -> anyhow::Result<String> {
        let parent = self.run.id.clone();
        let child = id.to_owned();
        let initial = self
            .db
            .blocking({
                let parent = parent.clone();
                let child = child.clone();
                move |db| db.child_result(&parent, &child)
            })
            .await?;
        if let Some(result) = initial {
            return Ok(result.to_string());
        }
        self.db
            .blocking({
                let parent = parent.clone();
                let child = child.clone();
                move |db| {
                    db.event(
                        &parent,
                        serde_json::json!({"kind":"delegation_waiting","run_id":child}),
                    )
                }
            })
            .await?;
        tracing::info!(child = ?id, "waiting for delegated run");
        // Release execution capacity, but retain the runtime's session lock.
        self.lease.lock().await.take();
        let changes = self.db.changes(id);
        let result = async {
            loop {
                let notified = changes.notified();
                tokio::pin!(notified);
                notified.as_mut().enable();
                let result = self
                    .db
                    .blocking({
                        let parent = parent.clone();
                        let child = child.clone();
                        move |db| db.child_result(&parent, &child)
                    })
                    .await?;
                if let Some(result) = result {
                    break Ok::<_, anyhow::Error>(result);
                }
                notified.await;
            }
        }
        .await;
        // Reacquire even when a database operation failed; never resume guest work
        // without a permit. Cancellation drops this entire future instead.
        let permit = self.capacity.clone().acquire_owned().await?;
        *self.lease.lock().await = Some(permit);
        let result = result?;
        self.db.blocking({
            let parent = parent.clone(); let child = child.clone();
            let status = result["status"].clone();
            move |db| db.event(&parent, serde_json::json!({"kind":"delegation_finished","run_id":child,"status":status}))
        }).await?;
        tracing::info!(child = ?id, "collected delegated run");
        Ok(result.to_string())
    }
}

/// The stored agent versions `records` refer to.
fn versions_of(
    db: &Database,
    records: &[crate::db::Event],
) -> anyhow::Result<Vec<crate::db::AgentVersion>> {
    audit::versions(records)
        .iter()
        .filter_map(|id| db.agent_version(id).transpose())
        .collect()
}

impl Delegation for ExecutionContext {
    fn delegate(&self, agent: String, prompt: String, deadline_secs: Option<u64>) -> Reply<'_> {
        Box::pin(async move {
            let parent = self.run.id.clone();
            let child = self
                .db
                .blocking(move |db| db.delegate(&parent, &agent, &prompt, deadline_secs))
                .await
                .map_err(|error| error.to_string())?;
            tracing::info!(child = ?child.id, agent = ?child.agent, "delegated run");
            Ok(serde_json::json!({"run_id":child.id,"agent":child.agent}).to_string())
        })
    }

    fn wait(&self, run_id: String) -> Reply<'_> {
        Box::pin(async move {
            self.collect(&run_id)
                .await
                .map_err(|error| error.to_string())
        })
    }

    /// Never this run's own trail, or any run's in its delegation tree: a
    /// reviewer that could read the run it's part of could be steered by
    /// what that run's own tools did.
    fn audit_trail(&self, trail: Trail) -> Reply<'_> {
        Box::pin(async move {
            let root = self.run.root_run_id.clone();
            let own_session = self.run.session_id.clone();
            let review = self
                .db
                .blocking(move |db| {
                    Ok(match trail {
                        Trail::Run(id) => match db.run(&id)? {
                            None => Err("no run with that id".to_string()),
                            Some(run) if run.root_run_id == root => {
                                Err("that run is in this run's delegation tree, so it can't \
                                 review it"
                                    .to_string())
                            }
                            Some(_) => {
                                let records = db.audit(&id)?;
                                let versions = versions_of(&db, &records)?;
                                Ok(audit::review(&format!("run {id}"), &records, &versions))
                            }
                        },
                        Trail::Session(id) => {
                            if id == own_session || db.session_in_tree(&id, &root)? {
                                Err("that session has a run in this run's delegation tree, \
                                     so it can't review it"
                                    .to_string())
                            } else {
                                match db.session_audit(&id)? {
                                    None => Err("no session with that id".to_string()),
                                    Some(records) => {
                                        let versions = versions_of(&db, &records)?;
                                        Ok(audit::review(
                                            &format!("session {id}"),
                                            &records,
                                            &versions,
                                        ))
                                    }
                                }
                            }
                        }
                    })
                })
                .await
                .map_err(|error| {
                    tracing::warn!(%error, "could not read an audit trail");
                    "could not read the audit trail".to_string()
                })??;
            tracing::info!("read an audit trail for review");
            Ok(review)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn record(seq: u64, action: &str) -> serde_json::Value {
        json!({"kind":"audit", "v":1, "seq":seq, "ts_ms":0, "action":action})
    }

    #[tokio::test]
    async fn audit_trails_are_readable_except_this_runs_delegation_tree() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(&dir.path().join("db")).unwrap();
        let other = db
            .submit(None, "default".into(), "other".into(), 30)
            .unwrap();
        db.claim().unwrap().unwrap();
        for (seq, action) in [(1, "turn.started"), (2, "turn.finished")] {
            db.audit_event(&other.id, record(seq, action)).unwrap();
        }
        db.submit(None, "run_auditor".into(), "review".into(), 30)
            .unwrap();
        let reviewer = db.claim().unwrap().unwrap();
        let child = db.delegate(&reviewer.id, "default", "child", None).unwrap();

        let capacity = Arc::new(Semaphore::new(1));
        let permit = capacity.clone().try_acquire_owned().unwrap();
        let context = ExecutionContext::new(db.clone(), reviewer.clone(), capacity, permit);

        let review = context
            .audit_trail(Trail::Run(other.id.clone()))
            .await
            .unwrap();
        assert!(review.starts_with(&format!("Audit trail of run {}: 2 records", other.id)));
        let review = context
            .audit_trail(Trail::Session(other.session_id.clone()))
            .await
            .unwrap();
        assert!(
            review.contains("\nTrail problems:\n- the turn reports"),
            "{review}"
        );

        for trail in [
            Trail::Run(reviewer.id.clone()),
            Trail::Run(child.id.clone()),
            Trail::Session(reviewer.session_id.clone()),
            Trail::Session(child.session_id.clone()),
        ] {
            let error = context.audit_trail(trail.clone()).await.unwrap_err();
            assert!(error.contains("delegation tree"), "{trail:?}: {error}");
        }
        for trail in [Trail::Run("nope".into()), Trail::Session("nope".into())] {
            assert!(
                context
                    .audit_trail(trail)
                    .await
                    .unwrap_err()
                    .starts_with("no ")
            );
        }
    }
}
