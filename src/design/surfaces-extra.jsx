import React from 'react';
const { Icon, Toggle, SurfaceShell, fetchJson, sendJson, notifyError, ask, formDialog, confirmAction, nonEmpty, Modal, LocationEditor } = window;
/* Additional surfaces: Mock / Lua / Webhooks / Settings / DNS / Capture / Shortcuts modal */

// ─── Mock modal ────────────────────────────────────────────────────────

const EMPTY_MOCK_LOC = { host: null, path: null, port: null, protocol: null, query: null, methods: [], mode: 'glob' };

function MockModal({ rule, onClose, onSave }) {
  const isNew = !rule;
  const first = rule?.responses?.[0] || { status: 200, headers: {}, body: '', delay_ms: 0 };
  const [name, setName] = React.useState(rule?.name || '');
  const [loc, setLoc] = React.useState(rule?.location || EMPTY_MOCK_LOC);
  const [status, setStatus] = React.useState(String(first.status || 200));
  const [ct, setCt] = React.useState(first.headers?.['content-type'] || first.headers?.['Content-Type'] || 'application/json');
  const [body, setBody] = React.useState(first.body || '{"ok":true}');
  const lbl = { fontSize: 12, color: 'var(--text-faint)', whiteSpace: 'nowrap' };
  const save = async () => {
    try { await onSave({ name, loc, status: Number(status || 200), contentType: ct, body }); }
    catch (e) { notifyError(e.message || e); }
  };
  return (
    <Modal title={isNew ? 'Add mock response' : `Edit — ${rule.name}`} onClose={onClose} onSave={save}>
      {/* Name row — full width */}
      <div style={{ display: 'flex', gap: 8, alignItems: 'center', marginBottom: 8 }}>
        <input className="cmp-input" style={{ flex: 1 }} value={name} onChange={e => setName(e.target.value)}
               placeholder={isNew ? 'Name (optional)' : 'Name'} autoFocus />
      </div>
      {/* Location — shown for new rules; on edit, location is kept as-is */}
      {isNew
        ? <LocationEditor loc={loc} onChange={setLoc} />
        : <div style={{ fontSize: 11, color: 'var(--text-faint)', marginBottom: 8 }}>
            Scope: {[rule.location?.host, rule.location?.path].filter(Boolean).join(' ') || 'any'}
          </div>
      }
      {/* Status + Content-Type */}
      <div style={{ display: 'grid', gridTemplateColumns: 'max-content 80px max-content 1fr', gap: '6px 8px', alignItems: 'center', marginBottom: 8 }}>
        <span style={lbl}>Status</span>
        <input className="cmp-input" type="number" min="100" max="599" value={status} onChange={e => setStatus(e.target.value)} />
        <span style={lbl}>Content-Type</span>
        <input className="cmp-input" value={ct} onChange={e => setCt(e.target.value)} placeholder="application/json" />
      </div>
      {/* Body */}
      <div style={{ display: 'grid', gridTemplateColumns: 'max-content 1fr', gap: '6px 8px', alignItems: 'start' }}>
        <span style={{ ...lbl, paddingTop: 5 }}>Body</span>
        <textarea className="cmp-input" rows={6} value={body} onChange={e => setBody(e.target.value)}
                  placeholder='{"ok": true}' style={{ resize: 'vertical', fontFamily: 'var(--font-mono)', fontSize: 12 }} />
      </div>
    </Modal>
  );
}

// ─── Mock Server ───────────────────────────────────────────────────────
const INITIAL_MOCK_RULES = [];

