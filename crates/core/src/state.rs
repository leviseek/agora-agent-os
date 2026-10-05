//! Explicit lifecycle state machines.
//!
//! Rule of the codebase: lifecycle is never expressed with booleans. Each entity owns an enum
//! and a transition table, and illegal transitions are errors rather than silent corruption.

use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransitionError {
    pub machine: &'static str,
    pub from: &'static str,
    pub to: &'static str,
}

impl fmt::Display for TransitionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "illegal {} transition: {} -> {}", self.machine, self.from, self.to)
    }
}

impl std::error::Error for TransitionError {}

/// Common surface implemented by every state machine produced by the macro below.
pub trait StateMachine: Copy + fmt::Debug {
    fn machine_name() -> &'static str;
    fn as_str(&self) -> &'static str;
    fn is_terminal(&self) -> bool;
    fn can_transition_to(&self, next: Self) -> bool;
    fn transition(&self, next: Self) -> Result<Self, TransitionError>;
}

macro_rules! state_machine {
    (
        $(#[$meta:meta])*
        $name:ident {
            variants: [ $( $variant:ident = $text:literal ),+ $(,)? ],
            initial: $initial:ident,
            terminal: [ $( $terminal:ident ),* $(,)? ],
            transitions: [ $( $from:ident => $to:ident ),* $(,)? ]
        }
    ) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub enum $name {
            $( #[serde(rename = $text)] $variant ),+
        }

        impl $name {
            pub const ALL: &'static [Self] = &[ $( Self::$variant ),+ ];

            /// States from which no further transition is legal.
            pub const TERMINAL: &'static [Self] = &[ $( Self::$terminal ),* ];

            pub fn iter() -> impl Iterator<Item = Self> {
                Self::ALL.iter().copied()
            }

            pub fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $text ),+ }
            }

            pub fn is_terminal(self) -> bool {
                Self::TERMINAL.contains(&self)
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::$initial }
        }

        impl StateMachine for $name {
            fn machine_name() -> &'static str { stringify!($name) }
            fn as_str(&self) -> &'static str { (*self).as_str() }
            fn is_terminal(&self) -> bool { (*self).is_terminal() }

            fn can_transition_to(&self, next: Self) -> bool {
                let cur = *self;
                if cur == next { return true; }
                matches!((cur, next), $( ($name::$from, $name::$to) )|*)
            }

            fn transition(&self, next: Self) -> Result<Self, TransitionError> {
                if self.can_transition_to(next) {
                    Ok(next)
                } else {
                    Err(TransitionError {
                        machine: stringify!($name),
                        from: self.as_str(),
                        to: next.as_str(),
                    })
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

state_machine! {
    /// Lifecycle of a user session (the externally visible conversation unit).
    SessionState {
        variants: [ Creating = "creating", Active = "active", Idle = "idle",
                    Suspended = "suspended", Closing = "closing", Closed = "closed", Failed = "failed" ],
        initial: Creating,
        terminal: [ Closed, Failed ],
        transitions: [
            Creating => Active, Creating => Failed,
            Active => Idle, Active => Suspended, Active => Closing, Active => Failed,
            Idle => Active, Idle => Suspended, Idle => Closing, Idle => Failed,
            Suspended => Active, Suspended => Closing, Suspended => Failed,
            Closing => Closed, Closing => Failed,
            // A closed session can be opened again. Closing stops the actor and freezes the content;
            // it is not a delete, and treating it as one made "close" the end of a conversation that
            // the record, the runs and the transcript had all kept.
            Closed => Active,
        ]
    }
}

state_machine! {
    /// Lifecycle of an actor inside the actor runtime.
    ActorState {
        variants: [ Spawning = "spawning", Active = "active", Idle = "idle",
                    Draining = "draining", Migrating = "migrating", Stopped = "stopped", Failed = "failed" ],
        initial: Spawning,
        terminal: [ Stopped, Failed ],
        transitions: [
            Spawning => Active, Spawning => Failed,
            Active => Idle, Active => Draining, Active => Migrating, Active => Failed,
            Idle => Active, Idle => Draining, Idle => Migrating, Idle => Failed,
            Draining => Stopped, Draining => Failed,
            Migrating => Active, Migrating => Failed,
        ]
    }
}

state_machine! {
    /// Lifecycle of a task-graph node. Retrying is a first-class state, not an error flag.
    TaskState {
        variants: [ Pending = "pending", Ready = "ready", Running = "running",
                    Retrying = "retrying", Succeeded = "succeeded", Failed = "failed", Cancelled = "cancelled" ],
        initial: Pending,
        terminal: [ Succeeded, Failed, Cancelled ],
        transitions: [
            Pending => Ready, Pending => Cancelled,
            Ready => Running, Ready => Cancelled,
            Running => Retrying, Running => Succeeded, Running => Failed, Running => Cancelled,
            Retrying => Running, Retrying => Failed, Retrying => Cancelled,
            Failed => Ready,
        ]
    }
}

state_machine! {
    /// Lifecycle of a worker process participating in placement.
    WorkerState {
        variants: [ Joining = "joining", Ready = "ready", Draining = "draining",
                    Offline = "offline", Lost = "lost" ],
        initial: Joining,
        terminal: [ Offline, Lost ],
        transitions: [
            Joining => Ready, Joining => Lost,
            Ready => Draining, Ready => Lost, Ready => Offline,
            Draining => Offline, Draining => Lost,
            Offline => Ready,
        ]
    }
}

state_machine! {
    /// The agent loop phases: Goal -> Plan -> Think -> Act -> Observe -> Finalize.
    AgentRunState {
        variants: [ Goal = "goal", Planning = "planning", Thinking = "thinking",
                    Acting = "acting", Observing = "observing", Finalizing = "finalizing",
                    Succeeded = "succeeded", Failed = "failed", Cancelled = "cancelled" ],
        initial: Goal,
        terminal: [ Succeeded, Failed, Cancelled ],
        transitions: [
            Goal => Planning, Goal => Thinking, Goal => Failed, Goal => Cancelled,
            Planning => Thinking, Planning => Acting, Planning => Finalizing,
            Planning => Failed, Planning => Cancelled,
            Thinking => Acting, Thinking => Finalizing, Thinking => Failed, Thinking => Cancelled,
            Acting => Observing, Acting => Failed, Acting => Cancelled,
            Observing => Thinking, Observing => Finalizing, Observing => Failed, Observing => Cancelled,
            Finalizing => Succeeded, Finalizing => Failed, Finalizing => Cancelled,
        ]
    }
}

state_machine! {
    /// Migration pipeline: Checkpoint -> Snapshot -> Transfer -> Restore -> Replay -> Resume.
    MigrationState {
        variants: [ Idle = "idle", Checkpointing = "checkpointing", Snapshotting = "snapshotting",
                    Transferring = "transferring", Restoring = "restoring", Replaying = "replaying",
                    Completed = "completed", Failed = "failed" ],
        initial: Idle,
        terminal: [ Completed, Failed ],
        transitions: [
            Idle => Checkpointing, Idle => Failed,
            Checkpointing => Snapshotting, Checkpointing => Failed,
            Snapshotting => Transferring, Snapshotting => Failed,
            Transferring => Restoring, Transferring => Failed,
            Restoring => Replaying, Restoring => Failed,
            Replaying => Completed, Replaying => Failed,
        ]
    }
}

state_machine! {
    /// Reachability of a capability in the mesh.
    CapabilityHealth {
        variants: [ Unknown = "unknown", Healthy = "healthy", Degraded = "degraded", Unavailable = "unavailable" ],
        initial: Unknown,
        terminal: [],
        transitions: [
            Unknown => Healthy, Unknown => Degraded, Unknown => Unavailable,
            Healthy => Degraded, Healthy => Unavailable,
            Degraded => Healthy, Degraded => Unavailable,
            Unavailable => Healthy, Unavailable => Degraded,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_happy_path() {
        let mut s = SessionState::default();
        assert_eq!(s, SessionState::Creating);
        s = s.transition(SessionState::Active).unwrap();
        s = s.transition(SessionState::Idle).unwrap();
        s = s.transition(SessionState::Closing).unwrap();
        s = s.transition(SessionState::Closed).unwrap();
        assert!(s.is_terminal());
    }

    #[test]
    fn a_closed_session_can_be_opened_again() {
        // Closing is a pause, not an end: the record, the runs and the transcript all survive it,
        // so the state has to be able to come back. What closing does end is the actor.
        assert_eq!(
            SessionState::Closed.transition(SessionState::Active).unwrap(),
            SessionState::Active
        );
        // A failed session is the one that cannot come back: nothing about it finished.
        assert!(SessionState::Failed.transition(SessionState::Active).is_err());
        // Terminal still means "nothing runs here", which is what a closed session is.
        assert!(SessionState::Closed.is_terminal());
    }

    #[test]
    fn self_transition_is_allowed_and_idempotent() {
        assert!(SessionState::Active.transition(SessionState::Active).is_ok());
    }

    #[test]
    fn task_retry_loop_is_legal_but_success_after_failure_requires_ready() {
        let t = TaskState::Running.transition(TaskState::Retrying).unwrap();
        assert!(t.transition(TaskState::Succeeded).is_err());
        let t = TaskState::Failed.transition(TaskState::Ready).unwrap();
        assert!(t.transition(TaskState::Running).is_ok());
    }

    #[test]
    fn agent_loop_reaches_final_and_observation_loops_back() {
        let a = AgentRunState::Goal.transition(AgentRunState::Planning).unwrap();
        let a = a.transition(AgentRunState::Thinking).unwrap();
        let a = a.transition(AgentRunState::Acting).unwrap();
        let a = a.transition(AgentRunState::Observing).unwrap();
        let a = a.transition(AgentRunState::Thinking).unwrap();
        let a = a.transition(AgentRunState::Finalizing).unwrap();
        let a = a.transition(AgentRunState::Succeeded).unwrap();
        assert!(a.is_terminal());
    }

    #[test]
    fn migration_pipeline_is_ordered() {
        let m = MigrationState::Idle.transition(MigrationState::Checkpointing).unwrap();
        assert!(m.transition(MigrationState::Restoring).is_err());
    }

    #[test]
    fn serde_uses_stable_snake_case_strings() {
        let j = serde_json::to_string(&SessionState::Idle).unwrap();
        assert_eq!(j, "\"idle\"");
        let back: SessionState = serde_json::from_str("\"suspended\"").unwrap();
        assert_eq!(back, SessionState::Suspended);
    }
}
