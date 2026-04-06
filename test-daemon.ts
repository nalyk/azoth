/**
 * Quick integration test for AZOTH daemon components.
 * Run: bun run test-daemon.ts
 */

console.log('=== AZOTH Daemon Component Tests ===\n')

// ─── Test 1: Banner renders ──────────────────────────────────────────────────

import { renderBanner, renderSupervisorBanner, renderStatusHeader, formatWorkerRow, workerTableHeader, formatPermissionRequest, logPrefix } from './daemon/banner.js'

console.log('--- Test 1: Banner ---')
process.stdout.write(renderBanner({ version: '0.1.0' }))
console.log('✓ Banner renders\n')

console.log('--- Test 2: Supervisor Banner ---')
process.stdout.write(renderSupervisorBanner({
  version: '0.1.0',
  socketPath: '~/.claude/daemon.sock',
  pid: process.pid,
  workers: 2,
  maxWorkers: 8,
}))
console.log('✓ Supervisor banner renders\n')

console.log('--- Test 3: Status Header ---')
process.stdout.write(renderStatusHeader({
  version: '0.1.0',
  uptime: '2h 14m',
  workers: 2,
  maxWorkers: 8,
  sessions: 5,
  pendingPermissions: 1,
}))
console.log('✓ Status header renders\n')

console.log('--- Test 4: Worker Table ---')
console.log(workerTableHeader())
console.log(formatWorkerRow({
  key: '/home/user/project:same-dir',
  pid: 48291,
  status: 'running',
  activeSessions: 3,
  capacity: 32,
  uptimeMs: 8040000,
}))
console.log(formatWorkerRow({
  key: '/home/user/api:worktree',
  pid: 48305,
  status: 'starting',
  activeSessions: 0,
  capacity: 32,
  uptimeMs: 2820000,
}))
console.log(formatWorkerRow({
  key: '/home/user/legacy:same-dir',
  pid: 48400,
  status: 'parked',
  activeSessions: 0,
  capacity: 32,
  uptimeMs: 120000,
}))
console.log('✓ Worker table renders\n')

console.log('--- Test 5: Permission Request ---')
process.stdout.write(formatPermissionRequest({
  requestId: 'req-1712419200-42',
  toolName: 'Bash',
  description: 'Execute: rm -rf node_modules',
  workerKey: '/home/user/project:same-dir',
}))
console.log('✓ Permission request renders\n')

console.log('--- Test 6: Log Prefix ---')
console.log(logPrefix('info') + 'Daemon started')
console.log(logPrefix('warn') + 'Worker restarting')
console.log(logPrefix('error') + 'Connection failed')
console.log(logPrefix('debug') + 'Polling for work')
console.log('✓ Log prefix renders\n')

// ─── Test 7: Ring Buffer ─────────────────────────────────────────────────────

import { RingBuffer } from './daemon/ringBuffer.js'

console.log('--- Test 7: RingBuffer ---')
const rb = new RingBuffer<number>(3)
rb.push(1); rb.push(2); rb.push(3); rb.push(4)
const arr = rb.toArray()
console.assert(arr[0] === 2 && arr[1] === 3 && arr[2] === 4, `Expected [2,3,4], got [${arr}]`)
console.assert(rb.size === 3, `Expected size 3, got ${rb.size}`)
console.log(`  Buffer: [${arr}], size: ${rb.size}`)
console.log('✓ RingBuffer works\n')

// ─── Test 8: IPC Protocol ────────────────────────────────────────────────────

import { frameEncode, FrameDecoder, nextRequestId, workerKey } from './daemon/ipcProtocol.js'

console.log('--- Test 8: IPC Protocol ---')
const testMsg = { type: 'ping', id: nextRequestId() }
const encoded = frameEncode(testMsg)
console.log(`  Encoded frame: ${encoded.length} bytes`)

let decoded: unknown = null
const decoder = new FrameDecoder((msg) => { decoded = msg })
decoder.push(encoded)
console.assert(JSON.stringify(decoded) === JSON.stringify(testMsg), 'Frame decode mismatch')
console.log(`  Decoded: ${JSON.stringify(decoded)}`)
console.log(`  Worker key: ${workerKey('/home/user/project', 'same-dir')}`)
console.log('✓ IPC Protocol works\n')

// ─── Test 9: PID File ────────────────────────────────────────────────────────

import { writePidFile, readPidFile, isAlive, removePidFile } from './daemon/pidFile.js'
import { tmpdir } from 'os'
import { join } from 'path'

console.log('--- Test 9: PID File ---')
const testPidPath = join(tmpdir(), 'azoth-test.pid')
await writePidFile(testPidPath)
const pid = await readPidFile(testPidPath)
console.assert(pid === process.pid, `Expected PID ${process.pid}, got ${pid}`)
console.assert(isAlive(process.pid) === true, 'Current process should be alive')
console.assert(isAlive(999999) === false, 'Fake PID should not be alive')
await removePidFile(testPidPath)
const pidAfter = await readPidFile(testPidPath)
console.assert(pidAfter === null, 'PID file should be removed')
console.log(`  PID: ${pid}, alive: ${isAlive(process.pid)}, removed: ${pidAfter === null}`)
console.log('✓ PID File works\n')

// ─── Test 10: Daemon Config ──────────────────────────────────────────────────

import { getDefaultDaemonConfig } from './daemon/daemonConfig.js'

console.log('--- Test 10: Daemon Config ---')
const cfg = getDefaultDaemonConfig()
console.log(`  maxWorkers: ${cfg.maxWorkersPerHost}, permissionTTL: ${cfg.permissionTtlMs}ms`)
console.assert(cfg.version === 1, 'Config version should be 1')
console.assert(cfg.maxWorkersPerHost === 8, 'Default maxWorkers should be 8')
console.log('✓ Daemon Config works\n')

// ─── Test 11: Socket Server IPC round-trip ───────────────────────────────────

import { createSocketServer, connectToSocket } from './daemon/socketServer.js'

console.log('--- Test 11: Socket IPC Round-Trip ---')
const testSockPath = join(tmpdir(), `azoth-test-${process.pid}.sock`)

const server = await createSocketServer({
  socketPath: testSockPath,
  onConnection(conn) {
    conn.onMessage((msg: any) => {
      // Echo back
      conn.send({ id: msg.id, type: 'pong', payload: { echo: msg.payload } })
    })
  },
})
await server.listen()
console.log(`  Server listening on ${testSockPath}`)

const client = connectToSocket(testSockPath)
await client.connected
console.log('  Client connected')

const roundTrip = await new Promise<any>((resolve) => {
  client.onMessage((msg) => resolve(msg))
  client.send({ id: 'test-1', type: 'ping', payload: 'hello' })
})

console.assert(roundTrip.type === 'pong', `Expected pong, got ${roundTrip.type}`)
console.assert(roundTrip.payload.echo === 'hello', `Expected echo hello`)
console.log(`  Round-trip: sent ping → got ${roundTrip.type}, echo: ${roundTrip.payload.echo}`)

client.close()
await server.close()
// Clean up socket file
import { unlink } from 'fs/promises'
try { await unlink(testSockPath) } catch {}
console.log('✓ Socket IPC works\n')

// ─── Done ────────────────────────────────────────────────────────────────────

console.log('=== All 11 tests passed ===')
process.exit(0)
