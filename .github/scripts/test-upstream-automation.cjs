const assert = require('node:assert/strict');
const { readFileSync, mkdtempSync, mkdirSync, writeFileSync, cpSync, rmSync } = require('node:fs');
const { join } = require('node:path');
const { tmpdir } = require('node:os');
const { execFileSync } = require('node:child_process');
const { test } = require('node:test');
const vm = require('node:vm');

const workflow = name => readFileSync(join(__dirname, '..', 'workflows', name), 'utf8');
const mergeSource = workflow('auto-merge-upstream-port.yml');
const loopSource = workflow('copilot-review-loop.yml');
const fixSource = workflow('copilot-triage-review-fix.md');
const head = 'a'.repeat(40);
const backlog = { filename: '.github/upstream-backlog.toml', status: 'modified' };
const labels = names => names.map(name => ({ name }));
const pr = (kind = 'upstream-triage') => ({
  number: 177, node_id: 'PR_177', state: 'open', draft: false, body: '',
  user: { login: 'github-actions[bot]' }, base: { ref: 'main' },
  head: { sha: head, repo: { full_name: 'anweiss/ableton-link-rs' } },
  labels: labels(['automation', kind, 'copilot-reviewed']),
});
const quiet = { info() {}, notice() {}, warning() {}, error() {}, setFailed(message) {
  throw new Error(message);
} };

// Exercise functions extracted from the checked-in workflows, not copies of
// their policy. The indentation is part of github-script's YAML block.
function loadFunctions(source, names, extra = {}) {
  const code = names.map(name => {
    const match = source.match(new RegExp(
      `^            (?:async )?function ${name}\\([^\\n]*\\) \\{\\n[\\s\\S]*?^            \\}`,
      'm',
    ));
    assert.ok(match, `missing workflow function ${name}`);
    return match[0];
  }).join('\n');
  const context = vm.createContext({
    owner: 'anweiss', repo: 'ableton-link-rs',
    labelsOf: value => value.labels.map(label => label.name),
    THREAT_LABEL: 'agentic-threat-detected', DONE_LABEL: 'copilot-reviewed',
    core: quiet, ...extra,
  });
  vm.runInContext(code, context);
  return context;
}

for (const [name, source] of [['merge', mergeSource], ['review', loopSource]]) {
  test(`${name}: accepts either upstream kind, rejects ambiguous and unrelated PRs`, () => {
    const { disqualify } = loadFunctions(source, ['disqualify']);
    for (const kind of ['upstream-sync', 'upstream-triage']) {
      assert.equal(disqualify(pr(kind), false), null);
    }
    const rejected = [
      { labels: labels(['automation']) },
      { labels: labels(['upstream-triage']) },
      { labels: labels(['automation', 'upstream-triage', 'upstream-sync']) },
      { labels: labels(['automation', 'upstream-triage', 'autorelease: pending']) },
      { user: { login: 'anweiss' } },
      { state: 'closed' },
      { base: { ref: 'release' } },
      { head: { repo: { full_name: 'someone/fork' } } },
      { head: { repo: null } },
    ];
    for (const change of rejected) assert.ok(disqualify({ ...pr(), ...change }, false));
  });

  test(`${name}: triage scope rejects extra, added, removed, renamed, and empty diffs`, async () => {
    let files = [backlog];
    const calls = [];
    const { triageScopeViolation } = loadFunctions(source, ['triageScopeViolation'], {
      github: {
        rest: { pulls: { listFiles: 'files' } },
        paginate: async (...args) => { calls.push(args); return files; },
      },
    });
    assert.equal(await triageScopeViolation(pr()), false);
    for (files of [
      [], [backlog, { filename: 'src/lib.rs', status: 'modified' }],
      [{ ...backlog, status: 'added' }], [{ ...backlog, status: 'removed' }],
      [{ ...backlog, status: 'renamed', previous_filename: 'README.md' }],
      [{ ...backlog, previous_filename: 'README.md' }],
      [{ filename: 'README.md', status: 'modified' }],
    ]) assert.equal(await triageScopeViolation(pr()), true);
    const count = calls.length;
    assert.equal(await triageScopeViolation(pr('upstream-sync')), false);
    assert.equal(calls.length, count, 'ports must retain their broader scope');
    assert.equal(calls[0][1].per_page, 100);
  });
}

