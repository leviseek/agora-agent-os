/** View 5 - Task graph: the session's DAG rendered with React Flow, live-updated from task_* events. */

import { useMemo, useState } from 'react';
import { Background, Controls, MarkerType, MiniMap, ReactFlow } from '@xyflow/react';
import type { Edge, Node } from '@xyflow/react';
import { ApiErrorBanner, Badge, EmptyState, JsonBlock, KeyValue, Panel } from '../components';
import { formatDuration, formatTime } from '../format';
import { useAsyncData } from '../hooks';
import { useI18n } from '../i18n';
import { useNav } from '../navigation';
import { useApp } from '../store';
import type { TaskGraphRecord, TaskRecord, TaskState } from '../api';

const STATE_STYLE: Record<TaskState, { background: string; border: string }> = {
  pending: { background: '#21262d', border: '#484f58' },
  ready: { background: '#132e4a', border: '#388bfd' },
  running: { background: '#3b2d0f', border: '#d29922' },
  retrying: { background: '#4a2c12', border: '#db6d28' },
  succeeded: { background: '#0f2f1d', border: '#3fb950' },
  failed: { background: '#3d1418', border: '#f85149' },
  cancelled: { background: '#2b2b2b', border: '#8b949e' },
};

const TASK_EVENT_STATES: Record<string, TaskState> = {
  task_created: 'pending',
  task_queued: 'ready',
  task_started: 'running',
  task_retrying: 'retrying',
  task_completed: 'succeeded',
  task_failed: 'failed',
  task_cancelled: 'cancelled',
};

/** Raw task kinds: machine identifiers, never translated. */
const KIND_LABEL: Record<string, string> = {
  capability: 'capability',
  model: 'model',
  agent: 'agent',
  join: 'join',
};

/** Translators handed to the pure layout helper so the graph stays locale aware. */
interface FlowLabels {
  t: (key: string, params?: Record<string, string | number>) => string;
  tState: (value: string) => string;
}

/** Simple layered layout: column = longest dependency chain, row = order inside the column. */
function layoutGraph(
  nodes: TaskRecord[],
  states: Record<string, TaskState>,
  labels: FlowLabels,
): { flowNodes: Node[]; edges: Edge[] } {
  const byId = new Map<string, TaskRecord>();
  for (const node of nodes) byId.set(node.id, node);

  const depthCache = new Map<string, number>();
  const depthOf = (id: string, seen: Set<string>): number => {
    const cached = depthCache.get(id);
    if (cached !== undefined) return cached;
    if (seen.has(id)) return 0;
    seen.add(id);
    const record = byId.get(id);
    let depth = 0;
    for (const dep of record?.deps ?? []) {
      depth = Math.max(depth, depthOf(dep, seen) + 1);
    }
    depthCache.set(id, depth);
    return depth;
  };

  const rows = new Map<number, number>();
  const flowNodes: Node[] = [];
  for (const record of nodes) {
    const depth = depthOf(record.id, new Set<string>());
    const row = rows.get(depth) ?? 0;
    rows.set(depth, row + 1);
    const state = states[record.id] ?? record.state;
    const style = STATE_STYLE[state] ?? STATE_STYLE.pending;
    flowNodes.push({
      id: record.id,
      position: { x: 40 + depth * 300, y: 30 + row * 120 },
      data: {
        label: (
          <div className="flow-node-body">
            <div className="flow-node-title">{record.title}</div>
            <div className="flow-node-meta">
              <span className={'flow-chip flow-chip-' + state}>{labels.tState(state)}</span>
              <span className="flow-node-kind">{KIND_LABEL[record.kind] ?? record.kind}</span>
            </div>
            <div className="flow-node-sub">
              {record.attempts > 0
                ? labels.t('graph.nodeAttempts', { n: record.attempts, max: record.max_attempts })
                : labels.t('graph.notStarted')}
            </div>
          </div>
        ),
      },
      style: {
        width: 240,
        background: style.background,
        border: '1px solid ' + style.border,
        borderRadius: 10,
        color: '#e6edf3',
        padding: 8,
        fontSize: 12,
      },
    });
  }

  const edges: Edge[] = [];
  for (const record of nodes) {
    for (const dep of record.deps) {
      if (!byId.has(dep)) continue;
      const state = states[record.id] ?? record.state;
      edges.push({
        id: dep + '->' + record.id,
        source: dep,
        target: record.id,
        animated: state === 'running' || state === 'retrying',
        markerEnd: { type: MarkerType.ArrowClosed, color: '#8b949e' },
        style: { stroke: '#8b949e' },
      });
    }
  }

  return { flowNodes, edges };
}

