//! Actor abstractions: typed interface, erased interface, mailbox commands and handles.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{ActorRecord, EventRecord};
use agentos_core::state::{ActorState, StateMachine};
use agentos_core::telemetry::Correlation;
use agentos_core::{ActorId, SessionId};
use async_trait::async_trait;
use parking_lot::RwLock;
use std::any::Any;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

/// Everything an actor needs to know about the message it is currently handling.
#[derive(Debug, Clone)]
pub struct ActorContext {
    pub actor_id: ActorId,
    pub session_id: SessionId,
    pub generation: u64,
    /// Monotonic per-actor sequence of the message being handled. Used for replay bookkeeping.
    pub seq: u64,
    pub correlation: Correlation,
    pub cancellation: CancellationToken,
}

impl ActorContext {
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

/// Identity handed to a factory when an actor is (re)created.
#[derive(Debug, Clone)]
pub struct ActorInit {
    pub actor_id: ActorId,
    pub session_id: SessionId,
    pub generation: u64,
    /// Free-form creation parameters (for example the model name for a session actor).
    pub params: serde_json::Value,
}

/// Creates actor instances. Kept as a trait so the runtime never needs to know concrete actors,
/// which is also what makes restore and migration possible.
pub trait ActorFactory: Send + Sync + 'static {
    fn kind(&self) -> &'static str;
    fn create(&self, init: &ActorInit) -> Result<Box<dyn ErasedActor>>;
}

/// Type-erased actor: the only thing the runtime scheduler knows how to drive.
#[async_trait]
pub trait ErasedActor: Send + 'static {
    fn kind(&self) -> &'static str;
    fn session_id(&self) -> SessionId;
    /// Serialize everything that must survive a restart, a clone or a migration.
    fn snapshot(&self) -> serde_json::Value;
    fn restore(&mut self, state: serde_json::Value) -> Result<()>;
    async fn handle(&mut self, message: Box<dyn Any + Send>, ctx: &ActorContext) -> Result<serde_json::Value>;
    async fn on_start(&mut self, _ctx: &ActorContext) -> Result<()> {
        Ok(())
    }
    async fn on_stop(&mut self, _ctx: &ActorContext) -> Result<()> {
        Ok(())
    }
    /// Replay one historical event onto the restored state.
    async fn on_replay(&mut self, _event: &EventRecord) -> Result<()> {
        Ok(())
    }
}

/// The interface application code implements. Deliberately small.
#[async_trait]
pub trait Actor: Send + 'static {
    type Message: Send + 'static;

    fn kind(&self) -> &'static str;
    fn session_id(&self) -> SessionId;

    /// Everything that must survive checkpoint and restore.
    fn state(&self) -> serde_json::Value;
    fn restore_state(&mut self, state: serde_json::Value) -> Result<()>;

    async fn handle(&mut self, message: Self::Message, ctx: &ActorContext) -> Result<serde_json::Value>;

    async fn on_start(&mut self, _ctx: &ActorContext) -> Result<()> {
        Ok(())
    }
    async fn on_stop(&mut self, _ctx: &ActorContext) -> Result<()> {
        Ok(())
    }
    async fn on_replay(&mut self, _event: &EventRecord) -> Result<()> {
        Ok(())
    }
}

/// Adapts a typed actor into the erased form used by the runtime.
pub struct TypedActor<A: Actor> {
    inner: A,
}

impl<A: Actor> TypedActor<A> {
    pub fn new(inner: A) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl<A: Actor> ErasedActor for TypedActor<A> {
    fn kind(&self) -> &'static str {
        self.inner.kind()
    }

    fn session_id(&self) -> SessionId {
        self.inner.session_id()
    }

    fn snapshot(&self) -> serde_json::Value {
        self.inner.state()
    }

    fn restore(&mut self, state: serde_json::Value) -> Result<()> {
        self.inner.restore_state(state)
    }

    async fn handle(&mut self, message: Box<dyn Any + Send>, ctx: &ActorContext) -> Result<serde_json::Value> {
        let typed = message.downcast::<A::Message>().map_err(|_| {
            RuntimeError::internal(format!("actor {} received a message of the wrong type", self.kind()))
        })?;
        self.inner.handle(*typed, ctx).await
    }

    async fn on_start(&mut self, ctx: &ActorContext) -> Result<()> {
        self.inner.on_start(ctx).await
    }

    async fn on_stop(&mut self, ctx: &ActorContext) -> Result<()> {
        self.inner.on_stop(ctx).await
    }

    async fn on_replay(&mut self, event: &EventRecord) -> Result<()> {
        self.inner.on_replay(event).await
    }
}

/// Mailbox commands. One enum keeps a single ordered queue per actor.
pub enum ActorCommand {
    Message {
        payload: Box<dyn Any + Send>,
        correlation: Correlation,
        reply: Option<oneshot::Sender<Result<serde_json::Value>>>,
    },
    Snapshot {
        reply: oneshot::Sender<Result<serde_json::Value>>,
    },
    Restore {
        state: serde_json::Value,
        generation: u64,
        reply: oneshot::Sender<Result<()>>,
    },
    Replay {
        events: Vec<EventRecord>,
        reply: oneshot::Sender<Result<u64>>,
    },
    Drain {
        reply: oneshot::Sender<()>,
    },
    Stop {
        reply: oneshot::Sender<()>,
    },
}