function MockSurface() {
  const [rules, setRules] = React.useState(INITIAL_MOCK_RULES);
  const [expanded, setExpanded] = React.useState(null);
  const [mockEdit, setMockEdit] = React.useState(undefined); // undefined=closed, null=new, obj=editing
  const load = React.useCallback(() => fetchJson('/admin/mock/rules', []).then(setRules), []);
  React.useEffect(() => { load(); }, [load]);
  const toggle = async (id) => {
    const rule = rules.find(r => r.id === id);
    if (!rule) return;
    await sendJson(`/admin/mock/rules/${encodeURIComponent(id)}`, 'PUT', { ...rule, enabled: !rule.enabled }).catch(e => notifyError(e.message || e));
    await load();
  };
  const saveMock = async ({ name, loc, status, contentType, body }) => {
    if (mockEdit) {
      // Edit existing — update first response; location unchanged
      const first = mockEdit.responses?.[0] || { status: 200, headers: {}, body: '', delay_ms: 0 };
      await sendJson(`/admin/mock/rules/${encodeURIComponent(mockEdit.id)}`, 'PUT', {
        ...mockEdit,
        name: name || mockEdit.name,
        responses: [{ ...first, status, headers: { ...(first.headers || {}), 'content-type': contentType }, body }],
      }).catch(e => notifyError(e.message || e));
    } else {
      // New — send full Location struct
      const location = loc || EMPTY_MOCK_LOC;
      await sendJson('/admin/mock/rules', 'POST', {
        id: '',
        name: name || `Mock ${location.path || '*'}`,
        enabled: true,
        location,
        responses: [{ status, headers: { 'content-type': contentType }, body, delay_ms: 0 }],
        call_count: 0,
      }).catch(e => notifyError(e.message || e));
    }
    setMockEdit(undefined);
    await load();
  };
  const deleteMock = async (rule) => {
    if (!await confirmAction('Delete this mock rule?', 'Delete', 'danger')) return;
    await fetch(`/admin/mock/rules/${encodeURIComponent(rule.id)}`, { method: 'DELETE' }).catch(e => notifyError(e.message || e));
    await load();
  };
  const resetMock = async (rule) => {
    await fetch(`/admin/mock/rules/${encodeURIComponent(rule.id)}/reset`, { method: 'POST' }).catch(e => notifyError(e.message || e));
    await load();
  };
  const totalCalls = rules.reduce((a, r) => a + (r.call_count || 0), 0);
  return (
    <SurfaceShell title="Mock Server"
                  sub={`${rules.filter(r => r.enabled).length} active · ${totalCalls} mock responses served`}
                  actions={<>
                    <button className="btn primary" onClick={() => setMockEdit(null)}>＋ Add mock</button>
                  </>}>
      <div className="rule-head" style={{ gridTemplateColumns: '36px 1fr 80px 220px 120px 80px 100px' }}>
        <div></div><div>Name / scope</div><div>Methods</div><div>Path</div><div>Responses</div><div style={{ textAlign: 'right' }}>Calls</div><div></div>
      </div>
      {rules.length === 0 && <div className="empty">No mock rules are configured.</div>}
      {rules.map(r => {
        const loc = r.location || {};
        const methods = loc.methods?.length ? loc.methods.join(', ') : '*';
        const firstMethod = loc.methods?.[0] || 'GET';
        return (
        <React.Fragment key={r.id}>
          <div className={'rule-row' + (r.enabled ? '' : ' off')} style={{ gridTemplateColumns: '36px 1fr 80px 220px 120px 80px 100px' }}>
            <div className="col-toggle"><Toggle label={`Toggle mock rule ${r.name}`} on={r.enabled} onChange={() => toggle(r.id)} /></div>
            <div className="col-match">
              <div style={{ color: 'var(--text-hi)', fontFamily: 'var(--font-sans)', fontSize: 12, fontWeight: 500 }}>{r.name}</div>
              <div className="dim" style={{ fontSize: 11 }}>{loc.host || 'any host'}</div>
            </div>
            <div><span className="cell-method" data-m={firstMethod}>{methods}</span></div>
            <div className="col-match" style={{ color: 'var(--c-3xx)' }}>{loc.path || '.*'}</div>
            <div className="col-meta">
              {r.responses.length} response{r.responses.length === 1 ? '' : 's'}
              {r.responses.length > 1 && <span className="mute"> · weighted</span>}
            </div>
            <div className="cell-num" style={{ fontFamily: 'var(--font-mono)' }}>{(r.call_count || 0).toLocaleString()} <button className="copy-btn" onClick={() => resetMock(r)} aria-label={`Reset mock call count for ${r.name}`}>↺</button></div>
            <div className="col-act">
              <button className="copy-btn" onClick={() => setExpanded(expanded === r.id ? null : r.id)} aria-expanded={expanded === r.id} aria-label={`${expanded === r.id ? 'Hide' : 'Show'} mock responses for ${r.name}`}>
                {expanded === r.id ? 'hide' : 'show'}
              </button>
              <button className="copy-btn" onClick={() => setMockEdit(r)} aria-label={`Edit mock rule ${r.name}`}>edit</button>
              <button className="copy-btn" onClick={() => deleteMock(r)} aria-label={`Delete mock rule ${r.name}`}>×</button>
            </div>
          </div>
          {expanded === r.id && (
            <div style={{ background: 'var(--surface-2)', padding: '12px 16px 14px', borderBottom: '1px solid var(--border)' }}>
              {r.responses.map((res, i) => (
                <div key={i} style={{ display: 'grid', gridTemplateColumns: '60px 80px 100px 1fr', alignItems: 'center', gap: 12, fontFamily: 'var(--font-mono)', fontSize: 11.5, padding: '6px 0' }}>
                  <span className="dim">variant {i + 1}</span>
                  <span className="cell-status" data-c={String(res.status)[0]}>{res.status}</span>
                  <span className="dim">+{res.delay_ms || 0} ms</span>
                  <code style={{ background: 'var(--bg-deep)', padding: '4px 8px', borderRadius: 4, color: 'var(--text)', overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' }}>{res.body}</code>
                </div>
              ))}
            </div>
          )}
        </React.Fragment>
      ); })}
      {mockEdit !== undefined && (
        <MockModal rule={mockEdit} onClose={() => setMockEdit(undefined)} onSave={saveMock} />
      )}
    </SurfaceShell>
  );
}

// ─── Lua scripts ───────────────────────────────────────────────────────
const LUA_SAMPLE = '';

function LuaSurface() {
  const [scripts, setScripts] = React.useState([]);
  const [activeId, setActiveId] = React.useState(null);
  const [code, setCode] = React.useState('');
  const active = scripts.find(s => s.id === activeId);
  const load = React.useCallback(async () => {
    const data = await fetchJson('/admin/scripts', []);
    setScripts(data || []);
    if (data?.length && (!activeId || !data.some(s => s.id === activeId))) {
      const next = data[0];
      setActiveId(next.id);
      setCode(next.code || '');
    }
  }, [activeId]);

  React.useEffect(() => { load(); }, [load]);
  React.useEffect(() => { setCode(active?.code || ''); }, [activeId]);

  const newScript = async () => {
    const name = await ask('Script name', `Script ${scripts.length + 1}`);
    if (!nonEmpty(name)) return;
    const script = { id: '', name, enabled: true, code: '-- Lua 5.4\n-- abort(403, "blocked")\n' };
    await sendJson('/admin/scripts', 'POST', script).catch(e => notifyError(e.message || e));
    const data = await fetchJson('/admin/scripts', []);
    setScripts(data || []);
    const created = [...(data || [])].reverse().find(s => s.name === name) || data?.[0];
    if (created) {
      setActiveId(created.id);
      setCode(created.code || '');
    }
  };
  const toggleScript = async (script) => {
    await sendJson(`/admin/scripts/${encodeURIComponent(script.id)}`, 'PUT', { ...script, enabled: !script.enabled }).catch(e => notifyError(e.message || e));
    await load();
  };
  const saveScript = async () => {
    if (!active) return;
    await sendJson(`/admin/scripts/${encodeURIComponent(active.id)}`, 'PUT', { ...active, code }).catch(e => notifyError(e.message || e));
    await load();
  };
  const renameScript = async () => {
    if (!active) return;
    const name = await ask('Rename script', active.name);
    if (!nonEmpty(name) || name === active.name) return;
    await sendJson(`/admin/scripts/${encodeURIComponent(active.id)}`, 'PUT', { ...active, name, code }).catch(e => notifyError(e.message || e));
    await load();
  };
  const deleteScript = async () => {
    if (!active || !await confirmAction('Delete this Lua script?', 'Delete', 'danger')) return;
    await fetch(`/admin/scripts/${encodeURIComponent(active.id)}`, { method: 'DELETE' }).catch(e => notifyError(e.message || e));
    setActiveId(null);
    setCode('');
    await load();
  };

  return (
    <SurfaceShell title="Lua scripts"
                  sub="sandboxed Lua 5.4 · runs after rewrite middleware"
                  actions={<button className="btn primary" onClick={newScript}>＋ New script</button>}>
      <div style={{ display: 'grid', gridTemplateColumns: '260px 1fr', height: '100%' }}>
        <div style={{ borderRight: '1px solid var(--border)', overflow: 'auto' }}>
          {scripts.length === 0 && <div className="empty" style={{ minHeight: 160 }}>No Lua scripts are configured.</div>}
          {scripts.map(s => (
            <div key={s.id}
                 onClick={() => setActiveId(s.id)}
                 style={{ padding: '10px 14px', borderBottom: '1px solid var(--border-soft)', cursor: 'pointer',
                          background: activeId === s.id ? 'var(--row-selected)' : 'transparent',
                          boxShadow: activeId === s.id ? 'inset 2px 0 0 var(--row-selected-border)' : 'none' }}>
              <div style={{ display: 'flex', alignItems: 'center', gap: 6 }}>
                <Toggle on={s.enabled} onChange={() => toggleScript(s)} />
                <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: s.enabled ? 'var(--text-hi)' : 'var(--text-low)', flex: 1 }}>{s.name}</span>
              </div>
              <div style={{ display: 'flex', gap: 8, marginTop: 4, fontSize: 10.5, color: 'var(--text-faint)', fontFamily: 'var(--font-mono)' }}>
                <span>{(s.code || '').split('\n').length} lines</span>
              </div>
            </div>
          ))}
        </div>

        {!active ? (
          <div className="empty" style={{ flex: 1, display: 'flex', flexDirection: 'column', alignItems: 'center', justifyContent: 'center', gap: 8, color: 'var(--text-mid)' }}>
            <span style={{ fontSize: 13 }}>No script selected</span>
            <span style={{ fontSize: 11, color: 'var(--text-faint)' }}>Create a script with + New script or select one from the list</span>
          </div>
        ) : (
          <div style={{ display: 'flex', flexDirection: 'column', minHeight: 0 }}>
            <div style={{ display: 'flex', alignItems: 'center', padding: '8px 14px', gap: 10, borderBottom: '1px solid var(--border)', background: 'var(--surface)' }}>
              <span style={{ fontFamily: 'var(--font-mono)', fontSize: 12, color: 'var(--text-hi)' }}>{active.name}</span>
              <span className="mute" style={{ fontSize: 11 }}>· Lua 5.4 · sandboxed</span>
              <div className="spacer" />
              <button className="btn sm ghost" onClick={renameScript} aria-label={`Rename Lua script ${active.name}`}>Rename</button>
              <button className="btn sm ghost" onClick={deleteScript} aria-label={`Delete Lua script ${active.name}`}>Delete</button>
              <button className="btn sm" onClick={saveScript} aria-label={`Save Lua script ${active.name}`}>Save</button>
            </div>
            <div style={{ flex: 1, overflow: 'auto', background: 'var(--bg-deep)', padding: '12px 4px 12px 0' }}>
              <textarea
                className="cmp-textarea"
                aria-label="Lua script code"
                value={code}
                onChange={e => setCode(e.target.value)}
                spellCheck="false"
                style={{ width: '100%', minHeight: '100%', border: 0, background: 'transparent', color: 'var(--text)', fontFamily: 'var(--font-mono)', fontSize: 12, lineHeight: 1.55, resize: 'none', outline: 'none', padding: '0 12px' }}
              />
            </div>
            <div style={{ borderTop: '1px solid var(--border)', padding: '8px 14px', background: 'var(--surface)', fontFamily: 'var(--font-mono)', fontSize: 11, color: 'var(--text-mid)' }}>
              <span className="mute">stdout</span>
              <div style={{ marginTop: 4, color: 'var(--text)' }}>No script output.</div>
            </div>
          </div>
        )}
      </div>
    </SurfaceShell>
  );
}