function mergeScript() {
  const marker = '          script: |\n';
  assert.equal(mergeSource.split(marker).length, 2);
  return mergeSource.split(marker)[1].replace(/^ {12}/gm, '');
}

async function runMerge(options = {}) {
  const current = { ...pr(options.kind), ...options.pr };
  const events = [];
  const files = options.files || [backlog];
  const comments = [{
    user: { login: 'github-actions[bot]' },
    body: '<!-- copilot-review-loop:state -->\n```json\n' +
      JSON.stringify({ reviewedSha: options.reviewedSha || head }) + '\n```',
  }];
  const pulls = {
    get: async () => ({ data: current }),
    listFiles: 'files',
    list: 'pulls',
    merge: async args => { events.push(['merge', args]); },
  };
  const issues = {
    listComments: 'comments',
    removeLabel: async () => { events.push(['revoke']); },
    createComment: async args => { events.push(['comment', args.body]); },
  };
  const github = {
    rest: { pulls, issues },
    paginate: async method => {
      if (method === 'files') {
        if (options.fileError) throw new Error('diff unavailable');
        return files;
      }
      if (method === 'comments') return comments;
      throw new Error(`Unexpected paginate ${method}`);
    },
    graphql: async query => {
      if (query.includes('disablePullRequestAutoMerge')) {
        events.push(['disable']);
        return {};
      }
      if (query.includes('enablePullRequestAutoMerge')) {
        events.push(['enable']);
        if (options.direct) throw new Error('Pull request is in clean status');
        return {};
      }
      if (query.includes('headRefOid')) {
        return { repository: { pullRequest: {
          labels: { nodes: options.finalLabels || current.labels },
          headRefOid: options.finalHead || current.head.sha,
          autoMergeRequest: null,
        } } };
      }
      if (query.includes('autoMergeRequest')) {
        return { repository: { pullRequest: {
          autoMergeRequest: { enabledAt: '2026-09-01' },
        } } };
      }
      throw new Error('Unexpected GraphQL call');
    },
  };
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  const execute = new AsyncFunction('github', 'context', 'core', 'setTimeout', mergeScript());
  let error;
  try {
    await execute(github, {
      repo: { owner: 'anweiss', repo: 'ableton-link-rs' },
      eventName: 'pull_request', payload: { pull_request: { number: current.number } },
    }, quiet, callback => callback());
  } catch (failure) { error = failure; }
  return { events, error };
}

test('merge: reviewed triage and ports can queue or SHA-bound squash merge', async () => {
  for (const kind of ['upstream-triage', 'upstream-sync']) {
    for (const direct of [false, true]) {
      const result = await runMerge({ kind, direct });
      assert.ifError(result.error);
      assert.ok(result.events.some(([event]) => event === 'enable'));
      assert.ok(!result.events.some(([event]) => event === 'revoke'));
      if (direct) assert.equal(result.events.find(([event]) => event === 'merge')[1].sha, head);
    }
  }
});

