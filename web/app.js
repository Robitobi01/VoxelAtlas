/* Dependency-free viewer. All map coordinates below are native-resolution pixels. */
(() => {
  'use strict';
  const $ = id => document.getElementById(id);
  const canvas = $('map'), ctx = canvas.getContext('2d');
  const state = { manifest: null, map: null, x: 0, y: 0, scale: 1, width: 1, height: 1 };
  const cache = new Map(), pointers = new Map(), indexes = new Map();
  const MAX_IMAGE_BYTES = 64 * 1024 * 1024, MAX_IMAGES = 512, MAX_PENDING = 8;
  let imageBytes = 0;
  let protectedImages = new Set(), protectedIndexes = new Set();
  const MAX_INDEXES = 128, MAX_INDEX_PENDING = 4;
  let indexPending = 0, wantedIndexes = [];
  let pending = 0, wanted = [], frame = 0, urlTimer, manifestBusy = false;
  let initialState = true, currentGeneration = '', pointerPosition = null;
  let coordinates = { x: 0, z: 0, region: '' }, copyTimer;
  const clamp = (v, min, max) => Math.max(min, Math.min(max, v));
  const finite = v => typeof v === 'number' && Number.isFinite(v);
  const record = v => v && typeof v === 'object' && !Array.isArray(v);
  const validHash = v => typeof v === 'string' && /^[a-zA-Z0-9_-]{1,256}$/.test(v);
  const requestDraw = () => { if (!frame) frame = requestAnimationFrame(draw); };

  function message(title, detail, retry = false) {
    $('message').hidden = false;
    $('message-title').textContent = title;
    $('message-detail').textContent = detail;
    $('retry').hidden = !retry;
  }

  function validMap(m, version) {
    return m && typeof m.id === 'string' && typeof m.world === 'string' &&
      ['topdown', 'isometric'].includes(m.view) && Number.isInteger(m.rotation) && m.rotation >= 0 && m.rotation <= 3 &&
      ['daylight', 'plain'].includes(m.mode) && Array.isArray(m.bounds) && m.bounds.length === 4 && m.bounds.every(finite) &&
      m.bounds.every(n => Math.abs(n) <= 2 ** 45) && m.bounds[2] >= m.bounds[0] && m.bounds[3] >= m.bounds[1] && Number.isInteger(m.min_zoom) &&
      m.min_zoom >= -30 && m.min_zoom <= 0 && m.max_zoom === 0 &&
      (version === 2 ? record(m.tile_indexes) : record(m.tiles));
  }

  async function refreshManifest() {
    if (manifestBusy) return;
    manifestBusy = true;
    try {
      const response = await fetch('current.json', { cache: 'no-cache' });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const manifest = await response.json();
      if (![1, 2].includes(manifest.version) || ![256, 512].includes(manifest.tile_size) ||
          typeof manifest.generation !== 'string' || !Array.isArray(manifest.maps)) throw new Error('Unsupported map metadata');
      manifest.maps = manifest.maps.filter(m => validMap(m, manifest.version));
      const previous = state.map;
      state.manifest = manifest;
      if (currentGeneration !== manifest.generation) {
        // Content-addressed images remain reusable across generations.
        for (const [key, image] of cache) if (image.status === 'error') cache.delete(key);
        for (const [key, index] of indexes) if (index.status === 'error') indexes.delete(key);
        currentGeneration = manifest.generation;
      }
      populateMaps();
      if (!manifest.maps.length) {
        state.map = null;
        message('No maps yet', 'Your map will appear here after the first render completes.');
      } else {
        const params = new URLSearchParams(location.search);
        const selected = initialState
          ? manifest.maps.find(m => m.id === params.get('map') && String(m.rotation) === (params.get('rotation') || '0'))
          : manifest.maps.find(m => m.id === previous?.id && m.rotation === previous.rotation);
        selectMap(selected || manifest.maps[0], !previous || !selected);
        if (initialState && selected && ['x', 'y', 'scale'].every(k => params.has(k) && finite(Number(params.get(k))))) {
          const x = Number(params.get('x')), y = Number(params.get('y')), scale = Number(params.get('scale'));
          // The previous viewer auto-saved this unusable camera on worlds with
          // far-away chunks. Recover those exact defaults, not other shared views.
          const oldScale = 2 ** (selected.min_zoom + Math.min(selected.default_zoom, -selected.min_zoom));
          const legacyDefault = selected.min_zoom <= -16 && Number.isInteger(selected.default_zoom) &&
            Math.abs(x - (selected.bounds[0] + selected.bounds[2]) / 2) < .01 &&
            Math.abs(y - (selected.bounds[1] + selected.bounds[3]) / 2) < .01 &&
            Math.abs(scale - oldScale) < oldScale * .000001;
          if (!legacyDefault) {
            state.x = clamp(x, -(2 ** 45), 2 ** 45);
            state.y = clamp(y, -(2 ** 45), 2 ** 45);
            state.scale = clamp(scale, minScale(), 8);
          } else changed();
          requestDraw();
        }
        initialState = false;
        $('message').hidden = true;
      }
      requestDraw();
    } catch (error) {
      if (!state.map) message('Waiting for a world', 'Could not load current.json. Check that a render has been published, then try again.', true);
    } finally { manifestBusy = false; }
  }

  function populateMaps() {
    const select = $('map-select');
    select.replaceChildren();
    const seen = new Set();
    for (const [index, map] of state.manifest.maps.entries()) {
      if (seen.has(map.id)) continue;
      seen.add(map.id);
      const option = document.createElement('option');
      option.value = String(index);
      option.textContent = map.id;
      select.append(option);
    }
    select.disabled = !state.manifest.maps.length;
  }

  function selectMap(map, fit = true) {
    const previous = state.map;
    // Pixel coordinates are projection-dependent. Rotations share a stable Y=0
    // world-plane center; changing world or projection instead starts fitted.
    const sameProjection = previous && previous.world === map.world && previous.view === map.view;
    const center = sameProjection ? unproject(state.x, state.y, previous) : null;
    state.map = map;
    $('map-select').value = String(state.manifest.maps.findIndex(m => m.id === map.id));
    const rotations = state.manifest.maps.filter(m => m.id === map.id);
    const controls = $('rotation-controls');
    controls.replaceChildren();
    for (const candidate of rotations) {
      const names = ['tl', 'tr', 'br', 'bl'];
      const label = ['Top left', 'Top right', 'Bottom right', 'Bottom left'][candidate.rotation];
      const button = document.createElement('button');
      button.type = 'button'; button.title = label;
      button.setAttribute('aria-label', label);
      button.setAttribute('aria-pressed', String(candidate.rotation === map.rotation));
      const image = document.createElement('img');
      image.src = `icons/${names[candidate.rotation]}${map.view === 'isometric' ? '-iso' : ''}.png`;
      image.alt = '';
      image.onerror = () => { button.textContent = label; };
      button.append(image);
      button.addEventListener('click', () => { selectMap(candidate); changed(); });
      controls.append(button);
    }
    if (fit && !sameProjection) {
      if (Number.isInteger(map.default_zoom) && map.default_zoom >= 0) {
        // Match the configured origin view. Global bounds can include distant
        // border/outpost chunks; neither the center nor default detail should
        // depend on their distance. Zoom 9 is native resolution; 7 is quarter.
        state.x = 0; state.y = 0;
        state.scale = clamp(2 ** (Math.min(map.default_zoom, 12) - 9), minScale(), 8);
      } else fitMap();
    } else if (center) {
      [state.x, state.y] = project(center[0], 0, center[1]);
      state.scale = clamp(state.scale, minScale(), 8);
    }
    requestDraw();
  }

  function minScale() {
    // Keep the lowest-resolution viewport below 256 tiles, even on large monitors.
    return state.map ? Math.max(.0003, 2 ** state.map.min_zoom * Math.max(.25,
      state.width / (state.manifest.tile_size * 14), state.height / (state.manifest.tile_size * 14))) : .0003;
  }
  function fitMap() {
    if (!state.map) return;
    const [x1, y1, x2, y2] = state.map.bounds;
    state.x = (x1 + x2) / 2; state.y = (y1 + y2) / 2;
    const size = state.manifest.tile_size;
    state.scale = clamp(Math.min(state.width * .8 / Math.max(x2 - x1, size), state.height * .65 / Math.max(y2 - y1, size)), minScale(), 2);
    changed();
  }
  function changed() {
    state.x = clamp(state.x, -(2 ** 45), 2 ** 45); state.y = clamp(state.y, -(2 ** 45), 2 ** 45);
    requestDraw(); clearTimeout(urlTimer); urlTimer = setTimeout(saveURL, 180);
  }
  function saveURL() {
    if (!state.map) return;
    const url = new URL(location.href);
    url.searchParams.set('map', state.map.id); url.searchParams.set('rotation', state.map.rotation);
    url.searchParams.set('x', state.x.toFixed(2)); url.searchParams.set('y', state.y.toFixed(2));
    url.searchParams.set('scale', state.scale.toPrecision(7));
    history.replaceState(null, '', url);
  }
  function screenToWorld(x, y) { return [state.x + (x - state.width / 2) / state.scale, state.y + (y - state.height / 2) / state.scale]; }
  function worldToScreen(x, y) { return [(x - state.x) * state.scale + state.width / 2, (y - state.y) * state.scale + state.height / 2]; }
  function zoom(factor, x = state.width / 2, y = state.height / 2) {
    const [wx, wy] = screenToWorld(x, y);
    state.scale = clamp(state.scale * factor, minScale(), 8);
    state.x = wx - (x - state.width / 2) / state.scale;
    state.y = wy - (y - state.height / 2) / state.scale;
    changed();
  }

  function touchCache(hash) {
    const item = cache.get(hash);
    if (item) { cache.delete(hash); cache.set(hash, item); }
    return item;
  }
  function evict() {
    for (const [hash, item] of cache) {
      if (imageBytes <= MAX_IMAGE_BYTES && cache.size <= MAX_IMAGES) break;
      if (item.status !== 'loading' && !protectedImages.has(hash)) {
        imageBytes -= item.bytes || 0;
        cache.delete(hash);
      }
    }
  }
  function cancelObsoleteImages(required) {
    for (const [hash, item] of cache) {
      if (item.status !== 'loading' || required.has(hash)) continue;
      item.status = 'cancelled';
      item.image.onload = item.image.onerror = null;
      item.image.removeAttribute('src');
      cache.delete(hash); pending--;
    }
  }
  function pumpImages() {
    const required = new Set(wanted);
    cancelObsoleteImages(required);
    for (const hash of required) {
      if (pending >= MAX_PENDING) break;
      if (cache.has(hash)) continue;
      const image = new Image();
      const item = { image, status: 'loading' };
      cache.set(hash, item); pending++;
      image.decoding = 'async';
      const finish = status => {
        if (item.status !== 'loading') return;
        item.status = status; pending--;
        item.bytes = status === 'ready' ? (image.naturalWidth || state.manifest.tile_size) * (image.naturalHeight || state.manifest.tile_size) * 4 : 0;
        imageBytes += item.bytes;
        evict(); requestDraw();
      };
      // Do not make the first canvas draw synchronously decode a large WebP.
      image.onload = () => {
        if (image.decode) image.decode().then(() => finish('ready'), () => finish('error'));
        else finish('ready');
      };
      image.onerror = () => finish('error');
      image.src = `tiles/${encodeURIComponent(hash)}.webp`;
      evict();
    }
  }

  function touchIndex(hash) {
    const item = indexes.get(hash);
    if (item) { indexes.delete(hash); indexes.set(hash, item); }
    return item;
  }
  function evictIndexes() {
    for (const [hash, item] of indexes) {
      if (indexes.size <= MAX_INDEXES) break;
      if (item.status !== 'loading' && !protectedIndexes.has(hash)) indexes.delete(hash);
    }
  }
  function pumpIndexes() {
    const required = new Set(wantedIndexes);
    for (const [hash, item] of indexes) {
      if (item.status === 'loading' && !required.has(hash)) {
        item.controller.abort(); indexes.delete(hash);
      }
    }
    for (const hash of required) {
      if (indexPending >= MAX_INDEX_PENDING) break;
      if (indexes.has(hash)) continue;
      const item = { status: 'loading', tiles: null, controller: new AbortController() };
      indexes.set(hash, item); indexPending++;
      (async () => {
        try {
          const response = await fetch(`indexes/${encodeURIComponent(hash)}.json`, { cache: 'force-cache', signal: item.controller.signal });
          if (!response.ok) throw new Error(`HTTP ${response.status}`);
          const data = await response.json();
          // A shard has at most 32 × 32 entries; never retain an unbounded response.
          if (!record(data.tiles) || Object.keys(data.tiles).length > 1024) throw new Error('Invalid tile index');
          item.tiles = Object.create(null);
          for (const [key, value] of Object.entries(data.tiles)) {
            if (/^-?\d+\/-?\d+\/-?\d+$/.test(key) && validHash(value)) item.tiles[key] = value;
          }
          item.status = 'ready';
        } catch { item.status = 'error'; }
        finally { indexPending--; evictIndexes(); requestDraw(); }
      })();
      evictIndexes();
    }
  }

  // Choose the coarsest source that still supplies at least one texel per device
  // pixel. Never substitute an upscaled overview for available native detail.
  function levelForScale(scale, dpr, map = state.map) {
    return clamp(Math.ceil(Math.log2(scale * dpr)), map.min_zoom, 0) || 0;
  }
  function tileHash(level, x, y, request = false) {
    const key = `${level}/${x}/${y}`, map = state.map;
    if (state.manifest.version === 1) return map.tiles[key];
    const hash = map.tile_indexes[`${level}/${Math.floor(x / 32)}/${Math.floor(y / 32)}`];
    if (!validHash(hash)) return;
    const index = touchIndex(hash);
    if (request) { protectedIndexes.add(hash); if (!index || index.status === 'loading') wantedIndexes.push(hash); }
    return index?.tiles?.[key];
  }
  function drawTile(tile, level, size) {
    const [x, y] = worldToScreen(tile.x * size, tile.y * size);
    const dpr = Math.min(window.devicePixelRatio || 1, 3);
    const dx = Math.round(x * dpr) / dpr, dy = Math.round(y * dpr) / dpr;
    const dw = Math.round((x + size * state.scale) * dpr) / dpr - dx;
    const dh = Math.round((y + size * state.scale) * dpr) / dpr - dy;
    let hash = tile.hash;
    for (let parent = level; parent >= state.map.min_zoom; parent--) {
      const factor = 2 ** (level - parent);
      if (parent !== level) hash = tileHash(parent, Math.floor(tile.x / factor), Math.floor(tile.y / factor));
      const item = validHash(hash) && touchCache(hash);
      if (item?.status !== 'ready') continue;
      protectedImages.add(hash);
      const pixels = state.manifest.tile_size, sourceSize = pixels / factor;
      const sx = ((tile.x % factor) + factor) % factor * sourceSize;
      const sy = ((tile.y % factor) + factor) % factor * sourceSize;
      ctx.drawImage(item.image, sx, sy, sourceSize, sourceSize, dx, dy, dw, dh);
      break;
    }
  }

  function draw() {
    frame = 0;
    const dpr = Math.min(window.devicePixelRatio || 1, 3);
    ctx.setTransform(dpr, 0, 0, dpr, 0, 0);
    ctx.fillStyle = '#000'; ctx.fillRect(0, 0, state.width, state.height);
    wanted = [];
    wantedIndexes = [];
    protectedImages = new Set(); protectedIndexes = new Set();
    if (!state.map) { pumpIndexes(); pumpImages(); evict(); return; }
    const map = state.map;
    const zoomLevel = levelForScale(state.scale, dpr);
    const size = state.manifest.tile_size * 2 ** -zoomLevel;
    const [left, top] = screenToWorld(0, 0), [right, bottom] = screenToWorld(state.width, state.height);
    const minX = Math.floor(left / size), maxX = Math.floor(right / size);
    const minY = Math.floor(top / size), maxY = Math.floor(bottom / size);
    const tiles = [];
    for (let y = minY; y <= maxY; y++) {
      for (let x = minX; x <= maxX; x++) {
        tiles.push({ x, y, distance: (x * size + size / 2 - state.x) ** 2 + (y * size + size / 2 - state.y) ** 2 });
      }
    }
    tiles.sort((a, b) => a.distance - b.distance);
    // Shards follow the same deterministic center-first traversal as images.
    wantedIndexes = [];
    for (const tile of tiles) tile.hash = tileHash(zoomLevel, tile.x, tile.y, true);
    // Interpolate only when shrinking actual device pixels, not merely CSS
    // pixels. High-DPI native-detail zoom must not blur pixel-art textures.
    ctx.imageSmoothingEnabled = 2 ** -zoomLevel * state.scale * dpr < 1;
    for (const tile of tiles) {
      drawTile(tile, zoomLevel, size);
      if (validHash(tile.hash)) {
        protectedImages.add(tile.hash);
        const item = cache.get(tile.hash);
        if (!item || item.status === 'loading') wanted.push(tile.hash);
      }
    }
    pumpIndexes();
    pumpImages();
    evict();
    updateHUD();
  }

  function project(x, y, z) {
    if (state.map.view === 'topdown') return [x * 16, z * 16];
    const rotations = [[x, z], [-z, x], [-x, -z], [z, -x]];
    const [rx, rz] = rotations[state.map.rotation];
    return [(rx + rz) * 16, (rz - rx) * 8 - y * 16];
  }
  function unproject(x, y, map = state.map) {
    if (map.view === 'topdown') return [x / 16, y / 16];
    const rx = x / 32 - y / 16, rz = x / 32 + y / 16;
    return [[rx, rz], [rz, -rx], [-rx, -rz], [-rz, rx]][map.rotation];
  }
  function updateHUD() {
    const [sx, sy] = pointerPosition || [state.width / 2, state.height / 2];
    const [wx, wy] = screenToWorld(sx, sy);
    const [x, z] = unproject(wx, wy + (state.map.view === 'isometric' ? 64 * 16 : 0));
    coordinates = { x: Math.round(x) || 0, z: Math.round(z) || 0,
      region: `r.${Math.floor(x / 512)}.${Math.floor(z / 512)}.mca` };
    $('coordinate-x').textContent = `X: ${coordinates.x}`;
    $('coordinate-z').textContent = `Z: ${coordinates.z}`;
    $('coordinate-region').textContent = coordinates.region;
  }

  async function copyCoordinate(value) {
    try { await navigator.clipboard.writeText(value); }
    catch {
      const input = document.createElement('textarea');
      input.value = value; input.style.position = 'fixed'; input.style.opacity = '0';
      document.body.append(input); input.select(); document.execCommand('copy'); input.remove();
    }
    $('copy-feedback').textContent = `Copied: ${value}`;
    $('copy-feedback').hidden = false;
    clearTimeout(copyTimer); copyTimer = setTimeout(() => { $('copy-feedback').hidden = true; }, 1000);
  }
  function resize() {
    state.width = canvas.clientWidth; state.height = canvas.clientHeight;
    const dpr = Math.min(window.devicePixelRatio || 1, 3);
    canvas.width = Math.round(state.width * dpr); canvas.height = Math.round(state.height * dpr);
    if (state.map) state.scale = clamp(state.scale, minScale(), 8);
    requestDraw();
  }
  function pointer(event) { const rect = canvas.getBoundingClientRect(); return { x: event.clientX - rect.left, y: event.clientY - rect.top }; }
  canvas.addEventListener('pointerdown', event => {
    if (event.pointerType === 'mouse' && event.button !== 0) return;
    canvas.focus({ preventScroll: true }); canvas.setPointerCapture(event.pointerId);
    pointers.set(event.pointerId, pointer(event)); canvas.classList.add('dragging');
  });
  canvas.addEventListener('pointermove', event => {
    const position = pointer(event);
    pointerPosition = [position.x, position.y];
    if (!pointers.has(event.pointerId)) { if (state.map) updateHUD(); return; }
    const before = [...pointers.values()];
    pointers.set(event.pointerId, position);
    const after = [...pointers.values()];
    if (before.length === 1) {
      state.x -= (position.x - before[0].x) / state.scale;
      state.y -= (position.y - before[0].y) / state.scale;
    } else if (before.length === 2) {
      const mid = points => ({ x: (points[0].x + points[1].x) / 2, y: (points[0].y + points[1].y) / 2 });
      const a = mid(before), b = mid(after);
      const distance = points => Math.hypot(points[0].x - points[1].x, points[0].y - points[1].y);
      state.x -= (b.x - a.x) / state.scale; state.y -= (b.y - a.y) / state.scale;
      if (distance(before) > 0) zoom(distance(after) / distance(before), b.x, b.y);
    }
    changed();
  });
  const release = event => { pointers.delete(event.pointerId); if (!pointers.size) canvas.classList.remove('dragging'); };
  canvas.addEventListener('pointerup', release); canvas.addEventListener('pointercancel', release); canvas.addEventListener('lostpointercapture', release);
  canvas.addEventListener('wheel', event => {
    event.preventDefault();
    const p = pointer(event), delta = event.deltaY * (event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? state.height : 1);
    zoom(Math.exp(-clamp(delta, -200, 200) * .003), p.x, p.y);
  }, { passive: false });
  canvas.addEventListener('dblclick', event => { const p = pointer(event); zoom(event.shiftKey ? .5 : 2, p.x, p.y); });
  canvas.addEventListener('contextmenu', event => {
    event.preventDefault(); const p = pointer(event); pointerPosition = [p.x, p.y];
    if (state.map) { updateHUD(); copyCoordinate(`${coordinates.x} 100 ${coordinates.z}`); }
  });
  canvas.addEventListener('keydown', event => {
    if (['+', '=', '-', '_', 'ArrowLeft', 'ArrowRight', 'ArrowUp', 'ArrowDown'].includes(event.key)) event.preventDefault();
    if (event.key === '+' || event.key === '=') zoom(2);
    else if (event.key === '-' || event.key === '_') zoom(.5);
    else if (event.key.startsWith('Arrow')) {
      const amount = (event.shiftKey ? 200 : 60) / state.scale;
      if (event.key === 'ArrowLeft') state.x -= amount;
      if (event.key === 'ArrowRight') state.x += amount;
      if (event.key === 'ArrowUp') state.y -= amount;
      if (event.key === 'ArrowDown') state.y += amount;
      changed();
    }
  });
  $('map-select').addEventListener('change', event => {
    const selected = state.manifest.maps[Number(event.target.value)];
    const rotated = state.map?.world === selected.world && state.manifest.maps.find(m => m.id === selected.id && m.rotation === state.map.rotation);
    selectMap(rotated || selected); changed();
  });
  $('zoom-in').addEventListener('click', () => zoom(2));
  $('zoom-out').addEventListener('click', () => zoom(.5));
  $('retry').addEventListener('click', refreshManifest);
  $('coordinate-x').addEventListener('click', () => copyCoordinate(String(coordinates.x)));
  $('coordinate-z').addEventListener('click', () => copyCoordinate(String(coordinates.z)));
  $('coordinate-region').addEventListener('click', () => copyCoordinate(coordinates.region));
  window.addEventListener('resize', resize);
  resize(); refreshManifest();
  setInterval(() => { if (!document.hidden) refreshManifest(); }, 30000);
  document.addEventListener('visibilitychange', () => { if (!document.hidden) { resize(); refreshManifest(); } });
})();
