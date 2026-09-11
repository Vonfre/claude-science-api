import { parseConfigMutationResponse } from './runtime-mutation-protocol.js';

export function visibleProfiles(profiles, query = '') {
  const needle = query.trim().toLowerCase();
  return profiles.filter(p => [p.name, p.model, p.template_id, p.notes]
    .some(value => String(value || '').toLowerCase().includes(needle)));
}

export function reconcileSelection(selection, profiles) {
  const valid = new Set(profiles.map(p => p.id));
  return new Set([...selection].filter(id => valid.has(id)));
}

// Existing mutations remain the owners. A batch is serial, not an atomic transaction;
// an uncertain response stops further writes and must never be retried automatically.
export async function runProfileBatch(items, action, invoke) {
  if (!['delete', 'clearkey'].includes(action)) throw new Error('未知批量操作');
  const command = action === 'delete' ? 'delete_profile' : 'clear_profile_key';
  const applied = action === 'delete' ? 'delete_applied_profile' : 'clear_applied_profile_key';
  const completed = [];
  for (const item of items) {
    try {
      const outcome = parseConfigMutationResponse(await invoke(command, { id: item.id }));
      const intent = outcome && outcome.operation === command
        && ['committed', 'no_change'].includes(outcome.disposition)
        && outcome.config_state === 'committed' && typeof outcome.intent_id === 'string'
        && outcome.validation === 'not_run' && outcome.science_running === false;
      const mutation = outcome && outcome.operation === applied
        && outcome.disposition === 'completed' && outcome.config_state === 'after'
        && outcome.runtime_state === 'stopped' && outcome.recovery_state === 'not_needed'
        && typeof outcome.operation_id === 'string';
      if (!intent && !mutation) throw new Error('返回结果未确认完成，请刷新状态后检查，勿直接重试。');
      completed.push(item.id);
    } catch (error) {
      return { completed, failed: item, error, remaining: items.length - completed.length - 1 };
    }
  }
  return { completed, failed: null, remaining: 0 };
}
