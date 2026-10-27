const assert = require('node:assert/strict');
const fs = require('node:fs');
const vm = require('node:vm');

async function main() {
  const listeners = new Map();
  const intervals = new Map();
  const timeouts = new Map();
  const requests = [];
  const artRequests = [];
  const sockets = [];
  const seeks = [];
  let timerId = 0;
  let toggles = 0;
  class Socket {
    static OPEN = 1;
    constructor() { this.readyState = 1; this.bufferedAmount = 0; this.sent = []; sockets.push(this); }
    send(raw) { this.sent.push(JSON.parse(raw)); }
    close() { this.readyState = 3; this.onclose?.(); }
  }
  const player = {
    data: { track: { uri: 'spotify:track:A', metadata: { title: 'Current song', artist_name: 'Artist', duration: '90000', image_large_url: 'spotify:image:cover' } }, is_playing: true },
    getProgress: () => 12000,
    addEventListener: (name, fn) => listeners.set(name, fn),
    removeEventListener: (name, fn) => { if (listeners.get(name) === fn) listeners.delete(name); },
    seek: value => seeks.push(value),
    togglePlay: () => toggles++,
  };
  const api = { Player: player, CosmosAsync: { get: url => new Promise(resolve => requests.push({ url, resolve })) } };
  const context = {
    window: { Spicetify: api }, Spicetify: api, WebSocket: Socket, console,
    AbortController,
    fetch: (url, options) => new Promise(resolve => artRequests.push({ url, options, resolve })),
    createImageBitmap: async blob => ({ width: 640, height: 640, data: blob, close() {} }),
    document: { createElement: () => {
      let data;
      return { getContext: () => ({ drawImage: bitmap => { data = bitmap.data; } }), toDataURL: () => data };
    } },
    setInterval: fn => { const id = ++timerId; intervals.set(id, fn); return id; },
    clearInterval: id => intervals.delete(id),
    setTimeout: fn => { const id = ++timerId; timeouts.set(id, fn); return id; },
    clearTimeout: id => timeouts.delete(id),
  };
  vm.runInNewContext(fs.readFileSync(__dirname + '/dida-box.js', 'utf8'), context);
  [...intervals.values()][0]();
  const socket = sockets[0];
  socket.onopen();
  assert.deepEqual(socket.sent[0], {
    type: 'state', version: 1, track: { id: 'spotify:track:A', title: 'Current song', artist: 'Artist', duration_ms: 90000 },
    position_ms: 12000, playing: true,
  });
  player.data.is_playing = false;
  listeners.get('onplaypause')();
  assert.equal(socket.sent.at(-1).playing, false);
  const command = message => socket.onmessage({ data: JSON.stringify({ type: 'command', track_id: 'spotify:track:A', ...message }) });
  command({ command: 'seek_by', milliseconds: -20000 });
  command({ command: 'seek', milliseconds: 100000 });
  command({ command: 'toggle_active', track_id: 'spotify:track:B' });
  assert.deepEqual(seeks, [0, 90000]);
  assert.equal(toggles, 0);
  player.data.track.uri = 'spotify:track:B';
  listeners.get('songchange')();
  player.data.track.uri = 'spotify:track:A';
  listeners.get('songchange')();
  assert.equal(requests.length, 3);
  const settle = () => new Promise(resolve => setImmediate(resolve));
  requests[2].resolve({ lyrics: { syncType: 'LINE_SYNCED', lines: [{ startTimeMs: '4000', words: 'new A' }] } });
  await settle();
  requests[1].resolve({ lyrics: { syncType: 'LINE_SYNCED', lines: [{ startTimeMs: '2000', words: 'B' }] } });
  requests[0].resolve({ lyrics: { syncType: 'LINE_SYNCED', lines: [{ startTimeMs: '1000', words: 'old A' }] } });
  await settle();
  artRequests[2].resolve({ ok: true, blob: async () => 'data:image/jpeg;base64,bmV3QQ==' });
  await settle();
  artRequests[1].resolve({ ok: true, blob: async () => 'data:image/jpeg;base64,Qg==' });
  artRequests[0].resolve({ ok: true, blob: async () => 'data:image/jpeg;base64,b2xkQQ==' });
  await settle();
  assert.deepEqual(socket.sent.filter(message => message.type === 'album_art'), [
    { type: 'album_art', version: 1, track_id: 'spotify:track:A', data: 'data:image/jpeg;base64,bmV3QQ==' },
  ]);
  assert.equal(artRequests[0].options.signal.aborted, true);
  assert.equal(artRequests[0].options.credentials, 'omit');
  assert.deepEqual(socket.sent.filter(message => message.type === 'lyrics'), [
    { type: 'lyrics', version: 1, track_id: 'spotify:track:A', status: 'synced', lines: [{ start_ms: 4000, text: 'new A' }] },
  ]);
  player.data.track.uri = 'spotify:track:C';
  listeners.get('songchange')();
  requests[3].resolve({ lyrics: { syncType: 'UNSYNCED', lines: [{ startTimeMs: '0', words: 'plain text' }] } });
  await settle();
  assert.equal(socket.sent.at(-1).status, 'unsynced');
  assert.deepEqual(socket.sent.at(-1).lines, [{ text: 'plain text' }]);
  player.data.track.uri = 'spotify:track:D';
  listeners.get('songchange')();
  const beforeDispose = socket.sent.length;
  artRequests[3].resolve({ ok: true, blob: async () => 'data:image/jpeg;base64,Qw==' });
  artRequests[4].resolve({ ok: true, blob: async () => 'data:image/jpeg;base64,RA==' });
  context.window.__didaBoxSpotifyBridge.dispose();
  requests[4].resolve({ lyrics: { lines: [] } });
  await settle();
  assert.equal(socket.sent.length, beforeDispose);
  assert.equal(listeners.size, 0);
  assert.equal(intervals.size, 0);
  assert.equal(timeouts.size, 0);
  console.log('Spotify bridge: metadata, pause, seek bounds, lyrics/artwork A/B/A races, unsynced lyrics and disposal passed');
}

main().catch(error => { console.error(error); process.exitCode = 1; });
