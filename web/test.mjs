// Run with: node --test web/test.mjs (no browser or npm dependencies).
import { readFile } from 'node:fs/promises';
import vm from 'node:vm';
import test from 'node:test';
import assert from 'node:assert/strict';
import { performance } from 'node:perf_hooks';

const source = await readFile(new URL('./app.js', import.meta.url), 'utf8');
const map = (extra = {}) => ({ id: 'survival', world: 'survival', view: 'topdown', rotation: 0,
  mode: 'daylight', bounds: [-16384, -16384, 16384, 16384], min_zoom: -8, max_zoom: 0,
  tile_indexes: {}, ...extra });
const root = maps => ({ version: 2, tile_size: 512, generation: 'one', maps });
const response = (data, status = 200) => ({ ok: status === 200, status, json: async () => data });

async function viewer(manifest, handler = () => response({}, 404), search = '', dpr = 1) {
  const elements = new Map(), frames = [], calls = [], images = [], paints = [], draws = [], renderOptions = {};
  let now = 1000000;
  const context = new Proxy({}, {
    get: (_, key) => key === 'measureText' ? () => ({ width: 20 }) : key === 'drawImage' ? (...args) => draws.push(args) : () => {},
    set: (_, key, value) => { renderOptions[key] = value; if (key === 'fillStyle') paints.push(value); return true; }
  });
  function element(id) {
    if (!elements.has(id)) elements.set(id, { clientWidth: 800, clientHeight: 600, style: {},
      events: {}, classList: { add() {}, remove() {} }, getContext: () => context,
      getBoundingClientRect: () => ({ left: 0, top: 0 }),
      replaceChildren() {}, append() {}, setAttribute() {},
      addEventListener(name, fn) { this.events[name] = fn; } });
    return elements.get(id);
  }
  class Clock extends Date { static now() { return now; } }
  const sandbox = { URL, URLSearchParams, AbortController, Date: Clock, console,
    location: { href: `https://atlas.test/maps/${search}`, origin: 'https://atlas.test', search },
    history: { replaceState() {} }, navigator: {},
    document: { getElementById: element, createElement: () => ({}), addEventListener() {} },
    window: { devicePixelRatio: dpr, addEventListener() {} },
    setTimeout: () => 1, clearTimeout() {}, setInterval() {},
    requestAnimationFrame: fn => { frames.push(fn); return frames.length; },
    Image: class { constructor() { images.push(this); } removeAttribute() { this.cancelled = true; } },
    fetch: async (url, options) => {
      calls.push({ url, options });
      if (url === 'current.json') return response(manifest);
      return handler(url, options);
    } };
  // Expose internals only in this VM; the shipped viewer has no testing globals.
  vm.runInNewContext(source.replace(/\}\)\(\);\s*$/, `globalThis.api = {
    state, indexes, cache, live, selectMap, project, unproject, headURL, refreshLive,
    pumpIndexes, evictIndexes, draw, validMap, pumpImages, evict, levelForScale,
    stats, indexChunks, drawChunks, visibleChunks,
    get imageBytes() { return imageBytes; },
    get pending() { return pending; },
    setImages: value => { wanted = value; },
    setWanted: value => { wantedIndexes = value; },
    get indexPending() { return indexPending; }
  }; })();`), sandbox);
  const settle = async () => { for (let i = 0; i < 12; i++) await Promise.resolve(); };
  await settle();
  return { ...sandbox.api, api: sandbox.api, calls, images, elements, paints, draws, frames, settle, renderOptions,
    advance: ms => { now += ms; },
    draw: async () => { frames.length = 0; sandbox.api.draw(); await settle(); } };
}

test('v2 loads only visible shards, uses Euclidean negative coordinates, and repaints tiles', async () => {
  const v = await viewer(root([map({ min_zoom: 0, tile_indexes: {
    '0/-1/-1': 'negative', '0/0/0': 'positive', '0/50/50': 'offscreen'
  } })]), url => response({ tiles: url.includes('negative') ? { '0/-1/-1': 'west' } : { '0/0/0': 'east' } }));
  assert.ok(v.state.map);
  v.state.x = 0; v.state.y = 0; v.state.scale = 1;
  await v.draw();
  assert.deepEqual(v.calls.filter(c => c.url.startsWith('indexes/')).map(c => c.url).sort(),
    ['indexes/negative.json', 'indexes/positive.json']);
  await v.draw();
  assert.deepEqual(v.images.map(i => i.src).sort(), ['tiles/east.webp', 'tiles/west.webp']);
  assert.equal(v.calls.find(c => c.url.startsWith('indexes/')).options.cache, 'force-cache');
});