// ─── Webhooks ──────────────────────────────────────────────────────────
const INITIAL_WEBHOOKS = [];
const WEBHOOK_EVENTS = new Set(['request_captured', 'response_captured']);
function parseWebhookEvents(value) {
  return String(value || '')
    .split(',')
    .map(e => e.trim())
    .filter(e => WEBHOOK_EVENTS.has(e));
}

function WebhooksSurface() {
  const [hooks, setHooks] = React.useState(INITIAL_WEBHOOKS);
  const load = React.useCallback(() => fetchJson('/admin/webhooks', []).then(setHooks), []);
  React.useEffect(() => { load(); }, [load]);
  const addWebhook = async () => {
    const form = await formDialog('Add webhook', [
      { name: 'name', label: 'Name', value: '' },
      { name: 'url', label: 'Webhook URL', value: 'http://127.0.0.1:19191/hook' },
      { name: 'events', label: 'Events', type: 'checkboxGroup', value: ['request_captured', 'response_captured'], options: [
        { value: 'request_captured', label: 'Request captured' },
        { value: 'response_captured', label: 'Response captured' },
      ]},
      { name: 'secret', label: 'Secret', placeholder: 'optional' },
    ]);
    if (!form || !nonEmpty(form.url)) return;
    const events = Array.isArray(form.events) ? form.events : parseWebhookEvents(form.events);
    if (!events.length) return notifyError('Select at least one event');
    await sendJson('/admin/webhooks', 'POST', { id: '', name: form.name || null, url: form.url, events, enabled: true, secret: form.secret || null }).catch(e => notifyError(e.message || e));
    await load();
  };
  const toggleHook = async (h) => {
    await sendJson(`/admin/webhooks/${encodeURIComponent(h.id)}`, 'PUT', { ...h, enabled: !h.enabled }).catch(e => notifyError(e.message || e));
    await load();
  };
  const editHook = async (h) => {
    const form = await formDialog('Edit webhook', [
      { name: 'name', label: 'Name', value: h.name || '' },
      { name: 'url', label: 'Webhook URL', value: h.url },
      { name: 'events', label: 'Events', type: 'checkboxGroup', value: h.events || [], options: [
        { value: 'request_captured', label: 'Request captured' },
        { value: 'response_captured', label: 'Response captured' },
      ]},
      { name: 'secret', label: 'Secret', value: h.secret || '', placeholder: 'optional' },
    ]);
    if (!form || !nonEmpty(form.url)) return;
    const events = Array.isArray(form.events) ? form.events : parseWebhookEvents(form.events);
    if (!events.length) return notifyError('Select at least one event');
    await sendJson(`/admin/webhooks/${encodeURIComponent(h.id)}`, 'PUT', { ...h, name: form.name || null, url: form.url, events, secret: form.secret || null }).catch(e => notifyError(e.message || e));
    await load();
  };
  const deleteHook = async (h) => {
    if (!await confirmAction('Delete this webhook?', 'Delete', 'danger')) return;
    await fetch(`/admin/webhooks/${encodeURIComponent(h.id)}`, { method: 'DELETE' }).catch(e => notifyError(e.message || e));
    await load();
  };
  return (
    <SurfaceShell title="Webhooks"
                  sub="fire HTTP POSTs when matching sessions complete"
                  actions={<button className="btn primary" onClick={addWebhook}>＋ Add webhook</button>}>
      <div className="rule-head" style={{ gridTemplateColumns: '36px 1.2fr 2fr 1fr 0.8fr 100px' }}>
        <div></div><div>Name</div><div>URL</div><div>Events</div><div>Last fired</div><div></div>
      </div>
      {hooks.length === 0 && <div className="empty">No webhooks are configured.</div>}
      {hooks.map(h => (
        <div key={h.id} className={'rule-row' + (h.enabled ? '' : ' off')} style={{ gridTemplateColumns: '36px 1.2fr 2fr 1fr 0.8fr 100px' }}>
          <div className="col-toggle"><Toggle label={`Toggle webhook ${h.url}`} on={h.enabled} onChange={() => toggleHook(h)} /></div>
          <div className="col-match" style={{ fontFamily: 'var(--font-sans)', color: 'var(--text-hi)', fontSize: 12, fontWeight: 500 }}>{h.name || <span className="mute" style={{ fontSize: 11 }}>{h.id}</span>}</div>
          <div className="col-match" style={{ color: 'var(--text-mid)' }}>{h.url}</div>
          <div className="col-meta">
            {h.events.map(e => <span key={e} className="tag-badge" style={{ marginLeft: 0, marginRight: 4 }}>{e}</span>)}
          </div>
          <div className="col-meta">
            <div className="mute">runtime</div>
          </div>
          <div className="col-act">
            <button className="copy-btn" onClick={() => editHook(h)} aria-label={`Edit webhook ${h.url}`}>edit</button>
            <button className="copy-btn" onClick={() => deleteHook(h)} aria-label={`Delete webhook ${h.url}`}>×</button>
          </div>
        </div>
      ))}
    </SurfaceShell>
  );
}

