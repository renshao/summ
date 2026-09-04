/*
 * The whole UI. Vanilla, no framework, no build step - `cargo build` is the
 * entire pipeline, and nothing here loads from the network except this
 * registry's own /api/v1.
 *
 * Two rules it keeps, both of which are really about the API underneath:
 *
 * - Nothing is ever fetched unpaged. Every list holds a cursor handed back by
 *   the server and asks for the next page only when someone asks for it. This
 *   page is the honesty check on that: if a screen here ever felt slow, the
 *   endpoint behind it was scanning something it should not have been.
 * - Nothing reaches the DOM as a string. Repository names, tags, platform
 *   strings and especially manifest annotations are all pushed by whoever can
 *   reach the registry, so every value goes in through `textContent`.
 */

'use strict';

const PAGE = 25;

// ---- tiny DOM helpers ----------------------------------------------------

/** `el('div', {class: 'row'}, [child, 'text'])`. Text always via textContent. */
function el(tag, attrs, children) {
  const node = document.createElement(tag);
  for (const [k, v] of Object.entries(attrs || {})) {
    if (v === null || v === undefined || v === false) continue;
    if (k === 'class') node.className = v;
    else if (k === 'text') node.textContent = v;
    else if (k.startsWith('on')) node.addEventListener(k.slice(2), v);
    else node.setAttribute(k, v);
  }
  for (const child of [].concat(children || [])) {
    if (child === null || child === undefined || child === false) continue;
    node.append(typeof child === 'string' ? document.createTextNode(child) : child);
  }
  return node;
}

const main = () => document.getElementById('main');

function render(...nodes) {
  const root = main();
  root.replaceChildren(...nodes);
  window.scrollTo(0, 0);
}

// ---- formatting ----------------------------------------------------------

const nf = new Intl.NumberFormat();

/**
 * A count the server may have stopped early. `complete: false` means the scan
 * hit its ceiling, so the number is a floor and saying so is the honest render.
 */
function tally(t) {
  if (!t) return '—';
  return t.complete ? nf.format(t.count) : nf.format(t.count) + '+';
}

function bytes(n) {
  if (!n) return ['0', 'B'];
  const units = ['B', 'kB', 'MB', 'GB', 'TB', 'PB'];
  let i = 0;
  let v = n;
  while (v >= 1000 && i < units.length - 1) { v /= 1000; i++; }
  return [i === 0 ? String(v) : v.toFixed(v < 10 ? 1 : 0), units[i]];
}

/** Enough of a digest to recognise, never enough to mistake for the whole. */
function shortDigest(d) {
  const [algo, hex] = String(d).split(':');
  return hex ? `${algo}:${hex.slice(0, 12)}` : String(d);
}

function when(unix) {
  if (!unix) return null;
  return relative(new Date(unix * 1000));
}

/*
 * Tag events are milliseconds, unlike `pushed_at` and `tagged_at` next door.
 * A second is not fine enough to order two events on one tag, so the store
 * keeps more precision here and the UI has to know which it is holding.
 */
function whenMs(millis) {
  if (!millis) return null;
  return relative(new Date(millis));
}

function relative(then) {
  const days = Math.floor((Date.now() - then.getTime()) / 86400000);
  if (days < 1) return 'today';
  if (days === 1) return 'yesterday';
  if (days < 30) return `${days} days ago`;
  return then.toISOString().slice(0, 10);
}

/** The exact instant, for a timeline where ordering is the whole point. */
function exact(millis) {
  return new Date(millis).toISOString().replace('T', ' ').replace('Z', ' UTC');
}

// ---- data ----------------------------------------------------------------

async function api(path, params) {
  const url = new URL(path, location.origin);
  for (const [k, v] of Object.entries(params || {})) {
    if (v !== null && v !== undefined && v !== '') url.searchParams.set(k, v);
  }
  const response = await fetch(url, { headers: { accept: 'application/json' } });
  if (!response.ok) {
    // The server answers in the spec's error envelope on every path, including
    // this one, so there is exactly one shape to unpack.
    let detail = `HTTP ${response.status}`;
    try {
      const body = await response.json();
      const first = body.errors && body.errors[0];
      if (first) detail = first.message || first.code || detail;
    } catch { /* a non-JSON error body is still an error */ }
    const error = new Error(detail);
    error.status = response.status;
    throw error;
  }
  return response.json();
}

/**
 * A repository name is a path, so each component is escaped separately: the
 * separators between them are real separators and must survive.
 */
