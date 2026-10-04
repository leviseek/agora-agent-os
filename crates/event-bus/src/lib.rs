//! agentos-event-bus - one ordered, replayable stream for the whole runtime.
//!
//! The bus is deliberately dumb: publish, subscribe, replay. It is NOT on the request path of a
//! capability call, and subscribers that block must buffer or drop on their own side.

pub mod bus;
pub mod local;

pub use bus::{EventBus, EventStream, Subscription};
pub use local::LocalEventBus;