// ─── DNS Override ──────────────────────────────────────────────────────
function DnsSurface() {
  const [entries, setEntries] = React.useState([]);
  const load = React.useCallback(async () => {
    const data = await fetchJson('/admin/dns', {});
    setEntries(Object.entries(data || {}).map(([host, ip]) => ({ id: host, host, ip, on: true, note: 'active override' })));
  }, []);
  React.useEffect(() => { load(); }, [load]);
  const saveDns = async (host, ip) => {
    const current = await fetchJson('/admin/dns', {});
    await sendJson('/admin/dns', 'POST', { ...current, [host]: ip });
    await load();
  };
  const addDns = async () => {
    const form = await formDialog('Add DNS override', [
      { name: 'host', label: 'Hostname', value: 'example.test' },
      { name: 'ip', label: 'Override IP', value: '127.0.0.1' },
    ]);
    if (!form || !nonEmpty(form.host) || !nonEmpty(form.ip)) return;
    await saveDns(form.host, form.ip).catch(e => notifyError(e.message || e));
  };
  const editDns = async (d) => {
    const form = await formDialog('Edit DNS override', [
      { name: 'ip', label: 'Override IP', value: d.ip },
    ]);
    if (!form || !nonEmpty(form.ip)) return;
    await saveDns(d.host, form.ip).catch(e => notifyError(e.message || e));
  };
  const deleteDns = async (d) => {
    await fetch(`/admin/dns/${encodeURIComponent(d.host)}`, { method: 'DELETE' }).catch(e => notifyError(e.message || e));
    await load();
  };
  return (
    <SurfaceShell title="DNS Override"
                  sub="resolve hostnames to fixed IPs before forwarding · CONNECT tunnels included"
                  actions={<button className="btn primary" onClick={addDns}>＋ Add override</button>}>
      <div className="rule-head" style={{ gridTemplateColumns: '36px 1fr 160px 1fr 100px' }}>
        <div></div><div>Hostname</div><div>Override IP</div><div>Note</div><div></div>
      </div>
      {entries.length === 0 && <div className="empty">No DNS overrides are configured.</div>}
      {entries.map(d => (
        <div key={d.id} className={'rule-row' + (d.on ? '' : ' off')} style={{ gridTemplateColumns: '36px 1fr 160px 1fr 100px' }}>
          <div className="col-toggle"><span className="mute">—</span></div>
          <div className="col-match">{d.host}</div>
          <div className="col-match" style={{ color: 'var(--c-3xx)' }}>{d.ip}</div>
          <div className="col-meta" style={{ fontFamily: 'var(--font-sans)' }}>{d.note}</div>
          <div className="col-act">
            <button className="copy-btn" onClick={() => editDns(d)} aria-label={`Edit DNS override ${d.host}`}>edit</button>
            <button className="copy-btn" onClick={() => deleteDns(d)} aria-label={`Delete DNS override ${d.host}`}>×</button>
          </div>
        </div>
      ))}
    </SurfaceShell>
  );
}