test('missing shard is empty, and v1 tiles remain compatible', async () => {
  const empty = await viewer(root([map()]));
  await empty.draw();
  assert.equal(empty.calls.filter(c => c.url.startsWith('indexes/')).length, 0);
  const legacy = await viewer({ ...root([map({ tiles: { '0/0/0': 'legacy' }, min_zoom: 0 })]), version: 1 });
  legacy.state.x = 0; legacy.state.y = 0; legacy.state.scale = 1;
  await legacy.draw();
  assert.equal(legacy.images[0].src, 'tiles/legacy.webp');
});

test('index requests are capped at four and completed shard LRU at 128', async () => {
  const releases = [];
  const v = await viewer(root([map()]), url => url.startsWith('indexes/')
    ? new Promise(resolve => releases.push(() => resolve(response({ tiles: {} })))) : response({}, 404));
  v.api.setWanted(['a', 'b', 'c', 'd', 'e', 'f']);
  v.pumpIndexes();
  assert.equal(v.api.indexPending, 4);
  assert.equal(releases.length, 4);
  for (const release of releases) release();
  await v.settle();
  assert.equal(v.api.indexPending, 0);
  for (let i = 0; i < 140; i++) v.indexes.set(`old-${i}`, { status: 'ready', tiles: {} });
  v.evictIndexes();
  assert.equal(v.indexes.size, 128);
  assert.equal(v.indexes.has('old-139'), true);
  assert.equal(v.indexes.has('old-0'), false);
});

test('rotations preserve world-plane center; projection changes fit; initial default zoom is used', async () => {
  const a = map({ view: 'isometric', default_zoom: 7 });
  const b = map({ view: 'isometric', rotation: 1 });
  const c = map({ id: 'top' });
  const v = await viewer(root([a, b, c]));
  assert.equal(v.state.scale, .5); // -8 + min(7, 8)
  [v.state.x, v.state.y] = v.project(123, 0, -456);
  v.state.scale = .25;
  v.selectMap(b);
  assert.deepEqual(Array.from(v.unproject(v.state.x, v.state.y)), [123, -456]);
  assert.equal(v.state.scale, .25);
  v.selectMap(c);
  assert.equal(v.state.x, 0);
  assert.equal(v.state.y, 0);
  assert.notEqual(v.state.scale, .25);
});

test('404 live feed retries after 60 seconds and numeric player dimensions use world', async () => {
  let ready = false;
  const v = await viewer(root([map()]), url => url === 'live/players.json' && ready
    ? response({ updated_at: 1060, players: [{ name: 'Alice', world: 'survival', dimension: 0, x: 0, y: 64, z: 0 }] })
    : response({}, 404));
  ready = true;
  const before = v.calls.length;
  await v.refreshLive('players');
  assert.equal(v.calls.length, before);
  v.advance(60000);
  await v.refreshLive('players');
  assert.equal(v.live.players.disabled, false);
  assert.equal(v.state.players.length, 1);
  assert.equal(v.elements.get('players-count').textContent, '1 in this world');
});

test('head assets resolve relative and root-relative local URLs, not foreign origins', async () => {
  const v = await viewer(root([map()]));
  assert.equal(v.headURL('/heads/alice.png'), 'https://atlas.test/heads/alice.png');
  assert.equal(v.headURL('heads/alice.png'), 'https://atlas.test/maps/heads/alice.png');
  for (const unsafe of ['//evil.test/x', 'https://evil.test/x', 'javascript:alert(1)', 'data:image/png,x', '\\evil.test/x']) {
    assert.equal(v.headURL(unsafe), null);
  }
});

test('image queue remains capped at eight and decoded LRU at 64 MiB', async () => {
  const v = await viewer(root([map()]));
  v.api.setImages(Array.from({ length: 20 }, (_, i) => `image-${i}`));
  v.pumpImages();
  assert.equal(v.images.length, 8);
  for (const image of v.images) image.onload();
  for (let batch = 0; batch < 12; batch++) {
    v.api.setImages(Array.from({ length: 8 }, (_, i) => `batch-${batch}-${i}`));
    v.pumpImages();
    for (const image of v.images.slice(-8)) image.onload();
  }
  v.evict();
  assert.equal(v.cache.size, 64);
  assert.equal(v.api.imageBytes, 64 * 1024 * 1024);
});

