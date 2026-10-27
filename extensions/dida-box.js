(() => {
  'use strict';

  const HANDLE = '__didaBoxSpotifyBridge';
  const URL = 'ws://127.0.0.1:17876/spotify';
  const MAX_BUFFER = 256 * 1024;
  const LYRICS_TIMEOUT = 10000;
  const EVENTS = ['songchange', 'onplaypause', 'onprogress'];
  const generation = { value: 0 };

  if (window[HANDLE]?.dispose) window[HANDLE].dispose();

  let socket;
  let reconnectTimer;
  let heartbeat;
  let waitTimer;
  let disposed = false;
  let currentId = null;
  let currentLyrics = null;
  let currentArt = null;
  let artworkRequest;

  const player = () => Spicetify.Player;
  const rawState = () => player()?.data || null;
  const track = () => rawState()?.track || rawState()?.item || null;
  const metadata = () => track()?.metadata || {};
  const finiteNumber = (...values) => values.find(value => Number.isFinite(Number(value))) ?? 0;

  const state = () => {
    const data = rawState();
    const item = track();
    const meta = metadata();
    if (!item?.uri) return { type: 'state', version: 1, track: null, position_ms: 0, playing: false };
    const duration = Math.max(0, Math.floor(finiteNumber(
      meta.duration,
      data?.duration,
      item.duration?.milliseconds,
      item.duration_ms,
    )));
    const position = Math.max(0, Math.floor(finiteNumber(
      player().getProgress?.(),
      data?.position_as_of_timestamp,
      data?.progress,
    )));
    const playing = typeof data?.is_playing === 'boolean'
      ? data.is_playing
      : !!player().isPlaying?.();
    return {
      type: 'state',
      version: 1,
      track: {
        id: String(item.uri),
        title: String(meta.title || item.name || item.title || ''),
        artist: String(meta.artist_name || item.artists?.[0]?.name || item.artist?.name || ''),
        duration_ms: duration,
      },
      position_ms: Math.min(position, duration || position),
      playing,
    };
  };

  const send = value => {
    if (socket?.readyState === WebSocket.OPEN && socket.bufferedAmount <= MAX_BUFFER) {
      socket.send(JSON.stringify(value));
    }
  };

  const sendState = () => send(state());

  async function lyrics(id, mine) {
    let timeout;
    try {
      const trackId = encodeURIComponent(id.split(':').pop());
      const endpoint = `https://spclient.wg.spotify.com/color-lyrics/v2/track/${trackId}?format=json&vocalRemoval=false&market=from_token`;
      const request = Spicetify.CosmosAsync.get(endpoint);
      const response = await Promise.race([
        request,
        new Promise((_, reject) => { timeout = setTimeout(() => reject(new Error('timeout')), LYRICS_TIMEOUT); }),
      ]);
      clearTimeout(timeout);
      if (disposed || mine !== generation.value) return;
      const raw = response?.lyrics?.lines;
      if (!Array.isArray(raw) || raw.length === 0) {
        currentLyrics = { type: 'lyrics', version: 1, track_id: id, status: 'unavailable', lines: [] };
        send(currentLyrics);
        return;
      }
      const parsed = raw.slice(0, 4096).map(line => {
        const text = String(line.words || '').slice(0, 4096);
        const start = Number(line.startTimeMs);
        return { start_ms: Number.isFinite(start) ? Math.max(0, Math.floor(start)) : undefined, text };
      });
      const synced = response?.lyrics?.syncType === 'LINE_SYNCED'
        && parsed.every(line => line.start_ms !== undefined);
      const lines = synced ? parsed : parsed.map(line => ({ text: line.text }));
      currentLyrics = { type: 'lyrics', version: 1, track_id: id, status: synced ? 'synced' : 'unsynced', lines };
      send(currentLyrics);
    } catch (_) {
      if (!disposed && mine === generation.value) {
        currentLyrics = { type: 'lyrics', version: 1, track_id: id, status: 'error', lines: [] };
        send(currentLyrics);
      }
    } finally {
      clearTimeout(timeout);
    }
  }

  async function artwork(id, mine) {
    const item = track();
    const meta = metadata();
    const source = meta.image_large_url || meta.image_xlarge_url || meta.image_url
      || item?.album?.images?.[0]?.url || item?.images?.[0]?.url;
    if (!source) return;
    const controller = new AbortController();
    artworkRequest = controller;
    const timeout = setTimeout(() => controller.abort(), LYRICS_TIMEOUT);
    let bitmap;
    try {
      const url = String(source).replace(/^spotify:image:/, 'https://i.scdn.co/image/');
      const response = await fetch(url, { signal: controller.signal, credentials: 'omit' });
      if (!response.ok) return;
      bitmap = await createImageBitmap(await response.blob());
      const scale = Math.min(1, 320 / Math.max(bitmap.width, bitmap.height));
      const canvas = document.createElement('canvas');
      canvas.width = Math.max(1, Math.round(bitmap.width * scale));
      canvas.height = Math.max(1, Math.round(bitmap.height * scale));
      canvas.getContext('2d').drawImage(bitmap, 0, 0, canvas.width, canvas.height);
      const data = canvas.toDataURL('image/jpeg', 0.88);
      if (disposed || mine !== generation.value || data.length > 175000) return;
      currentArt = { type: 'album_art', version: 1, track_id: id, data };
      send(currentArt);
    } catch (_) {
      // Artwork is optional; playback and lyrics remain available offline.
    } finally {
      bitmap?.close();
      clearTimeout(timeout);
      if (artworkRequest === controller) artworkRequest = undefined;
    }
  }

  const onTrack = () => {
    const next = state();
    const id = next.track?.id || null;
    if (id !== currentId) {
      currentId = id;
      currentLyrics = null;
      currentArt = null;
      artworkRequest?.abort();
      generation.value++;
      if (id) {
        lyrics(id, generation.value);
        artwork(id, generation.value);
      }
    }
    sendState();
  };

  const onEvent = () => sendState();

  const onMessage = event => {
    let message;
    try { message = JSON.parse(event.data); } catch (_) { return; }
    if (message?.type === 'ping') { onTrack(); return; }
    if (message?.type !== 'command' || message.track_id !== currentId) return;
    const current = state();
    if (!current.track) return;
    try {
      const milliseconds = Number(message.milliseconds);
      const duration = current.track.duration_ms;
      switch (message.command) {
        case 'toggle_active': player().togglePlay?.(); break;
        case 'next': player().next?.(); break;
        case 'seek':
          if (Number.isFinite(milliseconds)) player().seek(Math.max(0, Math.min(duration, milliseconds)));
          break;
        case 'seek_by':
          if (Number.isFinite(milliseconds)) player().seek(Math.max(0, Math.min(duration, current.position_ms + milliseconds)));
          break;
        default: return;
      }
      sendState();
    } catch (_) { console.error('dida-box command failed'); }
  };

  const connect = () => {
    if (disposed) return;
    try {
      socket = new WebSocket(URL);
      socket.onopen = () => {
        onTrack();
        if (currentLyrics) send(currentLyrics);
        if (currentArt) send(currentArt);
      };
      socket.onmessage = onMessage;
      socket.onclose = () => { if (!disposed) reconnectTimer = setTimeout(connect, 2000); };
      socket.onerror = () => {};
    } catch (_) { reconnectTimer = setTimeout(connect, 2000); }
  };

  const dispose = () => {
    disposed = true;
    clearInterval(waitTimer);
    clearInterval(heartbeat);
    clearTimeout(reconnectTimer);
    generation.value++;
    artworkRequest?.abort();
    EVENTS.forEach(event => player()?.removeEventListener?.(event, event === 'songchange' ? onTrack : onEvent));
    socket?.close();
    delete window[HANDLE];
  };

  window[HANDLE] = { dispose };
  waitTimer = setInterval(() => {
    if (disposed) return;
    if (window.Spicetify?.Player && window.Spicetify?.CosmosAsync) {
      clearInterval(waitTimer);
      player().addEventListener('songchange', onTrack);
      player().addEventListener('onplaypause', onEvent);
      player().addEventListener('onprogress', onEvent);
      currentId = state().track?.id || null;
      if (currentId) {
        lyrics(currentId, ++generation.value);
        artwork(currentId, generation.value);
      }
      heartbeat = setInterval(sendState, 1000);
      connect();
    }
  }, 100);
})();