// ─── Capture Filter ────────────────────────────────────────────────────
function CaptureFilterSurface() {
  const [mode, setMode] = React.useState('disabled');
  const [hosts, setHosts] = React.useState([]);
  const [input, setInput] = React.useState('');
  const load = React.useCallback(async () => {
    const cfg = await fetchJson('/admin/capture-filter', { mode: 'disabled', hosts: [] });
    setMode(cfg.mode || 'disabled');
    setHosts(cfg.hosts || []);
  }, []);
  React.useEffect(() => { load(); }, [load]);
  const save = async (nextMode = mode, nextHosts = hosts) => {
    await sendJson('/admin/capture-filter', 'POST', { mode: nextMode, hosts: nextHosts });
    setMode(nextMode);
    setHosts(nextHosts);
  };
  const setRemoteMode = (nextMode) => save(nextMode, hosts).catch(e => notifyError(e.message || e));
  const addHost = () => {
    const value = input.trim();
    if (!value) return;
    save(mode, [...hosts, value]).then(() => setInput('')).catch(e => notifyError(e.message || e));
  };
  const removeHost = (i) => save(mode, hosts.filter((_, idx) => idx !== i)).catch(e => notifyError(e.message || e));

  return (
    <SurfaceShell title="Capture Filter"
                  sub="control which hosts get recorded into the session log">
      <div style={{ padding: 16 }}>
        <div className="insp-card" style={{ margin: 0, marginBottom: 16 }}>
          <div className="head">
            <Icon name="filter" size={14} stroke={1.6} />
            <h3>Recording mode</h3>
            <div className="right">
              <div className="segctl">
                <button className={mode === 'disabled' ? 'on' : ''} onClick={() => setRemoteMode('disabled')}>Record all</button>
                <button className={mode === 'allowlist' ? 'on' : ''} onClick={() => setRemoteMode('allowlist')}>Allowlist</button>
                <button className={mode === 'denylist' ? 'on' : ''} onClick={() => setRemoteMode('denylist')}>Denylist</button>
              </div>
            </div>
          </div>
          <div className="body">
            <p>
              {mode === 'disabled' && 'Every proxied request is recorded into the session log.'}
              {mode === 'allowlist' && 'Only matching hosts are recorded. Non-matching traffic is still proxied.'}
              {mode === 'denylist' && 'Matching hosts are skipped from recording. Traffic is still proxied.'}
            </p>
          </div>
        </div>

        {mode !== 'disabled' && (
          <div className="insp-card" style={{ margin: 0 }}>
            <div className="head">
              <h3>Host patterns</h3>
              <span className="meta">{hosts.length} entries · case-insensitive substring or glob</span>
              <div className="right">
                <input className="cmp-input" aria-label="Capture filter host pattern" value={input} onChange={e => setInput(e.target.value)}
                       onKeyDown={e => { if (e.key === 'Enter') addHost(); }}
                       placeholder="api.example.com or *.example.com"
                       style={{ width: 240 }} />
                <button className="btn primary sm" onClick={addHost}>Add</button>
              </div>
            </div>
            <div className="body" style={{ display: 'flex', flexWrap: 'wrap', gap: 6 }}>
              {hosts.map((h, i) => (
                <span key={i} className="pat" style={{ display: 'inline-flex', alignItems: 'center', gap: 4 }}>
                  {h}
                  <button onClick={() => removeHost(i)}
                          aria-label={`Remove capture filter host ${h}`}
                          style={{ background: 'transparent', border: 0, color: 'var(--text-low)', cursor: 'pointer', padding: 0, lineHeight: 1 }}>×</button>
                </span>
              ))}
              {hosts.length === 0 && <span className="mute" style={{ fontSize: 11 }}>(no host patterns yet)</span>}
            </div>
          </div>
        )}
      </div>
    </SurfaceShell>
  );
}