test('merge: every triage hold disarms before it can queue or merge', async () => {
  const scenarios = [
    { files: [backlog, { filename: 'src/lib.rs', status: 'modified' }] },
    { files: [{ ...backlog, status: 'renamed' }] },
    { fileError: true },
    { reviewedSha: 'b'.repeat(40) },
    { finalHead: 'b'.repeat(40) },
    { finalLabels: labels(['automation', 'upstream-sync', 'copilot-reviewed']) },
    { finalLabels: labels(['automation', 'upstream-triage']) },
    { finalLabels: labels(['upstream-triage', 'copilot-reviewed']) },
    { finalLabels: labels(['automation', 'upstream-triage', 'copilot-reviewed', 'agentic-threat-detected']) },
    { pr: { labels: labels(['automation', 'upstream-triage', 'agentic-threat-detected']) }, fileError: true },
    { pr: { labels: labels(['automation', 'upstream-triage']) } },
    { pr: { labels: labels(['upstream-triage', 'copilot-reviewed']) } },
    { pr: { labels: labels(['automation', 'upstream-triage', 'upstream-sync', 'copilot-reviewed']) } },
    { pr: { body: 'risk: wire-format' } },
  ];
  for (const scenario of scenarios) {
    const result = await runMerge(scenario);
    const actions = result.events.map(([event]) => event);
    assert.ok(actions.includes('disable'), JSON.stringify(scenario));
    const unreviewed = scenario.pr?.labels?.every(label => label.name !== 'copilot-reviewed') &&
      !scenario.pr.labels.some(label => label.name === 'agentic-threat-detected');
    assert.equal(actions.includes('revoke'), !unreviewed, JSON.stringify(scenario));
    assert.ok(!actions.includes('enable') && !actions.includes('merge'), JSON.stringify(scenario));
    if (scenario.fileError && !scenario.pr) assert.match(result.error.message, /diff unavailable/);
    else assert.ifError(result.error);
  }
});

test('merge: unrelated release auto-merge remains untouched', async () => {
  const result = await runMerge({
    pr: { labels: labels(['autorelease: pending', 'automation']) },
    fileError: true,
  });
  assert.ifError(result.error);
  assert.deepEqual(result.events, []);
});

test('review: hold checks revoke on unreadable triage diffs and prioritize threat', async () => {
  const events = [];
  const context = loadFunctions(loopSource,
    ['disqualify', 'triageScopeViolation', 'holdReason'], {
      github: {
        rest: { pulls: { listFiles: 'files' } },
        paginate: async () => { throw new Error('diff unavailable'); },
      },
      revoke: async () => events.push('revoke'),
      isWireFormat: async () => false,
    });
  await assert.rejects(context.holdReason(pr()), /diff unavailable/);
  assert.deepEqual(events, ['revoke']);
  assert.equal(await context.holdReason({
    ...pr(), labels: labels(['agentic-threat-detected']),
  }), 'threat');
  assert.equal(await context.holdReason({ ...pr(), labels: [] }), 'eligibility');
});

test('review: routes dispatch and outcome lookup to the same exact workflow and head', async () => {
  for (const kind of ['upstream-triage', 'upstream-sync']) {
    const requests = [];
    const runs = [];
    let expected;
    const context = loadFunctions(loopSource,
      ['fixWorkflow', 'dispatchFixWorkflow', 'fixRunOutcome'], {
        setTimeout: callback => callback(),
        userRequest: async (method, path, body) => {
          requests.push({ method, path, body });
          return method === 'POST' ? null : { workflow_runs: runs };
        },
        github: { rest: { actions: { listWorkflowRuns: async args => {
          requests.push(args);
          return { data: { workflow_runs: runs } };
        } } } },
      });
    expected = context.fixWorkflow(pr(kind));
    assert.equal(expected.file, kind === 'upstream-triage'
      ? 'copilot-triage-review-fix.lock.yml' : 'copilot-review-fix.lock.yml');
    runs.push(
      { id: 1, display_title: `${expected.name} - PR #178 @ ${head}`, created_at: new Date().toISOString() },
      { id: 2, display_title: `${expected.name} - PR #177 @ ${head}`, created_at: new Date().toISOString() },
    );
    assert.equal(await context.dispatchFixWorkflow(177, head, expected), 2);
    assert.ok(requests[0].path.endsWith(`/${expected.file}/dispatches`));
    assert.equal(requests[0].body.inputs.head_sha, head);
    assert.equal(JSON.parse(requests[0].body.inputs.aw_context).item_number, 177);
    assert.equal((await context.fixRunOutcome(177, head, expected)).id, 2);
    assert.equal(requests.at(-1).workflow_id, expected.file);
  }
});