const escapeName = (name) => name.split('/').map(encodeURIComponent).join('/');

const apiRepo = (name) => '/api/v1/repositories/' + escapeName(name);
const apiTags = (name) => '/api/v1/tags/' + escapeName(name);
const apiManifests = (name) => '/api/v1/manifests/' + escapeName(name);
const apiManifest = (name, ref) =>
  '/api/v1/manifests/' + escapeName(name) + '@' + encodeURIComponent(ref);
const apiTagHistory = (name, ref) =>
  '/api/v1/tag-history/' + escapeName(name) + '@' + encodeURIComponent(ref);

// ---- shared pieces -------------------------------------------------------

function crumbs(parts) {
  const nodes = [];
  parts.forEach((part, i) => {
    if (i) nodes.push(el('span', { class: 'sep', text: '/' }));
    nodes.push(part.href ? link(part.href, part.text) : el('span', { text: part.text }));
  });
  return el('div', { class: 'crumbs' }, nodes);
}

/** An internal link that routes without a reload but is still a real href. */
function link(href, text, attrs) {
  return el('a', Object.assign({ href, text }, attrs || {}), null);
}

function stat(label, value, unit) {
  return el('div', { class: 'stat' }, [
    el('dt', { text: label }),
    el('dd', {}, [document.createTextNode(value), unit ? el('span', { class: 'unit', text: unit }) : null]),
  ]);
}

function empty(message, hint) {
  return el('div', { class: 'empty' }, [message, hint ? el('span', { class: 'hint', text: hint }) : null]);
}

function failure(error) {
  return el('div', { class: 'error' }, [
    error.status === 404 ? 'Not found.' : 'Could not reach the registry.',
    el('span', { class: 'code', text: error.message }),
  ]);
}

/**
 * A list that grows by cursor.
 *
 * `load(cursor)` resolves to `{items, next}`; rows are appended and the button
 * disappears when the server stops handing back a cursor. Deliberately a
 * button and not an infinite scroll: the page count is the visible cost of a
 * scan, and hiding it would hide the thing this UI exists to keep honest.
 */
function pagedList(load, rowFor, emptyNode) {
  const list = el('div', { class: 'list' });
  const button = el('button', { class: 'more', text: 'Load more' });
  const container = el('div', {}, [list]);
  let cursor = null;
  let done = false;
  let busy = false;

  async function step() {
    if (busy || done) return;
    busy = true;
    button.disabled = true;
    button.textContent = 'Loading…';
    try {
      const { items, next } = await load(cursor);
      for (const item of items) list.append(rowFor(item));
      cursor = next;
      done = !next;
      if (done) button.remove();
      if (!list.children.length) container.replaceChildren(emptyNode);
    } catch (error) {
      container.replaceChildren(failure(error));
      done = true;
    } finally {
      busy = false;
      button.disabled = false;
      button.textContent = 'Load more';
    }
  }

  button.addEventListener('click', step);
  container.append(button);
  step();
  return container;
}

// ---- page: repositories --------------------------------------------------

function repositoriesPage(query) {
  const q = query.get('q') || '';

  const input = el('input', {
    type: 'search',
    placeholder: 'Filter by name prefix…',
    value: q,
    'aria-label': 'Filter repositories by name prefix',
    spellcheck: 'false',
    autocapitalize: 'off',
  });

  // A prefix, not a substring: it narrows the key scan rather than filtering
  // its output, which is what lets search stay one seek at ten million repos.
  let timer;
  input.addEventListener('input', () => {
    clearTimeout(timer);
    timer = setTimeout(() => {
      const next = new URLSearchParams();
      if (input.value) next.set('q', input.value);
      go('/' + (next.toString() ? '?' + next : ''), { keepFocus: true });
    }, 180);
  });

  const list = pagedList(
    (cursor) => api('/api/v1/repositories', { q, last: cursor, n: PAGE })
      .then((body) => ({ items: body.repositories, next: body.next })),
    repositoryRow,
    q ? empty('No repository starts with that.', q) : empty('This registry is empty.', 'docker push <host>/<name>:<tag>'),
  );

  render(
    el('h1', { text: 'Repositories' }),
    el('p', { class: 'subtitle' }, q
      ? ['Names starting with ', el('span', { class: 'mono', text: q }), ', in name order.']
      : ['Every repository in this registry, in name order.']),
    el('div', { class: 'toolbar' }, [el('div', { class: 'search' }, [input])]),
    list,
  );

  if (query.get('focus') !== null || q) {
    input.focus();
    input.setSelectionRange(input.value.length, input.value.length);
  }
}

