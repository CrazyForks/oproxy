import React from 'react';

const { Icon, SurfaceShell, notifyError } = window;

const STORAGE_KEY = 'oproxy_assistant_config_v1';
const DEFAULT_CONFIG = {
  base_url: 'https://api.openai.com/v1',
  model: 'gpt-4.1-mini',
  api_key: '',
};

function loadConfig() {
  try {
    return { ...DEFAULT_CONFIG, ...(JSON.parse(sessionStorage.getItem(STORAGE_KEY) || '{}')) };
  } catch {
    return DEFAULT_CONFIG;
  }
}

function saveConfig(config) {
  sessionStorage.setItem(STORAGE_KEY, JSON.stringify(config));
}

function titleCase(value = '') {
  return value
    .replace(/[_/-]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .replace(/\b\w/g, ch => ch.toUpperCase());
}

function formatScalar(value) {
  if (value === true) return 'On';
  if (value === false) return 'Off';
  if (value === null || value === undefined || value === '') return 'None';
  return String(value);
}

function summarizeLocation(location = {}) {
  const parts = [];
  if (location.host) parts.push(`host ${location.host}`);
  if (location.path) parts.push(`path ${location.path}`);
  if (location.port) parts.push(`port ${location.port}`);
  if (location.methods?.length) parts.push(`methods ${location.methods.join(', ')}`);
  return parts.join(' · ') || 'all matching traffic';
}

function describeRuleActions(actions = []) {
  if (!Array.isArray(actions) || actions.length === 0) return [];
  return actions.map(action => {
    switch (action.type) {
      case 'set_header': return `Set header ${action.name} to ${action.value}`;
      case 'append_header': return `Append ${action.value} to header ${action.name}`;
      case 'remove_header': return `Remove header ${action.name}`;
      case 'set_query_param': return `Set query parameter ${action.name} to ${action.value}`;
      case 'remove_query_param': return `Remove query parameter ${action.name}`;
      case 'set_host': return `Set upstream host to ${action.value}`;
      case 'set_path': return `Rewrite path using ${action.pattern} -> ${action.replacement}`;
      case 'set_status': return `Set response status to ${action.code}`;
      case 'replace_body': return `Replace body text matching ${action.pattern}`;
      case 'redirect': return `Redirect to ${action.location} with status ${action.status}`;
      case 'block': return `Block with status ${action.status}`;
      default: return titleCase(action.type || 'rule action');
    }
  });
}

function buildActionPresentation(action) {
  const payload = action.payload || {};
  const details = [];
  let title = action.summary || titleCase(action.kind || 'Pending action');
  let subtitle = '';

  if (action.endpoint === '/admin/throttling') {
    title = payload.enabled ? 'Enable throttling' : 'Disable throttling';
    subtitle = payload.enabled
      ? `Limit bandwidth to ${formatScalar(payload.bandwidth_limit_kbps)} kbps with ${formatScalar(payload.latency_ms)} ms latency.`
      : 'Turn off traffic throttling.';
    details.push(['Status', payload.enabled ? 'Enabled' : 'Disabled']);
    details.push(['Latency', `${formatScalar(payload.latency_ms)} ms`]);
    details.push(['Bandwidth limit', `${formatScalar(payload.bandwidth_limit_kbps)} kbps`]);
  } else if (action.endpoint === '/admin/capture-filter') {
    title = 'Update capture filter';
    subtitle = `Set capture mode to ${titleCase(payload.mode || 'disabled')}.`;
    details.push(['Mode', titleCase(payload.mode || 'disabled')]);
    details.push(['Hosts', payload.hosts?.length ? payload.hosts.join(', ') : 'None']);
  } else if (action.endpoint === '/admin/upstream-proxy') {
    title = payload.upstream_proxy ? 'Set upstream proxy' : 'Clear upstream proxy';
    subtitle = payload.upstream_proxy || 'oproxy will connect directly.';
    details.push(['Proxy', payload.upstream_proxy || 'None']);
  } else if (action.endpoint === '/admin/dns') {
    title = 'Update DNS overrides';
    subtitle = `${Object.keys(payload).length} override${Object.keys(payload).length === 1 ? '' : 's'} will be saved.`;
    for (const [host, ip] of Object.entries(payload).slice(0, 6)) {
      details.push([host, ip]);
    }
  } else if (action.endpoint === '/admin/map-remote-rules') {
    title = payload.name || 'Create Map Remote rule';
    subtitle = `Route ${summarizeLocation(payload.location)} to ${payload.destination}.`;
    details.push(['Rule status', payload.enabled === false ? 'Disabled' : 'Enabled']);
    details.push(['Match', summarizeLocation(payload.location)]);
    details.push(['Destination', payload.destination]);
  } else if (action.endpoint === '/admin/map-local-rules') {
    title = payload.name || 'Create Map Local rule';
    subtitle = `Serve local content for ${summarizeLocation(payload.location)}.`;
    details.push(['Rule status', payload.enabled === false ? 'Disabled' : 'Enabled']);
    details.push(['Match', summarizeLocation(payload.location)]);
    details.push(['File path', payload.file_path]);
  } else if (action.endpoint === '/admin/rule-sets') {
    title = payload.name || 'Create rewrite rule';
    subtitle = `Apply ${payload.actions?.length || 0} rewrite action${payload.actions?.length === 1 ? '' : 's'} to ${summarizeLocation(payload.location)}.`;
    details.push(['Rule status', payload.enabled === false ? 'Disabled' : 'Enabled']);
    details.push(['Applies to', titleCase(payload.applies_to || 'both')]);
    details.push(['Match', summarizeLocation(payload.location)]);
    for (const actionText of describeRuleActions(payload.actions).slice(0, 4)) {
      details.push(['Action', actionText]);
    }
  } else if (action.endpoint === '/admin/access-rules') {
    title = payload.name || `${titleCase(payload.action || 'Access')} traffic`;
    subtitle = `${titleCase(payload.action || 'access')} ${summarizeLocation(payload.location)}.`;
    details.push(['Rule status', payload.enabled === false ? 'Disabled' : 'Enabled']);
    details.push(['Action', titleCase(payload.action)]);
    details.push(['Match', summarizeLocation(payload.location)]);
  } else if (action.endpoint === '/admin/mock/rules') {
    title = payload.name || 'Create mock rule';
    subtitle = `Return mocked responses for ${summarizeLocation(payload.location)}.`;
    details.push(['Rule status', payload.enabled === false ? 'Disabled' : 'Enabled']);
    details.push(['Match', summarizeLocation(payload.location)]);
    details.push(['Responses', `${payload.responses?.length || 0}`]);
  } else if (action.endpoint === '/admin/webhooks') {
    title = payload.name || 'Create webhook';
    subtitle = `Send ${payload.events?.join(', ') || 'events'} to ${payload.url}.`;
    details.push(['Webhook status', payload.enabled === false ? 'Disabled' : 'Enabled']);
    details.push(['URL', payload.url]);
    details.push(['Events', payload.events?.join(', ') || 'None']);
  } else if (action.endpoint === '/admin/forward') {
    title = 'Send request';
    subtitle = `${payload.method || 'GET'} ${payload.url}`;
    details.push(['Method', payload.method || 'GET']);
    details.push(['URL', payload.url]);
    details.push(['Body', payload.body ? 'Included' : 'None']);
  } else if (action.endpoint === '/admin/playback') {
    title = 'Replay captured traffic';
    subtitle = 'Run playback against captured sessions.';
  } else if (action.endpoint === '/admin/sessions' && action.method === 'DELETE') {
    title = 'Clear captured sessions';
    subtitle = 'Delete all captured session history.';
  } else {
    subtitle = action.summary || `${action.method} ${action.endpoint}`;
  }
  if (action.preconditions?.length) {
    details.push(['State check', `${action.preconditions.length} current-state check${action.preconditions.length === 1 ? '' : 's'}`]);
  }

  return {
    title,
    subtitle,
    details: details.filter(([_, value]) => value !== undefined && value !== ''),
  };
}

function PendingActionCard({ action, busy, onDismiss, onApply }) {
  const presentation = buildActionPresentation(action);
  return (
    <div className={`assistant-action risk-${action.risk}`}>
      <div className="assistant-action-head">
        <span>{action.risk}</span>
        <b>{presentation.title}</b>
      </div>
      {presentation.subtitle && <p>{presentation.subtitle}</p>}
      {presentation.details.length > 0 && (
        <dl className="assistant-action-details">
          {presentation.details.map(([label, value], idx) => (
            <React.Fragment key={`${label}-${idx}`}>
              <dt>{label}</dt>
              <dd>{formatScalar(value)}</dd>
            </React.Fragment>
          ))}
        </dl>
      )}
      <details className="assistant-action-technical">
        <summary>Technical details</summary>
        <code>{action.method} {action.endpoint}</code>
        <pre>{JSON.stringify(action.payload, null, 2)}</pre>
      </details>
      <div className="assistant-action-buttons">
        <button className="btn sm ghost" disabled={busy} onClick={onDismiss}>Dismiss</button>
        <button className="btn sm primary" disabled={busy} onClick={onApply}>Apply</button>
      </div>
    </div>
  );
}

function AssistantSurface({ onRefresh, onWorkspaceChanged, uiState, activeSurface = 'sessions', mode = 'surface', onClose }) {
  const [config, setConfig] = React.useState(loadConfig);
  const [messages, setMessages] = React.useState([
    {
      role: 'assistant',
      content: 'Tell me what you want to inspect, change, or see in the UI. I can apply Sessions filters directly, read oproxy state, and I will ask before applying any backend change.',
    },
  ]);
  const [draft, setDraft] = React.useState('');
  const [toolEvents, setToolEvents] = React.useState([]);
  const [proposedActions, setProposedActions] = React.useState([]);
  const [busy, setBusy] = React.useState(false);

  React.useEffect(() => {
    saveConfig(config);
  }, [config]);

  const updateConfig = (key, value) => setConfig(prev => ({ ...prev, [key]: value }));

  const send = async (event) => {
    event?.preventDefault?.();
    const text = draft.trim();
    if (!text || busy) return;

    const nextMessages = [...messages, { role: 'user', content: text }];
    setMessages(nextMessages);
    setDraft('');

    setBusy(true);
    setToolEvents([]);
    setProposedActions([]);

    try {
      const res = await fetch('/admin/assistant/chat', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          provider: { base_url: config.base_url, model: config.model },
          api_key: config.api_key,
          messages: nextMessages.filter(m => m.role === 'user' || m.role === 'assistant').slice(-12),
          client_context: { active_surface: activeSurface, ui_state: uiState || {} },
        }),
      });
      const body = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(body.error || `HTTP ${res.status}`);
      setMessages(prev => [...prev, { role: 'assistant', content: body.message || 'I finished the request.' }]);
      setToolEvents(body.tool_events || []);
      setProposedActions(body.proposed_actions || []);
      if ((body.tool_events || []).some(event => event.category === 'ui' && event.status === 'ok')) {
        onWorkspaceChanged?.();
      }
    } catch (err) {
      notifyError(err.message || err);
      setMessages(prev => [...prev, { role: 'assistant', content: `Assistant request failed: ${err.message || err}` }]);
    } finally {
      setBusy(false);
    }
  };

  const executeAction = async (action) => {
    setBusy(true);
    try {
      const res = await fetch('/admin/assistant/actions/execute', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          action_id: action.action_id,
          confirmation_token: action.confirmation_token,
        }),
      });
      const body = await res.json().catch(() => ({}));
      if (!res.ok) throw new Error(body.error || `HTTP ${res.status}`);
      setProposedActions(prev => prev.filter(a => a.action_id !== action.action_id));
      setMessages(prev => [...prev, {
        role: 'assistant',
        content: `Applied: ${action.summary}`,
      }]);
      onRefresh?.(body.refreshed_resources || []);
    } catch (err) {
      notifyError(err.message || err);
      setMessages(prev => [...prev, {
        role: 'assistant',
        content: `Apply failed: ${err.message || err}`,
      }]);
    } finally {
      setBusy(false);
    }
  };

  const dismissAction = async (action) => {
    setProposedActions(prev => prev.filter(a => a.action_id !== action.action_id));
    try {
      await fetch('/admin/assistant/actions/cancel', {
        method: 'POST',
        headers: { 'Content-Type': 'application/json' },
        body: JSON.stringify({
          action_id: action.action_id,
          confirmation_token: action.confirmation_token,
        }),
      });
    } catch {
      // The server-side pending action also expires automatically.
    }
  };

  const actions = (
    <div className="assistant-provider">
      <input className="cmp-input" aria-label="Provider base URL" value={config.base_url} onChange={e => updateConfig('base_url', e.target.value)} placeholder="https://api.openai.com/v1" />
      <input className="cmp-input" aria-label="Model" value={config.model} onChange={e => updateConfig('model', e.target.value)} placeholder="model" />
      <input className="cmp-input" aria-label="API key" type="password" value={config.api_key} onChange={e => updateConfig('api_key', e.target.value)} placeholder="API key stays in this tab" />
    </div>
  );

  const content = (
      <div className="assistant-grid">
        <div className="assistant-chat">
          <div className="assistant-thread">
            {messages.map((message, idx) => (
              <div key={idx} className={`assistant-msg ${message.role}`}>
                <div className="assistant-role">{message.role === 'user' ? 'You' : 'Assistant'}</div>
                <div className="assistant-bubble">{message.content}</div>
              </div>
            ))}
            {busy && <div className="assistant-msg assistant"><div className="assistant-role">Assistant</div><div className="assistant-bubble">Working...</div></div>}
          </div>
          <form className="assistant-input" onSubmit={send}>
            <textarea
              className="cmp-textarea"
              aria-label="Assistant message"
              value={draft}
              onChange={e => setDraft(e.target.value)}
              onKeyDown={e => {
                if ((e.metaKey || e.ctrlKey) && e.key === 'Enter') send(e);
              }}
              placeholder="Ask: show failed requests, create a mock, add a DNS override..."
            />
            <button className="btn primary" type="submit" disabled={busy || !draft.trim()}>
              <Icon name="bolt" size={13} stroke={1.8} /> Send
            </button>
          </form>
        </div>

        <aside className="assistant-side">
          <div className="assistant-panel">
            <div className="assistant-panel-title">Tool events</div>
            {toolEvents.length === 0 && <div className="assistant-empty">No tool calls yet.</div>}
            {toolEvents.map((event, idx) => (
              <div key={idx} className={`assistant-event ${event.status}`}>
                <span>{event.name}</span>
                <b>{event.status}</b>
                {event.summary && <small>{event.summary}</small>}
              </div>
            ))}
          </div>

          <div className="assistant-panel">
            <div className="assistant-panel-title">Pending actions</div>
            {proposedActions.length === 0 && <div className="assistant-empty">Changes will appear here for review.</div>}
            {proposedActions.map(action => (
              <PendingActionCard
                key={action.action_id}
                action={action}
                busy={busy}
                onDismiss={() => dismissAction(action)}
                onApply={() => executeAction(action)}
              />
            ))}
          </div>
        </aside>
      </div>
  );

  if (mode === 'drawer') {
    return (
      <div className="assistant-drawer-shell">
        <div className="assistant-drawer-head">
          <div>
            <h2>Assistant</h2>
            <p>Ask me to navigate, filter Sessions, explain setup, or prepare confirmed changes.</p>
          </div>
          <button className="icon-btn" type="button" aria-label="Close assistant" onClick={onClose}>×</button>
        </div>
        {actions}
        {content}
      </div>
    );
  }

  return (
    <SurfaceShell title="Assistant" sub="chat-driven control plane · confirmations required for changes" actions={actions}>
      {content}
    </SurfaceShell>
  );
}

window.AssistantSurface = AssistantSurface;
