import test from 'node:test';
import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { visibleProfiles, reconcileSelection, runProfileBatch } from '../desktop/src/profile-batch.js';

const items = [{id:'a',name:'Alpha',model:'GLM'}, {id:'b',name:'Beta',model:'deepseek'}, {id:'c',name:'Gamma',notes:'work'}];
const intent = operation => ({schema_version:1, operation, intent_id:'a'.repeat(32), disposition:'committed', config_state:'committed', validation:'not_run', science_running:false});
const applied = operation => ({schema_version:1, operation, operation_id:'b'.repeat(32), disposition:'completed', config_state:'after', runtime_state:'stopped', recovery_state:'not_needed'});

test('search is case-insensitive and supports names, models and notes', () => {
  assert.deepEqual(visibleProfiles(items,' glm ').map(p=>p.id),['a']);
  assert.deepEqual(visibleProfiles(items,'WORK').map(p=>p.id),['c']);
  assert.deepEqual(visibleProfiles(items,'missing'),[]);
});
test('selection reconciliation removes deleted rows without changing retained selections', () => {
  const selected = new Set(['a','c','stale']);
  assert.deepEqual([...reconcileSelection(selected,items)],['a','c']);
  assert.deepEqual([...selected],['a','c','stale']);
});
for (const action of ['delete','clearkey']) {
  test(`${action}: serial batch accepts applied and regular completed outcomes`, async () => {
    const calls=[]; let inFlight=0;
    const result=await runProfileBatch(items,action,async(command,{id})=>{
      assert.equal(inFlight++,0); calls.push(id); await Promise.resolve(); inFlight--;
      return id==='a' ? applied(action==='delete'?'delete_applied_profile':'clear_applied_profile_key') : intent(command);
    });
    assert.deepEqual(calls,['a','b','c']);
    assert.deepEqual(result.completed,calls); assert.equal(result.failed,null);
  });
}
test('failure stops immediately and records completed, uncertain and unattempted rows', async () => {
  const calls=[];
  const result=await runProfileBatch(items,'delete',async(command,{id})=>{
    calls.push(id); if(id==='b') throw new Error('uncertain'); return intent(command);
  });
  assert.deepEqual(calls,['a','b']); assert.deepEqual(result.completed,['a']);
  assert.equal(result.failed.id,'b'); assert.equal(result.remaining,1);
});
test('unknown/mismatched/attention responses fail closed without retrying', async () => {
  for (const response of [{status:'ok'},intent('clear_profile_key'),{...applied('delete_applied_profile'),disposition:'attention'}, {...intent('delete_profile'),validation:'accepted'}]) {
    let calls=0; const result=await runProfileBatch(items,'delete',async()=>{calls++; return response;});
    assert.equal(calls,1); assert.equal(result.completed.length,0); assert.equal(result.remaining,2);
  }
});
test('invalid action never invokes backend', async () => {
  await assert.rejects(runProfileBatch(items,'other',()=>assert.fail('must not invoke')));
});
test('API-only UI has a semantic table, accessible bulk confirmation and no account controls', () => {
  const html=readFileSync(new URL('../desktop/src/index.html',import.meta.url),'utf8');
  const controller=readFileSync(new URL('../desktop/src/profile-controller.js',import.meta.url),'utf8');
  assert.match(html,/<table class="profile-table"/);
  assert.doesNotMatch(html,/data-page="status"|data-page-target="status"|id="currentProfileName"/);
  assert.match(html,/aria-labelledby="runtimeHeading"/);
  assert.doesNotMatch(html,/id="selectAllProfiles"/);
  assert.match(html,/id="batchManageBtn" aria-pressed="false"/);
  assert.match(html,/id="batchDeletePhrase"/);
  assert.match(controller,/if \(!batchMode \|\| isBusy\(\) \|\| isActivationInFlight\(\)\) return/);
  assert.match(controller,/value\.trim\(\) !== `删除 \$\{pendingBatch\.items\.length\} 项`\) return/);
  assert.match(html,/<dialog id="batchDialog"/);
  assert.doesNotMatch(html,/id="codexLoginBtn"|id="codexAuthStartBtn"|data-page="skills"/);
  assert.match(controller,/if \(!pendingBatch \|\| isBusy\(\) \|\| isActivationInFlight\(\)\) return/);
  assert.match(controller,/document\.createElement\("li"\); li\.textContent = p\.name/);
  assert.match(controller,/aria-pressed="\$\{active\}"/);
  assert.match(controller,/title="编辑连接"/);
  assert.match(controller,/const credential = hasKey \? "••••••••" : "未填写"/);
});


test("API admission cannot prevent explicit local recovery and protects generic mutations", () => {
  const oneClick = readFileSync(new URL("../desktop/src-tauri/src/commands/runtime/one_click.rs", import.meta.url), "utf8");
  const commands = readFileSync(new URL("../desktop/src-tauri/src/commands/profiles.rs", import.meta.url), "utf8");
  const runtime = readFileSync(new URL("../desktop/src/runtime-controller.js", import.meta.url), "utf8");
  const loop = oneClick.slice(oneClick.indexOf("let (entry, prepared) = loop"));
  assert.ok(loop.indexOf("replay_history_before_auth_if_required") < loop.indexOf("require_api_profile"));
  assert.ok(loop.indexOf("require_api_profile") < loop.indexOf("prepare_provider_auth"));
  assert.equal((commands.match(/require_api_mutation_target/g) || []).length, 3);
  const boundary = runtime.slice(runtime.indexOf("async function checkOneClickBoundary"), runtime.indexOf("async function runOneClick"));
  assert.doesNotMatch(boundary, /!getConfigState\(\).active_id/);
});
