// Executes the real, read-only CRS Console relay source. Only account, Redis,
// queue and logging dependencies are replaced; Axios and error sanitization
// remain real. Every request goes to the local test daemon.
const fs = require('node:fs')
const http = require('node:http')
const path = require('node:path')
const crypto = require('node:crypto')
const { createRequire } = require('node:module')

const [crsRoot, upstream, apiKey] = process.argv.slice(2)
if (!/^http:\/\/127\.0\.0\.1:\d+$/.test(upstream)) {
  throw new Error('fixture upstream must be a loopback address')
}
const account = { apiUrl: upstream, apiKey, maxConcurrentTasks: 0, disableAutoProtection: true }
const accountService = {
  getAccount: async () => account,
  _createProxyAgent: () => null,
  checkQuotaUsage: async () => null,
  isAccountRateLimited: async () => false,
  isAccountOverloaded: async () => false
}
const logger = new Proxy({}, { get: () => () => {} })
const dependencies = {
  uuid: { v4: crypto.randomUUID },
  '../account/claudeConsoleAccountService': accountService,
  '../../models/redis': { getClientSafe: () => ({ exists: async () => false }) },
  '../../utils/logger': logger,
  '../../../config/config': { requestTimeout: 5000 },
  '../../utils/upstreamErrorHelper': {},
  '../userMessageQueueService': { isUserMessageRequest: () => false }
}
function loadSource(relative) {
  const filename = path.resolve(crsRoot, relative)
  const realRequire = createRequire(filename)
  const module = { exports: {} }
  const requireDependency = (name) => {
    if (Object.hasOwn(dependencies, name)) return dependencies[name]
    if (name === '../../utils/errorSanitizer') return loadSource('src/utils/errorSanitizer.js')
    if (name === './logger') return logger
    return realRequire(name)
  }
  const evaluate = new Function('require', 'module', 'exports', '__filename', '__dirname',
    fs.readFileSync(filename, 'utf8'))
  evaluate(requireDependency, module, module.exports, filename, path.dirname(filename))
  return module.exports
}
const relay = loadSource('src/services/relay/claudeConsoleRelayService.js')
const requests = []
const server = http.createServer(async (req, res) => {
  try {
    if (req.url === '/__requests') {
      res.setHeader('content-type', 'application/json')
      res.end(JSON.stringify(requests))
      return
    }
    const chunks = []
    for await (const chunk of req) chunks.push(chunk)
    const body = JSON.parse(Buffer.concat(chunks).toString())
    if (req.url.includes('count_tokens')) {
      res.setHeader('content-type', 'application/json')
      res.end('{"input_tokens":10}')
      return
    }
    requests.push({ stream: body.stream === true })
    if (body.stream === true) {
      await relay._makeClaudeConsoleStreamRequest(body, account, null, req.headers, res,
        'local-test-account', () => {})
    } else {
      const response = await relay.relayRequest(body, { id: 'local-test-key' }, null, res,
        req.headers, 'local-test-account')
      const headers = Object.fromEntries(Object.entries(response.headers).filter(([name]) =>
        !['content-encoding', 'transfer-encoding', 'content-length'].includes(name.toLowerCase())))
      res.writeHead(response.statusCode, headers)
      res.end(response.body)
    }
  } catch (error) {
    process.stderr.write(`${error.stack}\n`)
    if (!res.headersSent) res.writeHead(500, { 'content-type': 'application/json' })
    if (!res.writableEnded) res.end('{"error":{"message":"local fixture failed"}}')
  }
})
server.listen(0, '127.0.0.1', () => {
  process.stdout.write(`http://127.0.0.1:${server.address().port}\n`)
})