// ─── Settings ──────────────────────────────────────────────────────────
function SettingsSurface() {
  const [cfg, setCfg] = React.useState(null);
  const [upstream, setUpstream] = React.useState(null);
  const [socks5, setSocks5] = React.useState(null);
  const [errors, setErrors] = React.useState({});

  React.useEffect(() => {
    let cancelled = false;
    const load = async (label, url) => {
      try {
        const res = await fetch(url);
        if (!res.ok) throw new Error(`HTTP ${res.status}`);
        return { label, value: await res.json(), error: null };
      } catch (err) {
        return { label, value: null, error: err?.message || 'unavailable' };
      }
    };
    (async () => {
      const [config, upstreamProxy, socksStatus] = await Promise.all([
        load('config', '/admin/config'),
        load('upstream proxy', '/admin/upstream-proxy'),
        load('socks5', '/admin/socks5/status'),
      ]);
      if (cancelled) return;
      setCfg(config.value);
      setUpstream(upstreamProxy.value);
      setSocks5(socksStatus.value);
      setErrors(Object.fromEntries(
        [config, upstreamProxy, socksStatus]
          .filter(part => part.error)
          .map(part => [part.label, part.error]),
      ));
    })();
    return () => { cancelled = true; };
  }, []);

  const editUpstream = async () => {
    const current = upstream?.upstream_proxy || '';
    const next = await ask('Upstream proxy URL, empty to disable', current);
    if (next == null) return;
    await sendJson('/admin/upstream-proxy', 'POST', { upstream_proxy: next || null }).catch(e => notifyError(e.message || e));
    fetch('/admin/upstream-proxy').then(r => r.ok ? r.json() : null).then(setUpstream).catch(() => setUpstream(null));
  };

  const value = (v) => v === undefined || v === null || v === '' ? '—' : String(v);
  const exposedBind = cfg?.bind_host && !['127.0.0.1', 'localhost', '::1'].includes(cfg.bind_host);
  const clientProxy = cfg && window.location?.hostname
    ? `${window.location.hostname}:${window.location.port || (window.location.protocol === 'https:' ? '443' : '80')}`
    : (cfg ? `127.0.0.1:${cfg.port || 8080}` : '—');

  return (
    <SurfaceShell title="Settings" sub="proxy runtime configuration · env vars override these">
      <div style={{ padding: 16, display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 16 }}>
        {Object.keys(errors).length > 0 && (
          <div className="warn-strip" style={{ gridColumn: '1 / -1' }}>
            Settings API degraded. {Object.entries(errors).map(([name, err]) => `${name}: ${err}`).join('; ')}
          </div>
        )}
        {exposedBind && (
          <div className="warn-strip" style={{ gridColumn: '1 / -1' }}>
            Admin UI and proxy are reachable outside localhost because bind host is {cfg.bind_host}. Use this only on trusted networks.
          </div>
        )}
        <div className="insp-card" style={{ margin: 0 }}>
          <div className="head"><h3>Listener</h3></div>
          <div className="body">
            <div className="kv" style={{ gridTemplateColumns: '140px 1fr' }}>
              <div className="k">HTTP port</div><div className="v">{value(cfg?.port)}</div>
              <div className="k">Bind host</div><div className="v">{value(cfg?.bind_host)}</div>
              <div className="k">Client proxy</div><div className="v">{clientProxy}</div>
              <div className="k">SOCKS5</div><div className="v">{socks5 ? (socks5.enabled ? `Enabled on ${socks5.port} · ${socks5.mode || 'tunnel-only'}` : `Disabled · ${socks5.mode || 'tunnel-only'} when enabled`) : '—'}</div>
              <div className="k">Max body bytes</div><div className="v">{cfg?.max_body_bytes?.toLocaleString?.() || value(cfg?.max_body_bytes)}</div>
              <div className="k">Body retention</div><div className="v">{cfg?.max_retained_body_bytes?.toLocaleString?.() || value(cfg?.max_retained_body_bytes)}</div>
              <div className="k">Timeout</div><div className="v">{cfg?.timeout_secs ? `${cfg.timeout_secs}s` : '—'}</div>
            </div>
          </div>
        </div>

        <div className="insp-card" style={{ margin: 0 }}>
          <div className="head"><h3>MITM / TLS</h3></div>
          <div className="body">
            <div className="kv" style={{ gridTemplateColumns: '160px 1fr' }}>
              <div className="k">MITM enabled</div><div className="v">{cfg ? (cfg.mitm_enabled ? 'Enabled' : 'Disabled') : '—'}</div>
              <div className="k">Root CA</div><div className="v"><a href="/admin/ca">/admin/ca</a></div>
              <div className="k">Inspect WS frames</div><div className="v">{cfg ? (cfg.inspect_ws_frames ? 'On' : 'Off') : '—'}</div>
            </div>
          </div>
        </div>

        <div className="insp-card" style={{ margin: 0 }}>
          <div className="head"><h3>Session log</h3></div>
          <div className="body">
            <div className="kv" style={{ gridTemplateColumns: '160px 1fr' }}>
              <div className="k">Max sessions</div><div className="v">{cfg?.max_sessions?.toLocaleString?.() || value(cfg?.max_sessions)}</div>
              <div className="k">Storage path</div><div className="v" style={{ fontFamily: 'var(--font-mono)' }}>{value(cfg?.storage_path)}</div>
              <div className="k">Uptime</div><div className="v">{cfg?.uptime_secs ? `${cfg.uptime_secs}s` : '—'}</div>
              <div className="k">SSE stream</div><div className="v"><code>/api/sessions/stream</code></div>
            </div>
          </div>
        </div>

        <div className="insp-card" style={{ margin: 0 }}>
          <div className="head"><h3>Upstream proxy</h3><div className="right"><button className="btn sm" onClick={editUpstream}>Edit</button></div></div>
          <div className="body">
            <div className="kv" style={{ gridTemplateColumns: '140px 1fr' }}>
              <div className="k">Use upstream</div><div className="v">{upstream?.upstream_proxy ? 'Enabled' : 'Disabled'}</div>
              <div className="k">URL</div><div className="v">{value(upstream?.upstream_proxy)}</div>
            </div>
          </div>
        </div>

        <div className="insp-card" style={{ margin: 0, gridColumn: '1 / -1' }}>
          <div className="head"><h3>Logging</h3></div>
          <div className="body">
            <div className="kv" style={{ gridTemplateColumns: '140px 1fr' }}>
              <div className="k">Runtime source</div><div className="v">environment / process configuration</div>
              <div className="k">Editable here</div><div className="v">No</div>
            </div>
          </div>
        </div>
      </div>
    </SurfaceShell>
  );
}