impl ActorCommand {
    fn label(&self) -> &'static str {
        match self {
            ActorCommand::Message { .. } => "message",
            ActorCommand::Snapshot { .. } => "snapshot",
            ActorCommand::Restore { .. } => "restore",
            ActorCommand::Replay { .. } => "replay",
            ActorCommand::Drain { .. } => "drain",
            ActorCommand::Stop { .. } => "stop",
        }
    }
}

/// Cheap, cloneable address of a running actor.
#[derive(Clone)]
pub struct ActorHandle {
    pub id: ActorId,
    pub session_id: SessionId,
    pub kind: &'static str,
    pub init: ActorInit,
    pub(crate) tx: mpsc::Sender<ActorCommand>,
    pub(crate) depth: Arc<AtomicUsize>,
    pub(crate) state: Arc<RwLock<ActorState>>,
    pub(crate) seq: Arc<AtomicU64>,
    pub(crate) cancellation: CancellationToken,
}

impl ActorHandle {
    /// Send a typed message and wait for the actor result. FIFO order is guaranteed by the mailbox.
    pub async fn send<M: Send + 'static>(&self, message: M) -> Result<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.enqueue(ActorCommand::Message {
            payload: Box::new(message),
            correlation: Correlation::new().with_actor(&self.id).with_session(&self.session_id),
            reply: Some(tx),
        })
        .await?;
        match rx.await {
            Ok(result) => result,
            Err(_) => Err(RuntimeError::unavailable(format!(
                "actor {} dropped the reply channel",
                self.id
            ))),
        }
    }

    /// Fire-and-forget: ordering is preserved, no reply is awaited.
    pub async fn cast<M: Send + 'static>(&self, message: M) -> Result<()> {
        self.enqueue(ActorCommand::Message {
            payload: Box::new(message),
            correlation: Correlation::new().with_actor(&self.id).with_session(&self.session_id),
            reply: None,
        })
        .await
    }

    pub async fn snapshot(&self) -> Result<serde_json::Value> {
        let (tx, rx) = oneshot::channel();
        self.enqueue(ActorCommand::Snapshot { reply: tx }).await?;
        rx.await
            .map_err(|_| RuntimeError::unavailable("actor dropped snapshot reply"))?
    }

    pub async fn restore(&self, state: serde_json::Value, generation: u64) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.enqueue(ActorCommand::Restore { state, generation, reply: tx }).await?;
        rx.await
            .map_err(|_| RuntimeError::unavailable("actor dropped restore reply"))?
    }

    pub async fn replay(&self, events: Vec<EventRecord>) -> Result<u64> {
        let (tx, rx) = oneshot::channel();
        self.enqueue(ActorCommand::Replay { events, reply: tx }).await?;
        rx.await
            .map_err(|_| RuntimeError::unavailable("actor dropped replay reply"))?
    }

    /// Stop accepting new work once the queue drains, then terminate.
    pub async fn drain(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        self.enqueue(ActorCommand::Drain { reply: tx }).await?;
        let _ = rx.await;
        Ok(())
    }

    /// Hard stop: cancel the actor and wait for the task to observe it.
    pub async fn stop(&self) -> Result<()> {
        let (tx, rx) = oneshot::channel();
        let _ = self.tx.send(ActorCommand::Stop { reply: tx }).await;
        self.cancellation.cancel();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), rx).await;
        Ok(())
    }

    async fn enqueue(&self, cmd: ActorCommand) -> Result<()> {
        let label = cmd.label();
        self.depth.fetch_add(1, Ordering::SeqCst);
        match self.tx.send(cmd).await {
            Ok(()) => Ok(()),
            Err(e) => {
                self.depth.fetch_sub(1, Ordering::SeqCst);
                let _ = e;
                Err(RuntimeError::unavailable(format!(
                    "actor {} mailbox is closed (command: {label})",
                    self.id
                )))
            }
        }
    }

    pub fn depth(&self) -> usize {
        self.depth.load(Ordering::SeqCst)
    }

    pub fn state(&self) -> ActorState {
        *self.state.read()
    }

    pub fn last_seq(&self) -> u64 {
        self.seq.load(Ordering::SeqCst)
    }

    pub fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Guarded state transition used by the runtime and by migration.
    pub(crate) fn transition(&self, next: ActorState) -> Result<ActorState> {
        let mut guard = self.state.write();
        let current = *guard;
        let applied = current.transition(next).map_err(|e| {
            RuntimeError::conflict(format!("actor {}: {e}", self.id))
        })?;
        *guard = applied;
        Ok(applied)
    }

    pub fn record(&self) -> ActorRecord {
        let mut rec = ActorRecord::new(self.id.clone(), self.session_id.clone(), self.kind);
        rec.state = self.state();
        rec.generation = self.init.generation;
        rec.mailbox_depth = self.depth();
        rec.last_applied_seq = self.last_seq();
        rec
    }

    /// Used by the runtime to build the handle; not part of the public surface.
    pub(crate) fn new(
        init: ActorInit,
        kind: &'static str,
        tx: mpsc::Sender<ActorCommand>,
        depth: Arc<AtomicUsize>,
        state: Arc<RwLock<ActorState>>,
        seq: Arc<AtomicU64>,
        cancellation: CancellationToken,
    ) -> Self {
        Self {
            id: init.actor_id.clone(),
            session_id: init.session_id.clone(),
            kind,
            init,
            tx,
            depth,
            state,
            seq,
            cancellation,
        }
    }
}

impl std::fmt::Debug for ActorHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ActorHandle")
            .field("id", &self.id)
            .field("kind", &self.kind)
            .field("state", &self.state())
            .field("depth", &self.depth())
            .finish()
    }
}
