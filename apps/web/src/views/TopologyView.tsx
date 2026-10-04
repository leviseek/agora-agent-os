/** View 7 - Topology: workers -> actors -> sessions, plus the directory cache counters. */

import { useMemo, useState } from 'react';
import { Background, Controls, MarkerType, ReactFlow } from '@xyflow/react';
import type { Edge, Node } from '@xyflow/react';
import { ApiErrorBanner, Badge, EmptyState, KeyValue, Panel } from '../components';
import { formatBytes, formatDateTime, percent } from '../format';
import { useAsyncData } from '../hooks';
import { useI18n } from '../i18n';
import { useNav } from '../navigation';
import { useApp } from '../store';
import type { ActorRecord, ActorsResponse, DirectoryEntry, WorkerRecord, WorkersResponse } from '../api';

const WORKER_STYLE = { background: '#12283d', border: '#1f6feb' };
const ACTOR_STYLE = { background: '#2a2135', border: '#8957e5' };
const SESSION_STYLE = { background: '#0f2f1d', border: '#3fb950' };

function boxStyle(style: { background: string; border: string }): Record<string, string | number> {
  return {
    width: 230,
    background: style.background,
    border: '1px solid ' + style.border,
    borderRadius: 10,
    color: '#e6edf3',
    padding: 8,
    fontSize: 12,
  };
}

/** Translators handed to the graph builder so node captions follow the language switch. */
interface FlowLabels {
  t: (key: string, params?: Record<string, string | number>) => string;
  tState: (value: string) => string;
}