async function runReconcile(options = {}) {
  const events = [];
  const initial = { ...pr(), ...options.pr };
  const handle = { value: { rounds: 0, ...options.state } };
  let reads = 0;
  const context = loadFunctions(loopSource,
    ['disqualify', 'triageScopeViolation', 'holdReason', 'holdNote', 'fixWorkflow', 'reconcile'], {
      github: {
        rest: {
          pulls: { get: async () => ({
            data: ++reads === 1 ? initial : { ...initial, ...options.fresh },
          }), listFiles: 'files' },
          issues: { listComments: 'comments' },
        },
        paginate: async method => method === 'files' ? (options.files || [backlog]) : [],
      },
      hasPat: true, MAX_ROUNDS: 2, STALL_HOURS: 6,
      readState: async () => handle,
      writeState: async (_number, current, patch) => {
        Object.assign(current.value, patch);
        events.push(['state', patch]);
        return current.value;
      },
      revoke: async () => events.push(['revoke']),
      unblockRuns: async () => { events.push(['unblock']); return null; },
      reviewsForHead: async () => options.reviews || [],
      commentsForReviews: async () => options.comments || [],
      reviewerPending: async () => false,
      requestCopilotReview: async () => events.push(['review']),
      isWireFormat: async value => /risk:\s*wire-format/i.test(value.body),
      stalled: () => false,
      setLabel: async (_value, enabled) => { events.push(['label', enabled]); return true; },
      dispatchFixWorkflow: async (...args) => events.push(['dispatch', ...args]),
      userRequest: async () => {},
      announceHandoff: async () => events.push(['handoff']),
    });
  await context.reconcile(177);
  return { events, state: handle.value };
}

const cleanReview = {
  id: 42, state: 'APPROVED', submitted_at: '2026-09-01T00:00:00Z',
  body: '<!-- ccr-overview-v2 -->\n### \u{1f7e2} Approved',
};

test('review: triage requires review, requests it, and only signs off a clean current head', async () => {
  const pending = await runReconcile();
  assert.ok(pending.events.some(([event]) => event === 'review'));
  assert.ok(!pending.events.some(([event]) => event === 'label'));
  const done = await runReconcile({ reviews: [cleanReview] });
  assert.equal(done.state.reviewedSha, head);
  assert.ok(done.events.some(([event, enabled]) => event === 'label' && enabled));
  const restarted = await runReconcile({ state: { reviewedSha: 'b'.repeat(40) } });
  assert.equal(restarted.state.reviewedSha, null);
  assert.ok(restarted.events.some(([event]) => event === 'revoke'));
  assert.ok(restarted.events.some(([event]) => event === 'review'));
});

test('review: threat and triage scope violations stop before review or fix dispatch', async () => {
  for (const scenario of [
    { files: [backlog, { filename: 'src/lib.rs', status: 'modified' }] },
    { pr: { labels: labels(['automation', 'upstream-triage', 'agentic-threat-detected']) } },
  ]) {
    const result = await runReconcile(scenario);
    assert.equal(result.state.phase, 'human-hold');
    assert.ok(result.events.some(([event]) => event === 'revoke'));
    assert.ok(!result.events.some(([event]) => ['review', 'unblock', 'dispatch', 'label'].includes(event)));
  }
});

test('review: metadata and SHA changes prevent both sign-off and fix dispatch', async () => {
  for (const fresh of [
    { head: { ...pr().head, sha: 'b'.repeat(40) } },
    { labels: labels(['automation', 'upstream-sync', 'copilot-reviewed']) },
    { labels: labels(['automation', 'upstream-triage', 'agentic-threat-detected']) },
    { labels: labels(['upstream-triage', 'copilot-reviewed']) },
    { body: 'risk: wire-format' },
  ]) {
    for (const comments of [[], [{ html_url: 'https://github.com/example/comment' }]]) {
      const result = await runReconcile({ fresh, reviews: [cleanReview], comments });
      assert.ok(result.events.some(([event]) => event === 'revoke'));
      assert.ok(!result.events.some(([event]) => ['dispatch', 'label'].includes(event)));
    }
  }
});

