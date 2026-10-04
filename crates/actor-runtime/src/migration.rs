//! Actor migration and cloning.
//!
//! v1 runs everything on one node, but the pipeline is the real one:
//! Checkpoint -> Snapshot -> Transfer -> Restore -> Replay -> Resume.
//! Transfer is an interface with a local implementation, so replacing LocalTransfer with a
//! network transport later does not touch the runtime.

use crate::actor::ActorFactory;
use crate::checkpoint::CheckpointStore;
use crate::runtime::ActorRuntime;
use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{Checkpoint, EventKind, MigrationReport, NewEvent};
use agentos_core::state::{MigrationState, StateMachine};
use agentos_core::{ActorId, CheckpointId, SessionId, WorkerId};
use async_trait::async_trait;
use std::sync::Arc;

/// Where a checkpoint should land.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct TransferTarget {
    pub worker_id: Option<WorkerId>,
    /// Reserved: the network endpoint of the target worker.
    pub endpoint: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct TransferReceipt {
    pub checkpoint_id: CheckpointId,
    pub target: TransferTarget,
    pub bytes: u64,
    pub duration_ms: u64,
    pub transport: String,
}

/// The reserved seam for cross-node transfer.
#[async_trait]
pub trait ActorTransfer: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    async fn transfer(&self, checkpoint: &Checkpoint, target: &TransferTarget) -> Result<TransferReceipt>;
    /// Reserved: pull a checkpoint from another node during recovery.
    async fn fetch(&self, _id: &CheckpointId, _source: &TransferTarget) -> Result<Option<Checkpoint>> {
        Ok(None)
    }
}

/// Local transfer: the checkpoint is already in the shared store, so the "transfer" is a
/// durable acknowledgement. This is what makes single-node migration meaningful today and
/// multi-node migration a drop-in later.
pub struct LocalTransfer {
    checkpoints: Arc<dyn CheckpointStore>,
}

impl LocalTransfer {
    pub fn new(checkpoints: Arc<dyn CheckpointStore>) -> Self {
        Self { checkpoints }
    }
}

#[async_trait]
impl ActorTransfer for LocalTransfer {
    fn name(&self) -> &'static str {
        "local"
    }

    async fn transfer(&self, checkpoint: &Checkpoint, target: &TransferTarget) -> Result<TransferReceipt> {
        let started = agentos_core::now_ms();
        let stored = self.checkpoints.load(&checkpoint.meta.id).await?;
        if stored.is_none() {
            return Err(RuntimeError::migration(format!(
                "checkpoint {} was not durably stored before transfer",
                checkpoint.meta.id
            )));
        }
        if let Some(cp) = stored {
            if cp.meta.state_hash != checkpoint.meta.state_hash {
                return Err(RuntimeError::migration("checkpoint payload hash mismatch"));
            }
        }
        Ok(TransferReceipt {
            checkpoint_id: checkpoint.meta.id.clone(),
            target: target.clone(),
            bytes: checkpoint.meta.bytes,
            duration_ms: agentos_core::now_ms().saturating_sub(started),
            transport: self.name().to_string(),
        })
    }
}

/// Runs the migration pipeline and produces an auditable report.
pub struct MigrationCoordinator {
    runtime: Arc<ActorRuntime>,
    transfer: Arc<dyn ActorTransfer>,
}

impl MigrationCoordinator {
    pub fn new(runtime: Arc<ActorRuntime>, transfer: Arc<dyn ActorTransfer>) -> Self {
        Self { runtime, transfer }
    }

