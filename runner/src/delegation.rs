//! A run's supervised delegation and execution lease.
use crate::db::{Database, Run};
use host::delegation::{Delegation, Reply};
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
}