function repositoryRow(repo) {
  return el('a', { class: 'row', href: '/r/' + repo.name }, [
    el('div', { class: 'row-main' }, [
      el('div', { class: 'row-title', text: repo.name }),
    ]),
    el('div', { class: 'row-meta' }, [
      el('div', {}, [el('div', { class: 'v', text: tally(repo.tags) }), el('div', { class: 'k', text: 'tags' })]),
      el('div', {}, [el('div', { class: 'v', text: tally(repo.manifests) }), el('div', { class: 'k', text: 'manifests' })]),
    ]),
  ]);
}

// ---- page: one repository ------------------------------------------------

async function repositoryPage(name, query) {
  render(el('div', { class: 'loading', text: 'Loading…' }));

  let detail;
  try {
    detail = await api(apiRepo(name));
  } catch (error) {
    render(crumbs([{ text: 'Repositories', href: '/' }, { text: name }]), failure(error));
    return;
  }

  const [size, unit] = bytes(detail.size_bytes);
  const tab = query.get('tab') === 'manifests' ? 'manifests' : 'tags';

  const tabs = el('div', { class: 'tabs', role: 'tablist' }, [
    tabLink('Tags', tally(detail.tags), tab === 'tags', '/r/' + name),
    tabLink('Manifests', tally(detail.manifests), tab === 'manifests', '/r/' + name + '?tab=manifests'),
  ]);

  const body = tab === 'manifests'
    ? pagedList(
        (cursor) => api(apiManifests(name), { last: cursor, n: PAGE })
          .then((b) => ({ items: b.manifests, next: b.next })),
        (m) => manifestRow(name, m),
        empty('No manifests in this repository.'),
      )
    : pagedList(
        (cursor) => api(apiTags(name), { last: cursor, n: PAGE })
          .then((b) => ({ items: b.tags, next: b.next })),
        (t) => tagRow(name, t),
        empty('No tags point into this repository.', 'Manifests may still be here, addressed by digest.'),
      );

  render(
    crumbs([{ text: 'Repositories', href: '/' }, { text: name }]),
    el('h1', { text: name }),
    el('dl', { class: 'stats' }, [
      stat('Tags', tally(detail.tags)),
      stat('Manifests', tally(detail.manifests)),
      stat('Blobs', tally(detail.blobs)),
      stat('Size', size, unit),
    ]),
    tabs,
    body,
  );
}

function tabLink(label, count, selected, href) {
  return el('a', {
    class: 'tab',
    href,
    role: 'tab',
    'aria-selected': selected ? 'true' : 'false',
  }, [label, el('span', { class: 'n', text: count })]);
}

/** Platforms, blob count and weight - the same summary line for both lists. */
const PLATFORM_CHIPS = 4;

function manifestFacts(m) {
  const facts = [];
  const platforms = m.platforms || [];
  for (const platform of platforms.slice(0, PLATFORM_CHIPS)) {
    facts.push(el('span', { class: 'chip', text: platform }));
  }
  // A `linux/*` image commonly ships eight or nine of these plus an
  // attestation manifest per platform. Listing them all turns a row into a
  // wall, so the rest become a count and the manifest's own page has the list.
  if (platforms.length > PLATFORM_CHIPS) {
    facts.push(el('span', { class: 'chip', text: `+${platforms.length - PLATFORM_CHIPS}` }));
  }
  if (m.children) facts.push(el('span', { text: `${m.children} manifests` }));
  if (m.blobs) {
    const [v, u] = bytes(m.blob_size);
    facts.push(el('span', { text: `${m.blobs} blobs · ${v} ${u}` }));
  }
  if (m.artifact_type) facts.push(el('span', { class: 'chip', text: m.artifact_type }));
  return facts;
}

function tagRow(repo, tag) {
  const m = tag.manifest;
  const sub = [el('span', { class: 'digest', text: shortDigest(tag.digest) })];
  if (m) sub.push(...manifestFacts(m));
  const stamp = when(tag.tagged_at) || (m && when(m.pushed_at));
  if (stamp) sub.push(el('span', { text: stamp }));

  return el('a', {
    class: 'row',
    href: `/r/${repo}?manifest=${encodeURIComponent(tag.digest)}`,
  }, [
    el('div', { class: 'row-main' }, [
      el('div', { class: 'row-title' }, [el('span', { class: 'chip tag', text: tag.name })]),
      el('div', { class: 'row-sub' }, sub),
    ]),
  ]);
}