    pub fn transfer_name(&self) -> &'static str {
        self.transfer.name()
    }

    /// Full pipeline. Every stage emits an event, so a failed migration is diagnosable from the
    /// event log alone.
    pub async fn migrate(
        &self,
        actor_id: &ActorId,
        factory: Arc<dyn ActorFactory>,
        target: TransferTarget,
    ) -> Result<MigrationReport> {
        let started = agentos_core::now_ms();
        let handle = self
            .runtime
            .lookup(actor_id)
            .ok_or_else(|| RuntimeError::not_found(format!("actor {actor_id} is not running")))?;
        let session_id = handle.session_id.clone();
        let mut stage = MigrationState::Idle;

        let mut report = MigrationReport {
            actor_id: actor_id.clone(),
            session_id: session_id.clone(),
            from_worker: None,
            to_worker: target.worker_id.clone(),
            state: MigrationState::Idle,
            checkpoint_id: None,
            replayed_events: 0,
            duration_ms: 0,
            error: None,
        };

        macro_rules! stage_to {
            ($next:expr) => {
                stage = stage.transition($next).map_err(|e| {
                    RuntimeError::migration(format!("migration pipeline error: {e}"))
                })?;
                let _ = self
                    .runtime
                    .bus()
                    .publish(
                        NewEvent::new(EventKind::ActorMigrated, format!("migration stage {stage}"))
                            .actor(actor_id.clone())
                            .session(session_id.clone())
                            .payload(serde_json::json!({ "stage": stage.as_str() })),
                    )
                    .await;
            };
        }

        let outcome: Result<()> = async {
            stage_to!(MigrationState::Checkpointing);
            handle.transition(agentos_core::state::ActorState::Migrating)?;
            let checkpoint = self.runtime.checkpoint(actor_id).await?;
            report.checkpoint_id = Some(checkpoint.meta.id.clone());

            stage_to!(MigrationState::Snapshotting);
            stage_to!(MigrationState::Transferring);
            let receipt = self.transfer.transfer(&checkpoint, &target).await?;
            tracing::info!(
                actor = %actor_id,
                transport = %receipt.transport,
                bytes = receipt.bytes,
                "checkpoint transferred"
            );

            stage_to!(MigrationState::Restoring);
            let (_restored, replayed) = self.runtime.restore(checkpoint, factory).await?;

            stage_to!(MigrationState::Replaying);
            report.replayed_events = replayed;

            stage_to!(MigrationState::Completed);
            Ok(())
        }
        .await;

        match outcome {
            Ok(()) => {
                report.state = MigrationState::Completed;
                report.duration_ms = agentos_core::now_ms().saturating_sub(started);
                Ok(report)
            }
            Err(e) => {
                report.state = MigrationState::Failed;
                report.error = Some(e.to_string());
                report.duration_ms = agentos_core::now_ms().saturating_sub(started);
                let _ = self
                    .runtime
                    .bus()
                    .publish(
                        NewEvent::new(EventKind::ActorMigrated, "migration failed")
                            .error()
                            .actor(actor_id.clone())
                            .session(session_id.clone())
                            .payload(serde_json::json!({ "error": e.to_string() })),
                    )
                    .await;
                Err(RuntimeError::migration(format!("migration failed: {e}")))
            }
        }
    }

    /// Clone an actor: same state, new identity and new session. Built on the same checkpoint
    /// primitive, so a clone never copies a process.
    pub async fn clone_actor(
        &self,
        source: &ActorId,
        factory: Arc<dyn ActorFactory>,
        target_session: SessionId,
        new_actor: ActorId,
    ) -> Result<ActorId> {
        let checkpoint = self.runtime.checkpoint(source).await?;
        let init = crate::actor::ActorInit {
            actor_id: new_actor.clone(),
            session_id: target_session.clone(),
            generation: 0,
            params: serde_json::Value::Null,
        };
        let actor = factory.create(&init)?;
        let handle = self.runtime.spawn_boxed(init, actor).await?;
        handle.restore(checkpoint.state.clone(), 0).await?;
        self.runtime
            .bus()
            .publish(
                NewEvent::new(EventKind::ActorCloned, "actor cloned from checkpoint")
                    .actor(new_actor.clone())
                    .session(target_session)
                    .payload(serde_json::json!({ "source": source.as_str() })),
            )
            .await?;
        Ok(new_actor)
    }
}
