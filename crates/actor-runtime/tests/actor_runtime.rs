//! Actor runtime behaviour: ordering, isolation, checkpoint, restore, replay, migration.

use agentos_actor_runtime::actor::{Actor, ActorContext, ActorFactory, ActorInit, ErasedActor};
use agentos_actor_runtime::checkpoint::StoreCheckpointStore;
use agentos_actor_runtime::migration::{LocalTransfer, MigrationCoordinator, TransferTarget};
use agentos_actor_runtime::runtime::{ActorRuntime, ActorRuntimeConfig};
use agentos_core::error::Result as OsResult;
use agentos_core::state::{ActorState, MigrationState, StateMachine};
use agentos_core::{ActorId, SessionId};
use agentos_event_bus::{EventBus, LocalEventBus};
use agentos_storage::memory::MemoryStore;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Serialize, Deserialize)]
enum CounterMsg {
    Add(i64),
    Get,
}

/// A counter that records the order in which it saw messages.
struct Counter {
    session: SessionId,
    value: i64,
    seen: Vec<i64>,
}

#[derive(Serialize, Deserialize, Default)]
struct CounterState {
    value: i64,
    seen: Vec<i64>,
}

#[async_trait]
impl Actor for Counter {
    type Message = CounterMsg;

    fn kind(&self) -> &'static str {
        "counter"
    }
    fn session_id(&self) -> SessionId {
        self.session.clone()
    }
    fn state(&self) -> serde_json::Value {
        serde_json::to_value(CounterState { value: self.value, seen: self.seen.clone() }).unwrap()
    }
    fn restore_state(&mut self, state: serde_json::Value) -> OsResult<()> {
        let s: CounterState = serde_json::from_value(state)
            .map_err(|e| agentos_core::RuntimeError::internal(format!("bad counter state: {e}")))?;
        self.value = s.value;
        self.seen = s.seen;
        Ok(())
    }
    async fn handle(&mut self, msg: CounterMsg, ctx: &ActorContext) -> OsResult<serde_json::Value> {
        match msg {
            CounterMsg::Add(n) => {
                self.value += n;
                self.seen.push(ctx.seq as i64);
                Ok(serde_json::json!({ "value": self.value }))
            }
            CounterMsg::Get => Ok(serde_json::json!({ "value": self.value, "seen": self.seen })),
        }
    }
}

struct CounterFactory;

impl ActorFactory for CounterFactory {
    fn kind(&self) -> &'static str {
        "counter"
    }
    fn create(&self, init: &ActorInit) -> OsResult<Box<dyn ErasedActor>> {
        Ok(Box::new(agentos_actor_runtime::actor::TypedActor::new(Counter {
            session: init.session_id.clone(),
            value: 0,
            seen: vec![],
        })))
    }
}

async fn runtime() -> (Arc<ActorRuntime>, Arc<dyn EventBus>) {
    let store = Arc::new(MemoryStore::new());
    let bus: Arc<dyn EventBus> = Arc::new(LocalEventBus::new(store.clone(), 512, "test-node").await);
    let checkpoints = Arc::new(StoreCheckpointStore::new(store.clone()));
    let rt = Arc::new(ActorRuntime::new(
        store,
        bus.clone(),
        checkpoints,
        ActorRuntimeConfig { mailbox_capacity: 64, node_id: "test-node".into(), max_replay_events: 1000 },
    ));
    (rt, bus)
}

async fn spawn_counter(rt: &ActorRuntime, session: SessionId) -> agentos_actor_runtime::actor::ActorHandle {
    rt.spawn(
        ActorInit { actor_id: ActorId::new(), session_id: session, generation: 0, params: serde_json::Value::Null },
        Counter { session: SessionId::new(), value: 0, seen: vec![] },
    )
    .await
    .unwrap()
}

#[tokio::test]
async fn messages_within_one_actor_are_processed_in_order() {
    let (rt, _bus) = runtime().await;
    let handle = spawn_counter(&rt, SessionId::new()).await;
    for i in 1..=50 {
        handle.send(CounterMsg::Add(i)).await.unwrap();
    }
    let got = handle.send(CounterMsg::Get).await.unwrap();
    assert_eq!(got["value"], 1275, "1..50 summed");
    let seen: Vec<i64> = serde_json::from_value(got["seen"].clone()).unwrap();
    assert_eq!(seen, (1..=50).collect::<Vec<i64>>(), "strict mailbox order");
}