function manifestRow(repo, m) {
  // An untagged manifest has no name but its digest, so the digest leads and
  // the row does not waste its first line saying "untagged". Most of a
  // multi-arch repository is untagged - a nine-platform index brings nine
  // children and nine attestations with it - so this is the common row, not
  // the exception.
  const tagged = m.tags.length > 0;
  const title = tagged
    ? m.tags.map((t) => el('span', { class: 'chip tag', text: t }))
    : [el('span', { class: 'mono', text: shortDigest(m.digest) })];

  const sub = [];
  if (tagged) sub.push(el('span', { class: 'digest', text: shortDigest(m.digest) }));
  sub.push(...manifestFacts(m));
  const stamp = when(m.pushed_at);
  if (stamp) sub.push(el('span', { text: stamp }));

  return el('a', {
    class: 'row',
    href: `/r/${repo}?manifest=${encodeURIComponent(m.digest)}`,
  }, [
    el('div', { class: 'row-main' }, [
      el('div', { class: 'row-title' }, title),
      el('div', { class: 'row-sub' }, sub),
    ]),
  ]);
}

// ---- page: one manifest --------------------------------------------------

async function manifestPage(name, reference) {
  render(el('div', { class: 'loading', text: 'Loading…' }));

  let m;
  try {
    m = await api(apiManifest(name, reference));
  } catch (error) {
    render(
      crumbs([{ text: 'Repositories', href: '/' }, { text: name, href: '/r/' + name }, { text: 'manifest' }]),
      failure(error),
    );
    return;
  }

  const [size, unit] = bytes(m.blob_size || m.size);
  const rows = [
    ['Digest', el('span', { class: 'mono', text: m.digest })],
    ['Media type', el('span', { class: 'mono', text: m.media_type })],
    ['Manifest size', `${nf.format(m.size)} bytes`],
  ];
  if (m.platforms.length) {
    rows.push(['Platforms', el('span', { class: 'chips' }, m.platforms.map((p) => el('span', { class: 'chip', text: p })))]);
  }
  if (m.children) rows.push(['Child manifests', nf.format(m.children)]);
  if (m.blobs) rows.push(['Blobs', `${nf.format(m.blobs)} · ${size} ${unit}`]);
  if (m.artifact_type) rows.push(['Artifact type', el('span', { class: 'mono', text: m.artifact_type })]);
  if (m.subject) rows.push(['Subject', el('span', { class: 'mono', text: m.subject })]);
  if (m.pushed_at) rows.push(['Pushed', when(m.pushed_at)]);
  if (m.tags.length) {
    // Each tag links to its own history - the other direction of the same
    // events, indexed by name instead of by digest.
    rows.push(['Tags', el('span', { class: 'chips' }, m.tags.map((t) => el('a', {
      class: 'chip tag',
      href: `/r/${name}?history=${encodeURIComponent(t)}`,
    }, [t])))]);
  }
  for (const [key, value] of Object.entries(m.annotations || {})) {
    rows.push([key, el('span', { class: 'mono', text: value })]);
  }

  const kv = el('dl', { class: 'kv' });
  for (const [k, v] of rows) {
    kv.append(el('dt', { text: k }), el('dd', {}, [typeof v === 'string' ? document.createTextNode(v) : v]));
  }

  render(
    crumbs([
      { text: 'Repositories', href: '/' },
      { text: name, href: '/r/' + name },
      { text: shortDigest(m.digest) },
    ]),
    el('h1', {}, [el('span', { class: 'mono', text: shortDigest(m.digest) })]),
    el('p', { class: 'subtitle' }, [
      'Pull with ',
      el('span', { class: 'mono', text: `${location.host}/${name}@${m.digest}` }),
    ]),
    el('div', { class: 'list' }, [kv]),
    el('h2', { class: 'section', text: 'Tag history' }),
    el('p', { class: 'subtitle', text: 'Every name this manifest has been given, newest first.' }),
    historyList(name, m.digest, 'tag'),
  );
}

// ---- tag history ---------------------------------------------------------

/*
 * The event log, newest first. A row is one push or one delete, not one state:
 * a tag re-pushed at the digest it already had is genuinely two rows, because
 * the store records pushes rather than changes.
 *
 * Paged like every other list here, except that its cursor is a pair - the
 * instant plus the tiebreaker - because two events can share a millisecond and
 * an instant alone would skip the rest of one.
 */
