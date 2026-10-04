//! Approvals: a capability that needs a human decision parks until one arrives.
//!
//! Three things can end a wait, and all three must be handled or the runtime has a way to hang:
//! a decision, a timeout, and the caller being cancelled. The ticket removes itself from the
//! pending list when it is dropped, so a cancelled caller cannot leave a ghost request behind.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::{now_ms, SessionId};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::oneshot;

/// What is waiting for a decision. This is what an operator sees, so it carries enough to
/// decide: which capability, for which session, with which arguments (bounded).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalRequest {
    pub id: String,
    pub capability: String,
    pub session_id: SessionId,
    pub actor_id: Option<String>,
    pub task_id: Option<String>,
    /// A bounded preview of the arguments. Necessary to decide, so it is present, and truncated
    /// so a large payload cannot become a large event.
    pub arguments_preview: String,
    pub reason: String,
    /// Milliseconds since the epoch, as everywhere else in the runtime.
    pub created_at: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApprovalDecision {
    pub approved: bool,
    pub reason: Option<String>,
    /// Who decided. Free text: the runtime has no user directory and does not pretend to.
    pub decided_by: Option<String>,
    pub decided_at: u64,
}

impl ApprovalDecision {
    pub fn approve(by: Option<String>) -> Self {
        Self { approved: true, reason: None, decided_by: by, decided_at: now_ms() }
    }
    pub fn deny(reason: impl Into<String>, by: Option<String>) -> Self {
        Self { approved: false, reason: Some(reason.into()), decided_by: by, decided_at: now_ms() }
    }
}

/// How long a capability call may wait for a decision, if the configuration says otherwise.
pub const DEFAULT_APPROVAL_TIMEOUT_MS: u64 = 120_000;

type Pending = Arc<Mutex<HashMap<String, (ApprovalRequest, oneshot::Sender<ApprovalDecision>)>>>;

pub struct ApprovalBroker {
    pending: Pending,
    max_pending: usize,
}

impl ApprovalBroker {
    pub fn new(max_pending: usize) -> Self {
        Self { pending: Arc::new(Mutex::new(HashMap::new())), max_pending: max_pending.max(1) }
    }

    /// Register a request and hand back a ticket to wait on.
    pub fn request(&self, request: ApprovalRequest) -> Result<ApprovalTicket> {
        let mut pending = self.pending.lock();
        if pending.len() >= self.max_pending {
            return Err(RuntimeError::rate_limited(format!(
                "{} approvals are already waiting; decide or cancel some before asking for more",
                pending.len()
            )));
        }
        let (sender, receiver) = oneshot::channel();
        let id = request.id.clone();
        pending.insert(id.clone(), (request.clone(), sender));
        Ok(ApprovalTicket {
            id,
            request,
            receiver: Some(receiver),
            pending: self.pending.clone(),
        })
    }

    /// Deliver a decision. Returns what was decided, or an error if the request is gone.
    pub fn decide(&self, id: &str, decision: ApprovalDecision) -> Result<ApprovalRequest> {
        let entry = self.pending.lock().remove(id);
        let Some((request, sender)) = entry else {
            return Err(RuntimeError::not_found(format!(
                "approval {id} is not waiting: it was decided, cancelled or expired"
            )));
        };
        // A send failure means the caller went away between the lookup and the send, which is not
        // an error for the operator: the request is gone either way.
        let _ = sender.send(decision);
        Ok(request)
    }

    pub fn pending(&self) -> Vec<ApprovalRequest> {
        let mut requests: Vec<ApprovalRequest> =
            self.pending.lock().values().map(|(request, _)| request.clone()).collect();
        requests.sort_by_key(|request| request.created_at);
        requests
    }