test('review: triage findings reach the constrained fixer and still obey round limits', async () => {
  const options = { reviews: [cleanReview], comments: [{ html_url: 'https://github.com/example/comment' }] };
  const result = await runReconcile(options);
  const dispatch = result.events.find(([event]) => event === 'dispatch');
  assert.equal(dispatch[1], 177);
  assert.equal(dispatch[2], head);
  assert.equal(dispatch[3].file, 'copilot-triage-review-fix.lock.yml');
  assert.equal(result.state.rounds, 1);
  assert.ok(!result.events.some(([event]) => event === 'label'));
  const exhausted = await runReconcile({ ...options, state: { rounds: 2 } });
  assert.equal(exhausted.state.phase, 'rounds-exhausted');
  assert.ok(!exhausted.events.some(([event]) => event === 'dispatch'));
});

test('review: PR closed during reconciliation is not relabelled or marked human-held', async () => {
  for (const comments of [[], [{ html_url: 'https://github.com/example/comment' }]]) {
    const result = await runReconcile({
      reviews: [cleanReview], comments, fresh: { state: 'closed' },
    });
    assert.ok(!result.events.some(([event]) => ['dispatch', 'label', 'revoke', 'state'].includes(event)));
  }
});

test('review: body-only change requests and non-green overview verdicts remain held', async () => {
  for (const review of [
    { ...cleanReview, state: 'CHANGES_REQUESTED' },
    { ...cleanReview, state: 'COMMENTED', body: '<!-- ccr-overview-v2 -->\n### Needs a closer look\nSuppressed comments (1)' },
  ]) {
    const result = await runReconcile({ reviews: [review] });
    assert.equal(result.state.phase, 'human-hold');
    assert.ok(!result.events.some(([event]) => ['dispatch', 'label'].includes(event)));
  }
});

test('triage fix: deterministic preflight enforces scope, provenance, labels, and SHA', async () => {
  const script = fixSource.split('      script: |\n')[1]
    .split('  # gh-aw restores')[0].replace(/^ {8}/gm, '');
  const AsyncFunction = Object.getPrototypeOf(async function () {}).constructor;
  const execute = new AsyncFunction('github', 'context', 'process', script);
  for (const change of [
    {}, { head: { sha: 'b'.repeat(40), repo: pr().head.repo } },
    { user: { login: 'anweiss' } }, { state: 'closed' },
    { base: { ref: 'other' } }, { labels: labels(['automation']) },
    { labels: labels(['automation', 'upstream-triage', 'upstream-sync']) },
    { labels: labels(['automation', 'upstream-triage', 'agentic-threat-detected']) },
    { head: { sha: head, repo: { full_name: 'someone/fork' } } },
    { body: 'risk: wire-format' },
  ]) {
    const run = () => execute({
      rest: { pulls: { get: async () => ({ data: { ...pr(), ...change } }), listFiles: 'files' } },
      paginate: async () => [backlog],
    }, { repo: { owner: 'anweiss', repo: 'ableton-link-rs' } },
    { env: { PR_NUMBER: '177', REVIEWED_SHA: head,
      AW_CONTEXT: JSON.stringify({ item_type: 'pull_request', item_number: 177 }) } });
    if (Object.keys(change).length) await assert.rejects(run);
    else await run();
  }
});