export function TaskGraphView() {
  const { t, tState } = useI18n();
  const { selectedSessionId, sessions, events, client } = useApp();
  const { setView } = useNav();
  const [graphId, setGraphId] = useState<string | null>(null);
  const [nodeId, setNodeId] = useState<string | null>(null);

  const graphs = useAsyncData<TaskGraphRecord[]>(
    () => {
      if (selectedSessionId === null) return Promise.resolve<TaskGraphRecord[]>([]);
      return client.sessionGraph(selectedSessionId).then((response) => response.graphs);
    },
    [client, selectedSessionId],
    { intervalMs: 5000 },
  );

  const list = graphs.data ?? [];
  const graph = useMemo(() => {
    if (list.length === 0) return undefined;
    if (graphId !== null) {
      const found = list.find((item) => item.id === graphId);
      if (found !== undefined) return found;
    }
    return list[0];
  }, [list, graphId]);

  const liveStates = useMemo(() => {
    const states: Record<string, TaskState> = {};
    if (graph === undefined) return states;
    const known = new Set(graph.nodes.map((node) => node.id));
    for (const event of events) {
      const taskId = event.task_id;
      if (taskId === null || !known.has(taskId)) continue;
      const next = TASK_EVENT_STATES[event.kind];
      if (next !== undefined) states[taskId] = next;
    }
    return states;
  }, [events, graph]);

  // t and tState are dependencies: switching language must relabel the graph nodes.
  const layout = useMemo(
    () => (graph === undefined ? { flowNodes: [], edges: [] } : layoutGraph(graph.nodes, liveStates, { t, tState })),
    [graph, liveStates, t, tState],
  );

  const selectedNode = useMemo(() => {
    if (graph === undefined || nodeId === null) return undefined;
    return graph.nodes.find((node) => node.id === nodeId);
  }, [graph, nodeId]);

  const selected = sessions.find((item) => item.id === selectedSessionId) ?? null;

  if (selectedSessionId === null) {
    return (
      <Panel title={t('graph.title')} subtitle={t('chat.noSession')}>
        <EmptyState
          title={t('common.pickSession')}
          hint={
            <button type="button" className="btn" onClick={() => setView('sessions')}>
              {t('common.goToSessions')}
            </button>
          }
        />
      </Panel>
    );
  }

  const stateOf = (node: TaskRecord): TaskState => liveStates[node.id] ?? node.state;

  return (
    <div className="stack">
      <Panel
        title={selected === null ? t('graph.title') : t('graph.titleWithSession', { title: selected.title })}
        subtitle={t('graph.subtitleDetail')}
        actions={
          <>
            {list.map((item) => (
              <button
                key={item.id}
                type="button"
                className={graph !== undefined && item.id === graph.id ? 'chip chip-active' : 'chip'}
                onClick={() => {
                  setGraphId(item.id);
                  setNodeId(null);
                }}
                title={item.title}
              >
                {t('graph.nodeCount', { n: item.nodes.length })}
              </button>
            ))}
            <button type="button" className="btn btn-ghost btn-small" onClick={graphs.reload}>
              {graphs.loading ? t('common.refreshing') : t('common.refresh')}
            </button>
          </>
        }
      >
        <ApiErrorBanner error={graphs.error} scope="GET /v1/sessions/{id}/graph" onRetry={graphs.reload} />

        {graph === undefined ? (
          <EmptyState
            title={t('graph.empty')}
            hint={
              <>
                {t('graph.emptyHint')}{' '}
                <button type="button" className="btn btn-small" onClick={() => setView('chat')}>
                  {t('common.goToChat')}
                </button>
              </>
            }
          />
        ) : (
          <>
            <div className="graph-stats">
              <Badge tone="info">{graph.title}</Badge>
              <span className="muted small">{t('graph.nodeCount', { n: graph.nodes.length })}</span>
              <span className="muted small">
                {t('graph.succeededCount', {
                  n: graph.nodes.filter((node) => stateOf(node) === 'succeeded').length,
                })}
              </span>
              <span className="muted small">
                {t('graph.failedCount', {
                  n: graph.nodes.filter((node) => stateOf(node) === 'failed').length,
                })}
              </span>
              <span className="muted small">{t('graph.updatedAt', { time: formatTime(graph.updated_at) })}</span>
            </div>
            <div className="graph-canvas">
              <ReactFlow
                nodes={layout.flowNodes}
                edges={layout.edges}
                fitView
                minZoom={0.2}
                nodesDraggable={false}
                nodesConnectable={false}
                onNodeClick={(_event, node) => setNodeId(node.id)}
              >
                <Background color="#30363d" gap={20} />
                <Controls showInteractive={false} />
                <MiniMap
                  pannable
                  zoomable
                  nodeColor={(node) => {
                    const background = node.style === undefined ? undefined : node.style.background;
                    return typeof background === 'string' ? background : '#30363d';
                  }}
                />
              </ReactFlow>
            </div>
            <div className="legend">
              {(Object.keys(STATE_STYLE) as TaskState[]).map((state) => (
                <span className="legend-item" key={state}>
                  <span
                    className="legend-swatch"
                    style={{ background: STATE_STYLE[state].background, borderColor: STATE_STYLE[state].border }}
                  />
                  {tState(state)}
                </span>
              ))}
            </div>
          </>
        )}
      </Panel>

      {selectedNode !== undefined ? (
        <Panel
          title={t('graph.nodeDetail')}
          subtitle={selectedNode.id}
          actions={
            <button type="button" className="btn btn-ghost btn-small" onClick={() => setNodeId(null)}>
              {t('common.close')}
            </button>
          }
        >
          <KeyValue
            rows={[
              ['title', selectedNode.title],
              ['state', <Badge tone="info">{tState(stateOf(selectedNode))}</Badge>],
              ['kind', KIND_LABEL[selectedNode.kind] ?? selectedNode.kind],
              ['attempts', selectedNode.attempts + '/' + selectedNode.max_attempts],
              ['timeout_ms', String(selectedNode.timeout_ms)],
              ['deps', selectedNode.deps.length === 0 ? t('common.none') : selectedNode.deps.join(', ')],
              [
                'duration',
                formatDuration(
                  selectedNode.started_at === null
                    ? null
                    : (selectedNode.finished_at ?? Date.now()) - selectedNode.started_at,
                ),
              ],
              ['error', selectedNode.error ?? '--'],
            ]}
          />
          <h3 className="section-title">{t('events.payload')}</h3>
          <JsonBlock value={selectedNode.payload} />
          <h3 className="section-title">{t('graph.result')}</h3>
          <JsonBlock value={selectedNode.result} empty={t('graph.noResult')} />
        </Panel>
      ) : null}
    </div>
  );
}