    pub fn len(&self) -> usize {
        self.pending.lock().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A parked call. Dropping it withdraws the request.
#[derive(Debug)]
pub struct ApprovalTicket {
    id: String,
    request: ApprovalRequest,
    receiver: Option<oneshot::Receiver<ApprovalDecision>>,
    pending: Pending,
}

impl ApprovalTicket {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn request(&self) -> &ApprovalRequest {
        &self.request
    }

    /// Wait for the decision. The caller is expected to select on this together with its own
    /// cancellation token and a timeout.
    pub async fn wait(mut self) -> Result<ApprovalDecision> {
        let receiver = self
            .receiver
            .take()
            .ok_or_else(|| RuntimeError::internal("approval ticket was already waited on"))?;
        receiver
            .await
            .map_err(|_| RuntimeError::internal("approval channel closed without a decision"))
    }
}

impl Drop for ApprovalTicket {
    fn drop(&mut self) {
        // Only removes it if nobody decided: decide() already took the entry out.
        self.pending.lock().remove(&self.id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(capability: &str) -> ApprovalRequest {
        ApprovalRequest {
            id: format!("apr_{capability}"),
            capability: capability.to_string(),
            session_id: SessionId::new(),
            actor_id: None,
            task_id: None,
            arguments_preview: "{\"path\":\"notes.txt\"}".into(),
            reason: "filesystem writes need a decision".into(),
            created_at: now_ms(),
        }
    }

    #[tokio::test]
    async fn a_decision_reaches_the_waiting_caller() {
        let broker = Arc::new(ApprovalBroker::new(8));
        let ticket = broker.request(request("filesystem-write")).unwrap();
        assert_eq!(broker.len(), 1);

        let decided = broker
            .decide("apr_filesystem-write", ApprovalDecision::approve(Some("operator".into())))
            .unwrap();
        assert_eq!(decided.capability, "filesystem-write");

        let decision = ticket.wait().await.unwrap();
        assert!(decision.approved);
        assert_eq!(decision.decided_by.as_deref(), Some("operator"));
        assert!(broker.is_empty(), "a decided request stops being pending");
    }

    #[tokio::test]
    async fn a_denial_carries_its_reason() {
        let broker = ApprovalBroker::new(8);
        let ticket = broker.request(request("filesystem-write")).unwrap();
        broker
            .decide("apr_filesystem-write", ApprovalDecision::deny("not this file", None))
            .unwrap();
        let decision = ticket.wait().await.unwrap();
        assert!(!decision.approved);
        assert_eq!(decision.reason.as_deref(), Some("not this file"));
    }

    #[tokio::test]
    async fn a_decided_request_cannot_be_decided_twice() {
        let broker = ApprovalBroker::new(8);
        let _ticket = broker.request(request("filesystem-write")).unwrap();
        broker
            .decide("apr_filesystem-write", ApprovalDecision::approve(None))
            .unwrap();
        let second = broker.decide("apr_filesystem-write", ApprovalDecision::approve(None));
        assert!(second.is_err(), "a decision is single use, not a toggle");
        assert_eq!(second.unwrap_err().kind, agentos_core::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn dropping_a_ticket_withdraws_the_request() {
        let broker = ApprovalBroker::new(8);
        let ticket = broker.request(request("filesystem-write")).unwrap();
        assert_eq!(broker.len(), 1);
        drop(ticket);
        assert!(broker.is_empty(), "a cancelled caller leaves no ghost request");
        assert!(broker.decide("apr_filesystem-write", ApprovalDecision::approve(None)).is_err());
    }

    #[test]
    fn the_pending_list_is_bounded() {
        let broker = ApprovalBroker::new(2);
        let mut first = request("a");
        first.id = "one".into();
        let mut second = request("b");
        second.id = "two".into();
        let mut third = request("c");
        third.id = "three".into();
        let _a = broker.request(first).unwrap();
        let _b = broker.request(second).unwrap();
        let error = broker.request(third).unwrap_err();
        assert_eq!(error.kind, agentos_core::ErrorKind::RateLimited);
        assert_eq!(broker.pending().len(), 2, "and the list still shows what is waiting");
    }
}