function historyList(name, reference, showing) {
  return pagedList(
    (cursor) => api(apiTagHistory(name, reference), {
      n: PAGE,
      before: cursor && cursor.before,
      last: cursor && cursor.last,
    }).then((b) => ({ items: b.events, next: b.next })),
    (event) => historyRow(name, event, showing),
    empty('No tag events recorded.', 'History is kept from the first push onwards.'),
  );
}

/**
 * `showing` picks the half of the event the caller did *not* ask about: a tag's
 * history is a list of digests, a manifest's is a list of names.
 */
function historyRow(name, event, showing) {
  const created = event.event !== 'deleted';
  const title = showing === 'tag'
    ? el('span', { class: 'chip tag', text: event.tag })
    : el('span', { class: 'digest', text: shortDigest(event.digest) });

  const sub = [el('span', { title: exact(event.at), text: whenMs(event.at) })];
  if (event.media_type) sub.push(el('span', { text: event.media_type }));
  if (event.size) {
    const [size, unit] = bytes(event.size);
    sub.push(el('span', { text: `${size} ${unit}` }));
  }

  // A deleted event names a manifest that may no longer be here - the
  // descriptor is denormalised into the event precisely so this row still
  // renders - so only a created one is a link.
  const attrs = { class: 'row event' + (created ? '' : ' gone') };
  if (created) attrs.href = `/r/${name}?manifest=${encodeURIComponent(event.digest)}`;

  return el(created ? 'a' : 'div', attrs, [
    el('div', { class: 'row-main' }, [
      el('div', { class: 'row-title' }, [
        el('span', { class: 'chip ' + (created ? 'created' : 'deleted'), text: created ? 'created' : 'deleted' }),
        title,
      ]),
      el('div', { class: 'row-sub' }, sub),
    ]),
  ]);
}

/** One tag's history: what this name has pointed at, newest first. */
function tagHistoryPage(name, tag) {
  render(
    crumbs([
      { text: 'Repositories', href: '/' },
      { text: name, href: '/r/' + name },
      { text: tag },
    ]),
    el('h1', {}, [el('span', { class: 'chip tag', text: tag })]),
    el('p', { class: 'subtitle', text: 'Every push and delete of this tag, newest first.' }),
    historyList(name, tag, 'digest'),
  );
}

// ---- routing -------------------------------------------------------------

/**
 * Client-side, over real paths rather than a hash, because a repository page
 * should be a link somebody can paste. The server serves the shell for every
 * unknown path (see `ui.rs`), which is what makes a reload of one work.
 */
function route() {
  const path = decodeURI(location.pathname);
  const query = new URLSearchParams(location.search);
  document.title = 'Summ';

  if (path === '/' || path === '') return repositoriesPage(query);

  if (path.startsWith('/r/')) {
    const name = path.slice(3).replace(/\/+$/, '');
    if (!name) return repositoriesPage(query);
    document.title = `${name} · Summ`;
    const manifest = query.get('manifest');
    const history = query.get('history');
    if (history) return tagHistoryPage(name, history);
    return manifest ? manifestPage(name, manifest) : repositoryPage(name, query);
  }

  render(
    crumbs([{ text: 'Repositories', href: '/' }]),
    el('div', { class: 'error' }, ['No such page.', el('span', { class: 'code', text: path })]),
  );
}

function go(href, options) {
  history.pushState(null, '', href);
  route();
  if (options && options.keepFocus) {
    const input = main().querySelector('input[type=search]');
    if (input) {
      input.focus();
      input.setSelectionRange(input.value.length, input.value.length);
    }
  }
}

// One delegated listener rather than one per link, so rows rendered later are
// routed without anything having to remember to wire them up.
document.addEventListener('click', (event) => {
  if (event.defaultPrevented || event.button !== 0) return;
  if (event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) return;
  const anchor = event.target.closest('a');
  if (!anchor) return;
  const href = anchor.getAttribute('href');
  if (!href || !href.startsWith('/')) return;
  // `/v2/` and `/api/` are the registry, not the UI: let the browser have them.
  if (href.startsWith('/v2/') || href.startsWith('/api/')) return;
  event.preventDefault();
  go(href);
});

window.addEventListener('popstate', route);

document.getElementById('host').textContent = location.host;
route();