// ─── Keyboard shortcuts modal ──────────────────────────────────────────
function ShortcutsModal({ onClose }) {
  const groups = [
    { title: 'Navigation', items: [
      ['↑ / ↓',         'Move between sessions'],
      ['Enter',         'Open in detail panel'],
      ['Esc',           'Close panel / clear selection'],
      ['⌘ / Ctrl + 1…9','Jump to rail surface'],
    ]},
    { title: 'Search & filter', items: [
      ['⌘ / Ctrl + F',  'Focus search'],
      ['⌘ / Ctrl + K',  'Focus search'],
      ['.*',            'Toggle regex search'],
    ]},
    { title: 'Actions', items: [
      ['Space',         'Pause / resume live refresh'],
    ]},
    { title: 'Compose', items: [
      ['⌘ / Ctrl + T',  'New request tab'],
      ['⌘ / Ctrl + Enter', 'Send request'],
      ['⌘ / Ctrl + W',  'Close tab'],
    ]},
    { title: 'View', items: [
      ['⌘ / Ctrl + D',  'Toggle theme'],
      ['?',             'Open this dialog'],
    ]},
  ];
  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={e => e.stopPropagation()}>
        <div className="modal-head">
          <h3>Keyboard shortcuts</h3>
          <button className="icon-btn" onClick={onClose} aria-label="Close keyboard shortcuts"><Icon name="x" size={14} /></button>
        </div>
        <div className="modal-body">
          {groups.map(g => (
            <div key={g.title} className="sc-group">
              <h4>{g.title}</h4>
              {g.items.map(([k, label]) => (
                <div key={k} className="sc-row">
                  <span className="sc-label">{label}</span>
                  <span className="sc-keys">
                    {k.split(' + ').map((part, i) => (
                      <React.Fragment key={i}>
                        {i > 0 && <span className="sc-plus">+</span>}
                        <kbd>{part}</kbd>
                      </React.Fragment>
                    ))}
                  </span>
                </div>
              ))}
            </div>
          ))}
        </div>
      </div>
    </div>
  );
}