test('triage fix: reviewed backlog survives trusted instruction restoration', () => {
  const root = mkdtempSync(join(tmpdir(), 'triage-restore-test-'));
  try {
    const repository = join(root, 'repo');
    const snapshot = join(root, 'base', '.github');
    mkdirSync(join(repository, '.github'), { recursive: true });
    mkdirSync(snapshot, { recursive: true });
    writeFileSync(join(repository, '.github', 'upstream-backlog.toml'), 'reviewed backlog\n');
    writeFileSync(join(repository, '.github', 'copilot-instructions.md'), 'untrusted branch instructions\n');
    const git = (...args) => execFileSync('git', args, { cwd: repository, encoding: 'utf8' }).trim();
    git('init', '--quiet');
    git('add', '.github');
    git('-c', 'user.name=Test', '-c', 'user.email=test@example.invalid',
      '-c', 'commit.gpgsign=false', 'commit', '--quiet', '-m', 'test fixture');
    writeFileSync(join(snapshot, 'upstream-backlog.toml'), 'stale main backlog\n');
    writeFileSync(join(snapshot, 'copilot-instructions.md'), 'trusted main instructions\n');
    const step = fixSource.split('  - name: Preserve reviewed backlog through instruction restore\n')[1]
      .split('  - name: Verify backlog validator runtime')[0].split('    run: |\n')[1]
      .replace(/^ {6}/gm, '').replaceAll('/tmp/gh-aw/base', join(root, 'base'));
    execFileSync('bash', ['-e', '-c', step], {
      cwd: repository, env: { ...process.env, REVIEWED_SHA: git('rev-parse', 'HEAD') },
    });
    // The pinned gh-aw restore step replaces .github with this snapshot.
    cpSync(snapshot, join(repository, '.github'), { recursive: true, force: true });
    assert.equal(readFileSync(join(repository, '.github', 'upstream-backlog.toml'), 'utf8'), 'reviewed backlog\n');
    assert.equal(readFileSync(join(repository, '.github', 'copilot-instructions.md'), 'utf8'), 'trusted main instructions\n');
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
});

test('triage fix: safe-output allowlist and workflow wakeups are wired', () => {
  const push = fixSource.split('  push-to-pull-request-branch:\n')[1]
    .split('  update-pull-request:')[0];
  assert.match(push, /required-labels: \[upstream-triage, automation\]/);
  assert.match(push, /allowed-files:\n      - "\.github\/upstream-backlog\.toml"\n    protected-files:/);
  assert.match(loopSource, /workflows: \[.*"Link Upstream Watch".*"Copilot Triage Review Fix"/);
  assert.match(mergeSource, /types: \[.*unlabeled/);
  assert.match(loopSource, /dispatchFixWorkflow\(number, head, workflow\)/);
  assert.match(loopSource, /fixRunOutcome\(number, head, workflow\)/);
  const port = workflow('copilot-review-fix.md');
  assert.match(port, /required-labels: \[upstream-sync, automation\]/);
});

test('triage fix: compiled preflight and safe-output policy match the tested source', () => {
  const lock = workflow('copilot-triage-review-fix.lock.yml');
  const sourceScript = fixSource.split('      script: |\n')[1]
    .split('  # gh-aw restores')[0].replace(/^ {8}/gm, '');
  const compiledScript = lock.split('        name: Verify triage target\n')[1]
    .match(/^          script: (".*")$/m)[1];
  assert.equal(JSON.parse(compiledScript), sourceScript);
  const encoded = lock.match(/^          GH_AW_SAFE_OUTPUTS_HANDLER_CONFIG: (".*")$/m)[1];
  const config = JSON.parse(JSON.parse(encoded));
  assert.deepEqual(config.push_to_pull_request_branch.allowed_files, ['.github/upstream-backlog.toml']);
  assert.deepEqual(config.push_to_pull_request_branch.required_labels, ['upstream-triage', 'automation']);
  assert.deepEqual(config.update_pull_request.required_labels, ['upstream-triage', 'automation']);
  assert.ok(lock.indexOf('name: Verify triage target') <
    lock.indexOf('name: Preserve reviewed backlog through instruction restore'));
  assert.ok(lock.indexOf('name: Preserve reviewed backlog through instruction restore') <
    lock.indexOf('name: Restore agent config folders from base branch'));
});
