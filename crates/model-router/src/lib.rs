//! agentos-model-router - one model interface, many providers.
//!
//! The router owns three responsibilities and nothing else:
//!   1. translate a provider-agnostic request into a provider call,
//!   2. choose a provider according to policy (task, health, priority),
//!   3. fail over to the next candidate when a provider is unconfigured or sick.
//!
//! Agents never see provider specifics, which is what makes "replace the model" a configuration
//! change rather than a rewrite.

pub mod mock;
pub mod openai_compat;
pub mod provider;
pub mod router;

pub use mock::MockProvider;
pub use openai_compat::OpenAiCompatibleProvider;
pub use provider::{
    ChatMessage, ModelProvider, ModelRequest, ModelResponse, ModelTask, ProviderHealth, ToolCall,
    ToolSpec, Usage,
};
pub use router::{ModelRouter, ProviderInfo, RoutingPolicy};
