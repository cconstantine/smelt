// SME-452: a bound on opening every connection node makes during a
// language server's install: the DNS lookup, the TCP connect, an http://
// or https:// proxy's CONNECT reply and the TLS handshake alike, none of which npm bounds
// (see CONNECT_BOUND in src/lsp/pods/script.rs). A connect that hasn't finished
// in time is destroyed with ECONNECTIONTIMEOUT, which npm retries on a
// fresh connection and logs. pod_script writes this file with the bound
// filled in, and preloads it into the install alone.
'use strict'
const net = require('net')
const tls = require('tls')
const ms = __CONNECT_BOUND_MS__
// A socket's pending bound, so a tunnel handed to tls.connect stops its own.
const pending = new WeakMap()

function target (args) {
  try {
    const options = args.find(a => a !== null && typeof a === 'object') || {}
    if (options.path) return options.path
    const host = typeof args[1] === 'string' ? args[1] : options.host || options.servername || 'localhost'
    const port = typeof args[0] === 'number' || typeof args[0] === 'string' ? args[0] : options.port
    return port === undefined ? host : `${host}:${port}`
  } catch (_) {
    return '?'
  }
}

// Destroys `socket` with ECONNECTIONTIMEOUT, its message `what()`, unless
// `settled` is called, or `still()` says it's no longer opening when the
// bound runs out.
function bound (socket, what, still) {
  let timer = null
  const settled = () => {
    if (timer !== null) clearTimeout(timer)
    timer = null
    pending.delete(socket)
  }
  timer = setTimeout(() => {
    timer = null
    pending.delete(socket)
    if (socket.destroyed || !still()) return
    const e = new Error(`${what()} didn't finish within ${ms} ms`)
    e.code = 'ECONNECTIONTIMEOUT'
    socket.destroy(e)
  }, ms)
  timer.unref()
  pending.set(socket, settled)
  socket.once('close', settled)
  return settled
}

// Bounds opening `socket`: until its `ready` event (TCP's `connect`, TLS's
// `secureConnect`). A socket whose first write is a proxy's `CONNECT` is
// a tunnel (an http:// proxy's TCP socket, or an https:// proxy's TLS
// one): it's opening until it's handed to tls.connect({socket}) or
// closes, so a proxy that never answers, or answers in part and stalls,
// is bounded too. That holds whether the CONNECT was written before
// `ready` or after.
function opening (socket, what, ready, connecting) {
  let tunnel = false
  const label = () => tunnel ? `${what} (a proxy's CONNECT)` : what
  socket.write = function (chunk, ...rest) {
    delete socket.write
    const head = Buffer.isBuffer(chunk) ? chunk.subarray(0, 8).toString('latin1') : String(chunk).slice(0, 8)
    if (head === 'CONNECT ') {
      tunnel = true
      if (!pending.has(socket) && !socket.destroyed) bound(socket, label, () => true)
    }
    return socket.write(chunk, ...rest)
  }
  const settled = bound(socket, label, () => tunnel || connecting())
  socket.once(ready, () => { if (!tunnel) settled() })
}

// A TCP connect: an http registry, or an http:// proxy's socket.
const netConnect = net.connect
function connect (...args) {
  const socket = netConnect.apply(this, args)
  opening(socket, `Connect to ${target(args)}`, 'connect', () => socket.connecting)
  return socket
}
net.connect = connect
net.createConnection = connect

// A TLS connect, whole: from the call to `secureConnect`. Over an existing
// socket (a proxy's tunnel), that socket's own bound ends here: the
// tunnel is open, and this connect's bound takes over.
const tlsConnect = tls.connect
tls.connect = function (...args) {
  const options = args.find(a => a !== null && typeof a === 'object')
  const inner = options && options.socket && pending.get(options.socket)
  if (inner) inner()
  const socket = tlsConnect.apply(this, args)
  const what = `TLS connect to ${target(args)}`
  if (options && options.socket) {
    const settled = bound(socket, () => what, () => true)
    socket.once('secureConnect', settled)
  } else {
    opening(socket, what, 'secureConnect', () => true)
  }
  return socket
}
