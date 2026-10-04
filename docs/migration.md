# Actor migration, clone and recovery

## Principle

An actor is **data**, never a process image. We never copy a running process, a stack or a file
descriptor. Instead we take a serializable checkpoint, rebuild the actor on the target and replay
the events that happened after the snapshot. Migration, cloning and crash recovery are therefore
the same mechanism with three different entry points.

## Pipeline

```
 1 Checkpoint   actor.snapshot()                     -> serde_json::Value
 2 Snapshot     CheckpointStore.save(meta + state)   -> durable, hashed
 3 Transfer     ActorTransfer::transfer(checkpoint)  -> interface (LocalTransfer in v1)
 4 Restore      factory.create(init) + state restore -> a NEW actor instance
 5 Replay       bus.replay(after event_offset)       -> on_replay() per event
 6 Resume       actor becomes Active, generation + 1
```

Every stage emits an `actor_migrated` event carrying `{"stage": ...}`, so a failed migration is
diagnosable from the event log alone. The state machine refuses out-of-order stages
(`MigrationState` in `crates/core/src/state.rs`).

### What a checkpoint contains

| field | meaning |
|---|---|
| `id` | `CheckpointId` |
| `actor_id`, `session_id` | identity, checked on restore |
| `generation` | incremented on every restore/migration; a stale generation must never resume |
| `applied_seq` | how many mailbox messages are already reflected in the state |
| `event_offset` | event log position at snapshot time; replay starts after it |
| `bytes`, `state_hash` | size and SHA-256 of the serialized state |
| `domain_version` | guards against restoring state written by an incompatible runtime |

## Transfer is the reserved seam

```rust
#[async_trait]
pub trait ActorTransfer: Send + Sync + 'static {
    fn name(&self) -> &'static str;
    async fn transfer(&self, checkpoint: &Checkpoint, target: &TransferTarget) -> Result<TransferReceipt>;
    /// Reserved for pulling a checkpoint from another node during recovery.
    async fn fetch(&self, id: &CheckpointId, source: &TransferTarget) -> Result<Option<Checkpoint>>;
}
```

`LocalTransfer` (v1) verifies that the checkpoint is durably stored and that the payload hash
matches, then acknowledges. A future `GrpcTransfer` implements the same trait by streaming the
bytes to a peer - the pipeline above does not change, and neither does any caller.

## Clone

Cloning uses the same checkpoint with a **new** actor id and session id:

```rust
coordinator.clone_actor(source_actor, factory, target_session, new_actor_id)
```

The clone starts with its own generation counter and its own mailbox, so the two actors diverge
immediately. This is how "N copies of the same reasoning" (self-consistency, fan-out experiments)
becomes a runtime primitive rather than a copy-pasted pattern.

## Recovery

On startup the kernel restores worker records (marking previously-ready workers as `offline`) and
warms the directory cache. When a message arrives for a session whose actor is not running but is
present in the directory, `SessionManager::actor_for`:

1. finds the latest checkpoint for the actor,
2. spawns a new instance through the factory,
3. restores the state,
4. replays events after `event_offset`,
5. registers the new generation in the directory and routes the message.

Actor panics are caught (`catch_unwind`), the actor is marked `failed` and the same recovery path
applies - a panicked actor never keeps running with possibly corrupt state.

## What v1 deliberately does not do

* No live actor hand-off: migration stops the source actor, restores on the target. There is no
  window during which both process messages.
* No cross-node transfer implementation, only the interface (`LocalTransfer` is real and tested;
  `fetch` returns `None`).
* No streaming of large state: checkpoints are serialized whole. A large actor should checkpoint
  to a blob and stream it - the interface allows it, the v1 implementation does not do it.