export function TopologyView() {
  const { t, tState } = useI18n();
  const { client, sessions, selectedSessionId, selectSession } = useApp();
  const { setView } = useNav();
  const [nodeId, setNodeId] = useState<string | null>(null);

  const workers = useAsyncData<WorkersResponse>(() => client.listWorkers(), [client], { intervalMs: 5000 });
  const actors = useAsyncData<ActorsResponse>(() => client.listActors(), [client], { intervalMs: 5000 });

  const workerList: WorkerRecord[] = workers.data?.workers ?? [];
  const actorList: ActorRecord[] = actors.data?.actors ?? [];
  const directory: DirectoryEntry[] = actors.data?.directory ?? [];
  const cache = actors.data?.cache ?? { hits: 0, misses: 0, entries: 0 };

  // t and tState are dependencies: switching language must relabel the graph nodes.
  const { flowNodes, edges } = useMemo(() => {
    const labels: FlowLabels = { t, tState };
    const nodes: Node[] = [];
    const links: Edge[] = [];

    workerList.forEach((worker, index) => {
      nodes.push({
        id: 'worker:' + worker.id,
        position: { x: 20, y: 30 + index * 130 },
        data: {
          label: (
            <div className="flow-node-body">
              <div className="flow-node-title">{worker.name}</div>
              <div className="flow-node-meta">
                <span className={'flow-chip flow-chip-' + worker.state}>{labels.tState(worker.state)}</span>
                <span className="flow-node-kind">{worker.addr}</span>
              </div>
              <div className="flow-node-sub">
                {labels.t('topology.workerLoad', {
                  actors: worker.load.actors,
                  max: worker.capacity.max_actors,
                  tasks: worker.load.running_tasks,
                })}
              </div>
            </div>
          ),
        },
        style: boxStyle(WORKER_STYLE),
      });
    });

    actorList.forEach((actor, index) => {
      nodes.push({
        id: 'actor:' + actor.id,
        position: { x: 330, y: 30 + index * 130 },
        data: {
          label: (
            <div className="flow-node-body">
              <div className="flow-node-title">{labels.t('topology.actorTitle', { kind: actor.kind })}</div>
              <div className="flow-node-meta">
                <span className={'flow-chip flow-chip-' + actor.state}>{labels.tState(actor.state)}</span>
                <span className="flow-node-kind">
                  {labels.t('topology.generation', { n: actor.generation })}
                </span>
              </div>
              <div className="flow-node-sub">
                {labels.t('topology.mailbox', { depth: actor.mailbox_depth, seq: actor.last_applied_seq })}
              </div>
            </div>
          ),
        },
        style: boxStyle(ACTOR_STYLE),
      });
      if (actor.worker_id !== null) {
        links.push({
          id: 'worker:' + actor.worker_id + '->actor:' + actor.id,
          source: 'worker:' + actor.worker_id,
          target: 'actor:' + actor.id,
          style: { stroke: '#1f6feb' },
          markerEnd: { type: MarkerType.ArrowClosed, color: '#1f6feb' },
        });
      }
    });

    sessions.forEach((session, index) => {
      nodes.push({
        id: 'session:' + session.id,
        position: { x: 640, y: 30 + index * 130 },
        data: {
          label: (
            <div className="flow-node-body">
              <div className="flow-node-title">{session.title}</div>
              <div className="flow-node-meta">
                <span className={'flow-chip flow-chip-' + session.state}>{labels.tState(session.state)}</span>
                <span className="flow-node-kind">{session.user_id}</span>
              </div>
              <div className="flow-node-sub">
                {labels.t('topology.messageCount', { n: session.message_count })}
              </div>
            </div>
          ),
        },
        style: boxStyle(SESSION_STYLE),
      });
      links.push({
        id: 'actor:' + session.actor_id + '->session:' + session.id,
        source: 'actor:' + session.actor_id,
        target: 'session:' + session.id,
        animated: session.id === selectedSessionId,
        style: { stroke: '#3fb950' },
        markerEnd: { type: MarkerType.ArrowClosed, color: '#3fb950' },
      });
    });

    // Directory rows for actors that are not in the live actor list (a stale-but-routable cache entry).
    for (const entry of directory) {
      if (actorList.some((actor) => actor.id === entry.actor_id)) continue;
      nodes.push({
        id: 'actor:' + entry.actor_id,
        position: { x: 330, y: 30 + (actorList.length + directory.indexOf(entry)) * 130 },
        data: {
          label: (
            <div className="flow-node-body">
              <div className="flow-node-title">
                {labels.t('topology.directoryActorTitle', { kind: entry.kind })}
              </div>
              <div className="flow-node-meta">
                <span className={'flow-chip flow-chip-' + entry.state}>{labels.tState(entry.state)}</span>
                <span className="flow-node-kind">
                  {labels.t('topology.generation', { n: entry.generation })}
                </span>
              </div>
              <div className="flow-node-sub">{entry.node_id ?? labels.t('topology.unknownNode')}</div>
            </div>
          ),
        },
        style: boxStyle(ACTOR_STYLE),
      });
    }

    return { flowNodes: nodes, edges: links };
  }, [workerList, actorList, directory, sessions, selectedSessionId, t, tState]);

  const detail = useMemo(() => {
    if (nodeId === null) return null;
    const [kind, id] = nodeId.split(':', 2);
    if (kind === 'worker') return { kind, worker: workerList.find((item) => item.id === id) ?? null, actor: null, session: null };
    if (kind === 'actor') return { kind, worker: null, actor: actorList.find((item) => item.id === id) ?? null, session: null };
    if (kind === 'session') return { kind, worker: null, actor: null, session: sessions.find((item) => item.id === id) ?? null };
    return null;
  }, [nodeId, workerList, actorList, sessions]);

  const hitRate = percent(cache.hits, cache.hits + cache.misses);

  return (
    <div className="stack">
      <Panel
        title={t('topology.title')}
        subtitle={t('topology.subtitleDetail')}
        actions={
          <>
            <button type="button" className="btn btn-ghost btn-small" onClick={workers.reload}>
              {workers.loading ? t('common.refreshing') : t('common.refresh')}
            </button>
          </>
        }
      >
        <ApiErrorBanner error={workers.error} scope="GET /v1/workers" onRetry={workers.reload} />
        <ApiErrorBanner error={actors.error} scope="GET /v1/actors" onRetry={actors.reload} />

        <div className="graph-stats">
          <Badge tone="info">{t('topology.workerCount', { n: workerList.length })}</Badge>
          <Badge tone="info">{t('topology.actorCount', { n: actorList.length })}</Badge>
          <Badge tone="info">{t('topology.directoryCount', { n: directory.length })}</Badge>
          <Badge tone="muted">{t('sessions.count', { n: sessions.length })}</Badge>
        </div>

        <KeyValue
          rows={[
            [t('topology.cacheHits'), String(cache.hits)],
            [t('topology.cacheMisses'), String(cache.misses)],
            [t('topology.cacheEntries'), String(cache.entries)],
            [t('topology.hitRate'), hitRate],
          ]}
        />

        {flowNodes.length === 0 ? (
          <EmptyState title={t('topology.empty')} hint={t('topology.emptyHint')} />
        ) : (
          <div className="graph-canvas">
            <ReactFlow
              nodes={flowNodes}
              edges={edges}
              fitView
              minZoom={0.2}
              nodesDraggable={false}
              nodesConnectable={false}
              onNodeClick={(_event, node) => {
                setNodeId(node.id);
                if (node.id.startsWith('session:')) selectSession(node.id.slice('session:'.length));
              }}
            >
              <Background color="#30363d" gap={20} />
              <Controls showInteractive={false} />
            </ReactFlow>
          </div>
        )}
      </Panel>

      {detail !== null ? (
        <Panel
          title={t('topology.detailTitle', { kind: detail.kind })}
          subtitle={nodeId ?? ''}
          actions={
            <>
              {detail.kind === 'session' ? (
                <button type="button" className="btn btn-small" onClick={() => setView('chat')}>
                  {t('topology.openInChat')}
                </button>
              ) : null}
              <button type="button" className="btn btn-ghost btn-small" onClick={() => setNodeId(null)}>
                {t('common.close')}
              </button>
            </>
          }
        >
          {detail.worker !== null ? (
            <KeyValue
              rows={[
                ['id', detail.worker.id],
                ['node_id', detail.worker.node_id],
                ['state', <Badge tone="ok">{tState(detail.worker.state)}</Badge>],
                ['addr', detail.worker.addr],
                ['version', detail.worker.version],
                [
                  'capacity',
                  t('topology.capacityValue', {
                    actors: detail.worker.capacity.max_actors,
                    tasks: detail.worker.capacity.max_tasks,
                  }),
                ],
                [
                  'load',
                  t('topology.loadValue', {
                    actors: detail.worker.load.actors,
                    tasks: detail.worker.load.running_tasks,
                    cpu: detail.worker.load.cpu_percent.toFixed(1),
                    mem: formatBytes(detail.worker.load.memory_bytes),
                  }),
                ],
                ['capabilities', String(detail.worker.capabilities.length)],
                ['labels', Object.entries(detail.worker.labels).map(([k, v]) => k + '=' + v).join(', ') || '--'],
                ['registered_at', formatDateTime(detail.worker.registered_at)],
                ['last_heartbeat', formatDateTime(detail.worker.last_heartbeat)],
              ]}
            />
          ) : null}

          {detail.actor !== null ? (
            <KeyValue
              rows={[
                ['id', detail.actor.id],
                ['session_id', detail.actor.session_id],
                ['kind', detail.actor.kind],
                ['state', <Badge tone="info">{tState(detail.actor.state)}</Badge>],
                ['worker_id', detail.actor.worker_id ?? t('sessions.unassigned')],
                ['generation', String(detail.actor.generation)],
                ['mailbox_depth', String(detail.actor.mailbox_depth)],
                ['last_applied_seq', String(detail.actor.last_applied_seq)],
                ['created_at', formatDateTime(detail.actor.created_at)],
                ['updated_at', formatDateTime(detail.actor.updated_at)],
              ]}
            />
          ) : null}

          {detail.session !== null ? (
            <KeyValue
              rows={[
                ['id', detail.session.id],
                ['title', detail.session.title],
                ['state', <Badge tone="info">{tState(detail.session.state)}</Badge>],
                ['user_id', detail.session.user_id],
                ['actor_id', detail.session.actor_id],
                ['message_count', String(detail.session.message_count)],
                ['created_at', formatDateTime(detail.session.created_at)],
                ['updated_at', formatDateTime(detail.session.updated_at)],
              ]}
            />
          ) : null}

          {detail.worker === null && detail.actor === null && detail.session === null ? (
            <p className="muted">{t('topology.staleNode')}</p>
          ) : null}
        </Panel>
      ) : null}
    </div>
  );
}