test('numeric chunk types have distinct colors and the overlay legend is shown', async () => {
  const v = await viewer(root([map()]), url => url === 'live/chunks.json'
    ? response({ updated_at: 1000, chunks: [0, 1, 2].map(type => ({ world: 'survival', x: 0, z: 0, type })) })
    : response({}, 404));
  v.elements.get('chunks-toggle').events.click();
  await v.settle();
  await v.draw();
  for (const color of ['#74c4fc26', '#f2c66d26', '#a1e5b226']) assert.ok(v.paints.includes(color));
  assert.equal(v.elements.get('chunk-legend').hidden, false);
});

test('256px manifests fit using their tile size; unsupported sizes are rejected', async () => {
  const v = await viewer({ ...root([map({ bounds: [0, 0, 0, 0] })]), tile_size: 256 });
  assert.ok(v.state.map);
  assert.equal(v.state.scale, 600 * .65 / 256);
  const bad = await viewer({ ...root([map()]), tile_size: 128 });
  assert.equal(bad.state.map, null);
});

test('LOD respects DPR and never undersamples available native detail', async () => {
  const v = await viewer(root([map()]));
  for (const [scale, dpr, expected] of [[.5, 1, -1], [.5, 2, 0], [.25, 2, -1],
    [.25, 3, 0], [1, 1, 0], [1, 2, 0], [8, 3, 0], [.125, 1, -3]]) {
    assert.equal(v.levelForScale(scale, dpr), expected);
  }
  for (const dpr of [1, 2, 3]) for (const scale of [.01, .07, .2, .49, .51, .9]) {
    const level = v.levelForScale(scale, dpr);
    if (level < 0) assert.ok(2 ** level >= scale * dpr);
  }
  const hi = await viewer({ ...root([map({ tiles: { '0/0/0': 'native', '-1/0/0': 'overview' } })]), version: 1 }, undefined, '', 2);
  hi.state.scale = .5; hi.state.x = 256; hi.state.y = 256;
  await hi.draw();
  assert.equal(hi.images[0].src, 'tiles/native.webp');
});

test('high-DPI magnification stays sharp and fractional minification interpolates', async () => {
  const v = await viewer(root([map()]), undefined, '', 2);
  v.state.scale = .75;
  await v.draw();
  assert.equal(v.renderOptions.imageSmoothingEnabled, false);
  v.state.scale = .3;
  await v.draw();
  assert.equal(v.renderOptions.imageSmoothingEnabled, true);
});

test('obsolete image loads release all slots; duplicate hashes load once in priority order', async () => {
  const v = await viewer(root([map()]));
  v.api.setImages(Array.from({ length: 8 }, (_, i) => `old-${i}`));
  v.pumpImages();
  const late = v.images[0].onload;
  v.api.setImages(['center', 'center', 'near', 'edge']);
  v.pumpImages();
  assert.ok(v.images.slice(0, 8).every(i => i.cancelled));
  assert.deepEqual(v.images.slice(8).map(i => i.src), ['tiles/center.webp', 'tiles/near.webp', 'tiles/edge.webp']);
  assert.equal(v.api.pending, 3);
  // Browsers do not dispatch detached handlers; the old callback is also inert.
  assert.equal(v.images[0].onload, null);
  assert.equal(typeof late, 'function');
  late(); assert.equal(v.api.pending, 3);
  for (const image of v.images.slice(8)) image.onload();
  assert.equal(v.api.pending, 0);
  assert.equal(v.frames.length, 1, 'completions coalesce into one RAF');
});

test('obsolete shard fetches abort and current center shard takes priority', async () => {
  const v = await viewer(root([map()]), (url, options) => {
    if (!url.startsWith('indexes/')) return response({}, 404);
    return new Promise((resolve, reject) => options.signal.addEventListener('abort', () => reject(new Error('aborted'))));
  });
  v.api.setWanted(['old-a', 'old-b', 'old-c', 'old-d']); v.pumpIndexes();
  v.api.setWanted(['center', 'center', 'edge']); v.pumpIndexes();
  await v.settle();
  v.pumpIndexes();
  assert.equal(v.api.indexPending, 2);
  assert.deepEqual(v.calls.filter(c => c.url.startsWith('indexes/')).slice(-2).map(c => c.url),
    ['indexes/center.json', 'indexes/edge.json']);
  assert.ok(v.calls.filter(c => c.url.includes('old-')).every(c => c.options.signal.aborted));
});