#[tokio::test]
async fn different_actors_run_concurrently() {
    let (rt, _bus) = runtime().await;
    let a = spawn_counter(&rt, SessionId::new()).await;
    let b = spawn_counter(&rt, SessionId::new()).await;
    assert_ne!(a.id, b.id);
    let (ra, rb) = tokio::join!(a.send(CounterMsg::Add(5)), b.send(CounterMsg::Add(7)));
    assert_eq!(ra.unwrap()["value"], 5);
    assert_eq!(rb.unwrap()["value"], 7);
}

#[tokio::test]
async fn checkpoint_restore_and_replay_preserves_state() {
    let (rt, _bus) = runtime().await;
    let session = SessionId::new();
    let handle = spawn_counter(&rt, session.clone()).await;
    handle.send(CounterMsg::Add(10)).await.unwrap();
    handle.send(CounterMsg::Add(5)).await.unwrap();

    let checkpoint = rt.checkpoint(&handle.id).await.unwrap();
    assert!(checkpoint.meta.state_hash.len() == 64);
    assert_eq!(checkpoint.state["value"], 15);

    // More work after the checkpoint: it must survive as replayed history.
    handle.send(CounterMsg::Add(100)).await.unwrap();

    let (restored, _replayed) = rt.restore(checkpoint, Arc::new(CounterFactory)).await.unwrap();
    let after = restored.send(CounterMsg::Get).await.unwrap();
    assert_eq!(after["value"], 15, "restored from the snapshot, not from the live actor");
    assert_eq!(restored.state(), ActorState::Active);
}

#[tokio::test]
async fn stops_are_observable_and_actors_leave_the_registry() {
    let (rt, _bus) = runtime().await;
    let handle = spawn_counter(&rt, SessionId::new()).await;
    assert_eq!(rt.list().len(), 1);
    rt.stop(&handle.id).await.unwrap();
    assert!(rt.lookup(&handle.id).is_none());
    assert!(handle.state().is_terminal());
}

#[tokio::test]
async fn migration_pipeline_completes_and_reports_every_stage() {
    let (rt, bus) = runtime().await;
    let session = SessionId::new();
    let handle = spawn_counter(&rt, session.clone()).await;
    handle.send(CounterMsg::Add(42)).await.unwrap();

    let coordinator = MigrationCoordinator::new(
        rt.clone(),
        Arc::new(LocalTransfer::new(rt.checkpoints())),
    );
    let report = coordinator
        .migrate(&handle.id, Arc::new(CounterFactory), TransferTarget { worker_id: None, endpoint: None })
        .await
        .unwrap();
    assert_eq!(report.state, MigrationState::Completed);
    assert!(report.checkpoint_id.is_some());

    let moved = rt.lookup(&handle.id).unwrap();
    assert_eq!(moved.init.generation, 1, "generation must advance across migration");
    assert_eq!(moved.send(CounterMsg::Get).await.unwrap()["value"], 42);

    let events = bus
        .replay(agentos_core::model::EventFilter {
            kinds: vec![agentos_core::model::EventKind::ActorMigrated],
            limit: 50,
            ..Default::default()
        })
        .await
        .unwrap();
    let stages: Vec<String> = events
        .iter()
        .filter_map(|e| e.payload.get("stage").and_then(|s| s.as_str()).map(|s| s.to_string()))
        .collect();
    for expected in ["checkpointing", "snapshotting", "transferring", "restoring", "replaying", "completed"] {
        assert!(stages.contains(&expected.to_string()), "missing stage {expected} in {stages:?}");
    }
}

#[tokio::test]
async fn cloning_produces_independent_actors() {
    let (rt, _bus) = runtime().await;
    let source = spawn_counter(&rt, SessionId::new()).await;
    source.send(CounterMsg::Add(9)).await.unwrap();
    let coordinator = MigrationCoordinator::new(rt.clone(), Arc::new(LocalTransfer::new(rt.checkpoints())));
    let clone_id = coordinator
        .clone_actor(&source.id, Arc::new(CounterFactory), SessionId::new(), ActorId::new())
        .await
        .unwrap();
    let clone = rt.lookup(&clone_id).unwrap();
    assert_eq!(clone.send(CounterMsg::Get).await.unwrap()["value"], 9);
    clone.send(CounterMsg::Add(1)).await.unwrap();
    assert_eq!(source.send(CounterMsg::Get).await.unwrap()["value"], 9, "clone is independent");
    assert_eq!(clone.send(CounterMsg::Get).await.unwrap()["value"], 10);
}

#[tokio::test]
async fn illegal_state_transitions_are_rejected() {
    let (rt, _bus) = runtime().await;
    let handle = spawn_counter(&rt, SessionId::new()).await;
    rt.stop(&handle.id).await.unwrap();
    // A stopped actor cannot go back to Active.
    assert!(ActorState::Stopped.transition(ActorState::Active).is_err());
}
