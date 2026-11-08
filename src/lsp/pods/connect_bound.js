// SME-452: a bound on every TLS connect node makes during a language
// server's install: the DNS lookup, the TCP connect and the handshake
// alike, none of which npm bounds (see CONNECT_BOUND in src/lsp/pods.rs).
// A connect that hasn't finished in time is destroyed with
// ECONNECTIONTIMEOUT, which npm retries on a fresh connection and logs.
// pod_script writes this file with the bound filled in, and preloads it
// into the install alone.
'use strict'
const tls = require('tls')
const ms = __CONNECT_BOUND_MS__
const connect = tls.connect
function target (args) {
  try {
    const options = args.find(a => a !== null && typeof a === 'object') || {}
    const host = typeof args[1] === 'string' ? args[1] : options.host || options.servername || 'localhost'
    const port = typeof args[0] === 'number' || typeof args[0] === 'string' ? args[0] : options.port
    return port === undefined ? host : `${host}:${port}`
  } catch (_) {
    return '?'
  }
}
tls.connect = function (...args) {
  const socket = connect.apply(this, args)
  const timer = setTimeout(() => {
    const e = new Error(`TLS connect to ${target(args)} didn't finish within ${ms} ms`)
    e.code = 'ECONNECTIONTIMEOUT'
    socket.destroy(e)
  }, ms)
  timer.unref()
  const done = () => clearTimeout(timer)
  socket.once('secureConnect', done)
  socket.once('close', done)
  return socket
}