test('images become drawable only after async decode; stale decode completion is inert', async () => {
  const v = await viewer(root([map()]));
  v.api.setImages(['decoding']); v.pumpImages();
  let decoded;
  v.images[0].decode = () => new Promise(resolve => { decoded = resolve; });
  v.images[0].onload();
  assert.equal(v.cache.get('decoding').status, 'loading');
  assert.equal(v.api.pending, 1);
  v.api.setImages(['replacement']); v.pumpImages();
  decoded(); await v.settle();
  assert.equal(v.cache.has('decoding'), false);
  assert.equal(v.api.pending, 1);
  assert.equal(v.api.imageBytes, 0);
});

test('cached parent fills missing children with correct negative-coordinate cropping', async () => {
  const v = await viewer({ ...root([map({ tiles: { '-1/-1/-1': 'parent', '0/-1/-1': 'fine' } })]), version: 1 });
  const parent = {};
  v.cache.set('parent', { status: 'ready', image: parent });
  v.state.x = -256; v.state.y = -256; v.state.scale = 1;
  await v.draw();
  const crop = v.draws.find(d => d[0] === parent && d[1] === 256 && d[2] === 256);
  assert.ok(crop);
  assert.deepEqual(crop.slice(1, 5), [256, 256, 256, 256]);
  assert.equal(v.images[0].src, 'tiles/fine.webp');
  v.images[0].onload(); await v.draw();
  assert.ok(v.draws.some(d => d[0] === v.images[0] && d[3] === 512));
});

test('a visible working set larger than budget stays decoded without request thrash', async t => {
  const tiles = {};
  for (let y = -5; y <= 5; y++) for (let x = -5; x <= 5; x++) tiles[`0/${x}/${y}`] = `tile-${x}-${y}`;
  const v = await viewer({ ...root([map({ tiles })]), version: 1 });
  v.state.width = 4096; v.state.height = 4096; v.state.scale = 1;
  for (let batch = 0; batch < 20; batch++) {
    await v.draw();
    for (const image of v.images) if (image.onload && !image.completed) { image.completed = true; image.onload(); }
  }
  const requests = v.images.length;
  assert.equal(requests, 81);
  assert.equal(v.api.pending, 0);
  assert.equal(v.api.imageBytes, 81 * 512 * 512 * 4);
  await v.draw(); assert.equal(v.images.length, requests);
  t.diagnostic(`4096² CSS viewport: ${requests} unique decoded tiles, ${v.api.imageBytes / 1024 ** 2} MiB, 8 concurrent requests max; no reloads`);
  v.state.x = 1000000; await v.draw();
  assert.ok(v.api.imageBytes <= 64 * 1024 ** 2);
});

test('million-chunk overlay indexes once and examines only viewport buckets', async t => {
  const v = await viewer(root([map()]));
  v.state.chunks = Array.from({ length: 1000000 }, (_, i) => ({ world: 'survival', x: i % 1000 - 500, z: Math.floor(i / 1000) - 500, type: i % 3 }));
  const start = performance.now(); v.indexChunks();
  const built = performance.now() - start;
  v.state.x = 0; v.state.y = 0; v.state.scale = 1;
  const drawStart = performance.now();
  for (let i = 0; i < 100; i++) v.drawChunks();
  const drawMs = (performance.now() - drawStart) / 100;
  assert.ok(v.api.stats.chunkCandidates <= 4096);
  assert.equal(v.state.chunks.length, 1000000, 'no subsampling or silent chunk loss');
  t.diagnostic(`1,000,000 chunks: index ${built.toFixed(1)} ms; ${v.api.stats.chunkCandidates} candidates/frame; mock canvas draw ${drawMs.toFixed(2)} ms/frame (100 frames, not GPU timing)`);
});

test('hover updates coordinates without repainting the map canvas', async () => {
  const v = await viewer(root([map()]));
  await v.draw();
  v.elements.get('map').events.pointermove({ clientX: 10, clientY: 20, pointerId: 1 });
  assert.equal(v.frames.length, 0);
  assert.ok(v.elements.get('coordinates').textContent.startsWith('X '));
});