// ─── Connections (HTTP/2·3 multiplexing view) ──────────────────────────
const protoBucketC = (p) => p === 'HTTP/2' ? 'h2' : p === 'HTTP/3' ? 'h3' : (p === 'HTTP/1.1' || p === 'HTTP/1.0') ? 'h1' : 'other';
const protoShortC = (p) => !p ? '—' : p === 'HTTP/2' ? 'H2' : p === 'HTTP/3' ? 'H3' : (p === 'HTTP/1.1' || p === 'HTTP/1.0') ? '1.1' : p.replace(/^HTTP\//, '');
const fmtClock = (iso) => { try { return new Date(iso).toTimeString().slice(0, 8); } catch { return '—'; } };

function ConnectionsSurface() {
  const [connections, setConnections] = React.useState([]);
  const [open, setOpen] = React.useState(() => new Set());
  const [loaded, setLoaded] = React.useState(false);
  const load = React.useCallback(async () => {
    const data = await fetchJson('/api/connections', {});
    setConnections((data && data.connections) || []);
    setLoaded(true);
  }, []);
  React.useEffect(() => {
    load();
    const t = setInterval(load, 4000); // live-ish refresh
    return () => clearInterval(t);
  }, [load]);
  const toggle = (id) => setOpen(p => { const n = new Set(p); n.has(id) ? n.delete(id) : n.add(id); return n; });

  return (
    <SurfaceShell title="Connections"
                  sub="downstream connections grouped by identity · HTTP/2 and HTTP/3 streams multiplexed on one connection"
                  actions={<button className="btn" onClick={load}>↻ Refresh</button>}>
      {loaded && connections.length === 0 && (
        <div className="empty">No connections recorded yet. Proxy some traffic to populate this view.</div>
      )}
      <div className="conn-list">
        {connections.map(c => {
          const isOpen = open.has(c.connection_id);
          const multiplexed = c.stream_count > 1 && (c.downstream_protocol === 'HTTP/2' || c.downstream_protocol === 'HTTP/3');
          return (
            <div key={c.connection_id} className="conn-card">
              <div className="conn-head" onClick={() => toggle(c.connection_id)}>
                <span className="conn-twig">{isOpen ? '▾' : '▸'}</span>
                <span className="proto-badge" data-proto={protoBucketC(c.downstream_protocol)} title={c.downstream_protocol || 'unknown'}>
                  {protoShortC(c.downstream_protocol)}
                </span>
                <span className="conn-host" title={c.hosts.join(', ')}>{c.hosts[0] || '(unknown host)'}{c.hosts.length > 1 && <span className="dim"> +{c.hosts.length - 1}</span>}</span>
                {multiplexed && <span className="conn-mux" title="Multiplexed streams">⇄ {c.stream_count} streams</span>}
                {c.max_concurrency > 1 && <span className="conn-peak" title="Peak streams in flight at once">peak {c.max_concurrency}</span>}
                <span className="conn-meta">{c.exchange_count} exchange{c.exchange_count === 1 ? '' : 's'}</span>
                <span className="conn-meta dim">{fmtClock(c.first_seen)}–{fmtClock(c.last_seen)}</span>
                <span className="conn-id dim" title={c.connection_id}>{c.connection_id.slice(0, 8)}</span>
              </div>
              {isOpen && (
                <div className="conn-streams">
                  {c.streams.map(s => {
                    const span = Math.max(1, c.span_ms || 1);
                    const left = Math.max(0, Math.min(100, (s.start_offset_ms / span) * 100));
                    const width = Math.max(1.5, Math.min(100 - left, ((s.duration_ms || 0) / span) * 100));
                    return (
                      <div key={s.id} className="conn-stream">
                        <span className="conn-stream-id">#{s.stream_id ?? '–'}</span>
                        <span className="cell-method" data-m={s.method}>{s.method}</span>
                        <span className="conn-stream-path" title={s.host + s.path}>{s.path}</span>
                        <span className="conn-track" title={`+${s.start_offset_ms} ms · ${s.duration_ms} ms`}>
                          <span className={'conn-bar' + (s.status >= 400 ? ' err' : '')} style={{ left: left + '%', width: width + '%' }} />
                        </span>
                        <span className="cell-status" data-c={s.status ? String(s.status)[0] : '-'}>{s.status || '···'}</span>
                      </div>
                    );
                  })}
                </div>
              )}
            </div>
          );
        })}
      </div>
    </SurfaceShell>
  );
}

// ─── Protocol dashboard (live aggregates) ──────────────────────────────
const fmtBytesD = (n) => {
  if (n == null) return '—';
  if (n < 1024) return n + ' B';
  if (n < 1048576) return (n / 1024).toFixed(1) + ' KB';
  if (n < 1073741824) return (n / 1048576).toFixed(1) + ' MB';
  return (n / 1073741824).toFixed(2) + ' GB';
};
function StatCard({ label, value, sub }) {
  return (
    <div className="stat-card">
      <div className="stat-label">{label}</div>
      <div className="stat-value">{value}</div>
      {sub && <div className="stat-sub">{sub}</div>}
    </div>
  );
}
function DistBars({ title, rows, tone }) {
  const total = rows.reduce((a, r) => a + r.count, 0);
  return (
    <div className="dist">
      <div className="dist-title">{title}</div>
      {rows.length === 0 && <div className="mute" style={{ fontSize: 12 }}>(none)</div>}
      {rows.map(r => (
        <div key={r.label} className="dist-row">
          <span className="dist-key" title={r.label}>{r.label}</span>
          <span className="dist-track"><span className={'dist-bar ' + (tone ? tone(r.label) : '')} style={{ width: (total ? (r.count / total) * 100 : 0) + '%' }} /></span>
          <span className="dist-count">{r.count}</span>
        </div>
      ))}
    </div>
  );
}
function ProtocolDashboard() {
  const [m, setM] = React.useState(null);
  const load = React.useCallback(async () => {
    setM(await fetchJson('/api/metrics/protocol', {}));
  }, []);
  React.useEffect(() => {
    load();
    const t = setInterval(load, 3000);
    return () => clearInterval(t);
  }, [load]);
  const statusTone = (k) => k === '2xx' ? 'ok' : k === '3xx' ? 'redir' : (k === '4xx' || k === '5xx') ? 'err' : '';
  const protoTone = (k) => k === 'HTTP/2' ? 'h2' : k === 'HTTP/3' ? 'h3' : 'h1';
  return (
    <SurfaceShell title="Protocol Dashboard"
                  sub="live aggregates across recorded sessions · refreshes every 3s"
                  actions={<button className="btn" onClick={load}>↻ Refresh</button>}>
      {!m ? <div className="empty">Loading metrics…</div> : (
        <div className="dash">
          <div className="stat-grid">
            <StatCard label="Exchanges" value={m.total_exchanges} />
            <StatCard label="Connections" value={m.connections} />
            <StatCard label="WebSockets" value={m.websockets} />
            <StatCard label="gRPC calls" value={m.grpc_calls} />
            <StatCard label="Latency p50 / p95" value={`${m.latency_p50_ms} / ${m.latency_p95_ms} ms`} sub={`max ${m.latency_max_ms} ms`} />
            <StatCard label="Traffic" value={fmtBytesD(m.total_bytes)} />
          </div>
          <div className="dash-cols">
            <DistBars title="Upstream protocol" rows={m.protocol_mix} tone={protoTone} />
            <DistBars title="Downstream protocol" rows={m.downstream_mix} tone={protoTone} />
            <DistBars title="Status classes" rows={m.status_classes} tone={statusTone} />
            {m.grpc_status.length > 0 && <DistBars title="gRPC status" rows={m.grpc_status} />}
          </div>
        </div>
      )}
    </SurfaceShell>
  );
}

window.MockSurface = MockSurface;
window.LuaSurface = LuaSurface;
window.WebhooksSurface = WebhooksSurface;
window.DnsSurface = DnsSurface;
window.CaptureFilterSurface = CaptureFilterSurface;
window.SettingsSurface = SettingsSurface;
window.ConnectionsSurface = ConnectionsSurface;
window.ProtocolDashboard = ProtocolDashboard;
window.ShortcutsModal = ShortcutsModal;
