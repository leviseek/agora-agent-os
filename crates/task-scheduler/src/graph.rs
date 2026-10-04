//! Graph construction and validation.

use agentos_core::error::{Result, RuntimeError};
use agentos_core::model::{TaskGraphRecord, TaskRecord};
use agentos_core::{SessionId, TaskId};
use std::collections::{BTreeMap, BTreeSet, VecDeque};

/// Builds and validates a DAG of tasks. Validation is strict: dangling dependencies and cycles
/// are rejected before anything is scheduled.
pub struct TaskGraphBuilder {
    graph: TaskGraphRecord,
}

impl TaskGraphBuilder {
    pub fn new(session_id: SessionId, title: impl Into<String>) -> Self {
        Self { graph: TaskGraphRecord::new(session_id, title) }
    }

    pub fn graph_id(&self) -> TaskId {
        self.graph.id.clone()
    }

    pub fn add(&mut self, node: TaskRecord) -> TaskId {
        let id = node.id.clone();
        self.graph.add(node);
        id
    }

    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.graph.title = title.into();
        self
    }

    /// Validate and return the graph. Dependencies must be acyclic and resolvable.
    pub fn build(self) -> Result<TaskGraphRecord> {
        let order = topological_order(&self.graph.nodes)?;
        debug_assert_eq!(order.len(), self.graph.nodes.len());
        Ok(self.graph)
    }
}

/// Kahn topological sort. Returns an error naming the cycle members when one exists.
pub fn topological_order(nodes: &[TaskRecord]) -> Result<Vec<TaskId>> {
    let ids: BTreeSet<TaskId> = nodes.iter().map(|n| n.id.clone()).collect();
    for node in nodes {
        for dep in &node.deps {
            if !ids.contains(dep) {
                return Err(RuntimeError::invalid_input(format!(
                    "task {} depends on unknown task {dep}",
                    node.id
                ))
                .with_detail("task_id", node.id.as_str()));
            }
        }
    }

    let mut indegree: BTreeMap<TaskId, usize> = nodes.iter().map(|n| (n.id.clone(), n.deps.len())).collect();
    let mut dependents: BTreeMap<TaskId, Vec<TaskId>> = BTreeMap::new();
    for node in nodes {
        for dep in &node.deps {
            dependents.entry(dep.clone()).or_default().push(node.id.clone());
        }
    }

    let mut queue: VecDeque<TaskId> = indegree
        .iter()
        .filter(|(_, d)| **d == 0)
        .map(|(id, _)| id.clone())
        .collect();
    let mut order = Vec::with_capacity(nodes.len());
    while let Some(id) = queue.pop_front() {
        order.push(id.clone());
        if let Some(children) = dependents.get(&id) {
            for child in children {
                if let Some(d) = indegree.get_mut(child) {
                    *d -= 1;
                    if *d == 0 {
                        queue.push_back(child.clone());
                    }
                }
            }
        }
    }

    if order.len() != nodes.len() {
        let stuck: Vec<String> = nodes
            .iter()
            .filter(|n| !order.contains(&n.id))
            .map(|n| n.id.to_string())
            .collect();
        return Err(RuntimeError::invalid_input("task graph contains a cycle")
            .with_detail("cycle_nodes", serde_json::json!(stuck)));
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::model::{TaskKind, TaskPayload};

    fn node(title: &str) -> TaskRecord {
        TaskRecord::new(
            TaskId::new(),
            SessionId::new(),
            title,
            TaskKind::Join,
            TaskPayload::Join { template: "{}".into() },
        )
    }

    #[test]
    fn independent_tasks_have_no_order_constraint() {
        let nodes = vec![node("a"), node("b")];
        let order = topological_order(&nodes).unwrap();
        assert_eq!(order.len(), 2);
    }

    #[test]
    fn dangling_dependency_is_rejected() {
        let mut b = TaskGraphBuilder::new(SessionId::new(), "g");
        let missing = TaskId::new();
        let n = node("a").with_deps(vec![missing]);
        b.add(n);
        assert!(b.build().is_err());
    }

    #[test]
    fn cycles_are_detected() {
        // Build a cycle manually: a -> b -> a.
        let mut a = node("a");
        let mut b = node("b");
        a.deps = vec![b.id.clone()];
        b.deps = vec![a.id.clone()];
        let err = topological_order(&[a, b]).unwrap_err();
        assert!(err.message.contains("cycle"));
    }

    #[test]
    fn diamond_dependencies_order_correctly() {
        let root = node("root");
        let left = node("left").with_deps(vec![root.id.clone()]);
        let right = node("right").with_deps(vec![root.id.clone()]);
        let join = node("join").with_deps(vec![left.id.clone(), right.id.clone()]);
        let order = topological_order(&[join.clone(), right.clone(), root.clone(), left.clone()]).unwrap();
        let pos = |id: &TaskId| order.iter().position(|x| x == id).unwrap();
        assert!(pos(&root.id) < pos(&left.id));
        assert!(pos(&left.id) < pos(&join.id));
        assert!(pos(&right.id) < pos(&join.id));
    }
}
