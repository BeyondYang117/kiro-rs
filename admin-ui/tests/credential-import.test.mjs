import test from 'node:test'
import assert from 'node:assert/strict'
import { parseCredentialImport, runCredentialImport } from '../src/lib/credential-import.ts'

const balance = { currentUsage: 10, usageLimit: 100 }
const dependencies = (overrides = {}) => ({
  getCredentials: async () => ({ credentials: [] }),
  addCredential: async () => ({ credentialId: 7, balance }),
  getBalance: async () => balance,
  hash: async value => `hash:${value}`,
  describeError: error => ({ title: error.response?.data?.error?.message || error.message }),
  ...overrides,
})
const entries = input => parseCredentialImport(JSON.stringify(input))

async function run(input, deps, previous = [], verify = true, cancelled = () => false) {
  const results = new Map()
  await runCredentialImport(input, previous, verify, dependencies(deps), result => results.set(result.index, result), cancelled)
  return [...results.values()]
}

test('accepts exports, aliases, BOM and preserves account metadata', () => {
  const [entry] = parseCredentialImport('\uFEFF' + JSON.stringify({ accounts: [{
    email: 'account@example.test', disabled: true, credentials: {
      refresh_token: ' token ', client_id: 'client', client_secret: 'secret', auth_method: 'IAM',
      profile_arn: 'arn:test', api_region: 'eu-west-1', region: 'us-east-1',
      machine_id: 'machine', subscription_title: 'KIRO STUDENT', proxy_url: 'direct',
    },
  }] }))
  assert.equal(entry.error, undefined)
  assert.deepEqual(entry.request, {
    refreshToken: 'token', clientId: 'client', clientSecret: 'secret', authMethod: 'idc',
    priority: 0, disabled: true, email: 'account@example.test', profileArn: 'arn:test',
    apiRegion: 'eu-west-1', authRegion: 'us-east-1', machineId: 'machine',
    subscriptionTitle: 'KIRO STUDENT', proxyUrl: 'direct', proxyUsername: undefined,
    proxyPassword: undefined, endpoint: undefined,
  })
  assert.equal(entries({ credentials: [{ kiroApiKey: 'key' }] })[0].request.authMethod, 'api_key')
})

test('invalid rows report local errors while valid rows continue', async () => {
  const input = entries([null, 12, { refreshToken: 5 }, { refreshToken: 'token', priority: -1 },
    { refreshToken: 'token', authMethod: 'idc' }, { kiroApiKey: 'key' }])
  assert.equal(input.filter(row => row.error).length, 5)
  let calls = 0
  const results = await run(input, { addCredential: async () => { calls++; return { credentialId: 9, balance } } })
  assert.equal(calls, 1)
  assert.deepEqual(results.map(row => row.status), ['failed', 'failed', 'failed', 'failed', 'failed', 'verified'])
})

test('balance 400 retains saved ID, deduplicates warnings, retries without creating again', async () => {
  let adds = 0
  const input = entries([{ refreshToken: 'token' }, { refreshToken: 'token' }])
  const results = await run(input, {
    addCredential: async () => { adds++; return { credentialId: 42, balanceError: '获取使用额度失败: 400 Bad Request' } },
    getBalance: async () => { throw new Error('unexpected duplicate balance request') },
  })
  assert.equal(adds, 1)
  assert.deepEqual(results.map(row => row.status), ['warning', 'duplicate'])
  assert.equal(results[0].credentialId, 42)
  assert.match(results[0].error, /400/)
  const retried = await run([input[0]], {
    addCredential: async () => { throw new Error('must not recreate saved account') },
    getBalance: async (id, force) => { assert.equal(id, 42); assert.equal(force, true); return balance },
  }, results)
  assert.equal(retried[0].status, 'verified')
})

test('uses returned balance without an additional network call; skips requested verification', async () => {
  const input = entries({ kiroApiKey: 'key' })
  const [result] = await run(input, { getBalance: async () => { throw new Error('extra request') } })
  assert.equal(result.status, 'verified')
  assert.equal(result.usage, '10/100')
  const [skipped] = await run(input, {
    addCredential: async request => { assert.equal(request.checkBalance, false); return { credentialId: 1 } },
    getBalance: async () => { throw new Error('verification was skipped') },
  }, [], false)
  assert.equal(skipped.status, 'imported')
})

test('older servers balance failures preserve accounts and show a warning', async () => {
  const [result] = await run(entries({ kiroApiKey: 'key' }), {
    addCredential: async () => ({ credentialId: 5 }),
    getBalance: async () => { throw new Error('network unavailable') },
  })
  assert.equal(result.status, 'warning')
  assert.equal(result.credentialId, 5)
})

test('checks existing hashes for both auth types before creating', async () => {
  const results = await run(entries([{ refreshToken: 'token' }, { kiroApiKey: 'key' }]), {
    getCredentials: async () => ({ credentials: [{ refreshTokenHash: 'hash:token' }, { apiKeyHash: 'hash:key' }] }),
    addCredential: async () => { throw new Error('must not add duplicates') },
  })
  assert.deepEqual(results.map(row => row.status), ['duplicate', 'duplicate'])
})

test('stop finishes in-flight account and leaves remaining rows for retry', async () => {
  let cancelled = false
  const results = await run(entries([{ kiroApiKey: 'one' }, { kiroApiKey: 'two' }]), {
    addCredential: async () => { cancelled = true; return { credentialId: 1, balance } },
  }, [], true, () => cancelled)
  assert.deepEqual(results.map(row => row.status), ['verified', 'cancelled'])
})

test('fresh list failure prevents all writes', async () => {
  let called = false
  await assert.rejects(run(entries({ kiroApiKey: 'key' }), {
    getCredentials: async () => { throw new Error('offline') },
    addCredential: async () => { called = true },
  }), /offline/)
  assert.equal(called, false)
})
