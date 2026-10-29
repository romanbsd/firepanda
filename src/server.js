import http from 'node:http';
import { randomUUID } from 'node:crypto';
import { setTimeout as delay } from 'node:timers/promises';
import puppeteer from 'puppeteer-core';
import TurndownService from 'turndown';

const PORT = Number(process.env.PORT || 4000);
const HOST = process.env.HOST || '0.0.0.0';
const CDP_URL = process.env.LIGHTPANDA_CDP_URL || 'http://127.0.0.1:9222';
const API_KEY = process.env.API_KEY || '';
const MAX_BODY = Number(process.env.MAX_BODY_BYTES || 1_048_576);
const MAX_PAGES = Number(process.env.MAX_CRAWL_PAGES || 100);
const MAX_CONCURRENCY = Math.max(1, Number(process.env.MAX_CONCURRENCY || 4));
const JOB_TTL_MS = Number(process.env.JOB_TTL_MS || 86_400_000);
const turndown = new TurndownService({ headingStyle: 'atx', codeBlockStyle: 'fenced' });
const jobs = new Map();
let browserPromise;
let scrapeQueue = Promise.resolve();

function json(res, status, body) {
  const payload = JSON.stringify(body);
  res.writeHead(status, { 'content-type': 'application/json; charset=utf-8', 'content-length': Buffer.byteLength(payload) });
  res.end(payload);
}

async function bodyJson(req) {
  const chunks = [];
  let size = 0;
  for await (const chunk of req) {
    size += chunk.length;
    if (size > MAX_BODY) throw Object.assign(new Error('Request body too large'), { status: 413 });
    chunks.push(chunk);
  }
  try { return JSON.parse(Buffer.concat(chunks).toString('utf8') || '{}'); }
  catch { throw Object.assign(new Error('Request body must be valid JSON'), { status: 400 }); }
}

function normalizeUrl(value) {
  let url;
  try { url = new URL(value); } catch { throw Object.assign(new Error('A valid url is required'), { status: 400 }); }
  if (!['http:', 'https:'].includes(url.protocol)) throw Object.assign(new Error('Only http and https URLs are supported'), { status: 400 });
  if (url.username || url.password) throw Object.assign(new Error('URLs with embedded credentials are not supported'), { status: 400 });
  return url;
}

function getBrowser() {
  if (!browserPromise) {
    const connection = CDP_URL.startsWith('ws')
      ? puppeteer.connect({ browserWSEndpoint: CDP_URL })
      : puppeteer.connect({ browserURL: CDP_URL });
    browserPromise = connection;
    connection.then(browser => browser.once('disconnected', () => {
      if (browserPromise === connection) browserPromise = undefined;
    })).catch(() => {
      if (browserPromise === connection) browserPromise = undefined;
    });
  }
  return browserPromise;
}

async function scrapePage(input) {
  if (!input || typeof input.url !== 'string') throw Object.assign(new Error('A url is required'), { status: 400 });
  const target = normalizeUrl(input.url);
  const options = input.scrapeOptions || input;
  const formats = options.formats || ['markdown'];
  if (!Array.isArray(formats)) throw Object.assign(new Error('formats must be an array'), { status: 400 });
  if (formats.some(x => typeof x !== 'string' && (!x || typeof x !== 'object' || typeof x.type !== 'string'))) {
    throw Object.assign(new Error('Each format must be a string or an object with a type'), { status: 400 });
  }
  const formatTypes = formats.map(x => typeof x === 'string' ? x : x.type);
  const unsupported = formatTypes.filter(x => !['markdown', 'html', 'rawHtml', 'links'].includes(x));
  if (unsupported.length) throw Object.assign(new Error(`Unsupported formats: ${unsupported.join(', ')}. Supported formats are markdown, html, rawHtml, and links.`), { status: 400 });
  const browser = await getBrowser();
  const page = await browser.newPage();
  const timeout = Math.max(1000, Math.min(Number(options.timeout || 30000), 120000));
  page.setDefaultNavigationTimeout(timeout);
  try {
    if (options.headers && typeof options.headers === 'object') await page.setExtraHTTPHeaders(options.headers);
    if (options.mobile) await page.setViewport({ width: 390, height: 844, isMobile: true, hasTouch: true });
    const response = await page.goto(target.href, { waitUntil: 'domcontentloaded', timeout });
    if (options.waitFor) await delay(Math.min(Number(options.waitFor), 30000));
    const data = await page.evaluate(({ includeTags, excludeTags, onlyMainContent }) => {
      const clone = document.documentElement.cloneNode(true);
      const selectorsToRemove = [...new Set([...(excludeTags || []), ...(onlyMainContent ? ['script', 'style', 'noscript', 'template', 'svg', 'nav', 'footer', 'header', 'aside'] : [])])];
      for (const selector of selectorsToRemove) {
        try { clone.querySelectorAll(selector).forEach(el => el.remove()); } catch {}
      }
      let content = clone.querySelector('main, article, [role="main"]') || clone.body || clone;
      if (includeTags?.length) {
        const selected = includeTags.flatMap(selector => { try { return [...content.querySelectorAll(selector)]; } catch { return []; } });
        if (selected.length) { const wrapper = document.createElement('div'); selected.forEach(el => wrapper.append(el.cloneNode(true))); content = wrapper; }
      } else if (!onlyMainContent) content = clone.body || clone;
      return {
        title: document.title || '',
        description: document.querySelector('meta[name="description"]')?.content || '',
        author: document.querySelector('meta[name="author"]')?.content || undefined,
        keywords: document.querySelector('meta[name="keywords"]')?.content || undefined,
        ogTitle: document.querySelector('meta[property="og:title"]')?.content || undefined,
        ogDescription: document.querySelector('meta[property="og:description"]')?.content || undefined,
        ogImage: document.querySelector('meta[property="og:image"]')?.content || undefined,
        language: document.documentElement.lang || '',
        canonical: document.querySelector('link[rel="canonical"]')?.href || location.href,
        contentHtml: content.innerHTML || '',
        html: document.documentElement.outerHTML,
        links: [...document.querySelectorAll('a[href]')].map(a => ({ url: a.href, text: (a.innerText || a.textContent || '').trim(), title: a.title || undefined })).filter(a => a.url),
      };
    }, { includeTags: options.includeTags, excludeTags: options.excludeTags, onlyMainContent: options.onlyMainContent !== false });
    const markdown = turndown.turndown(data.contentHtml);
    const result = {};
    if (formatTypes.includes('markdown')) result.markdown = markdown;
    if (formatTypes.includes('html')) result.html = data.contentHtml;
    if (formatTypes.includes('rawHtml')) result.rawHtml = data.html;
    if (formatTypes.includes('links')) result.links = data.links.map(link => link.url);
    const metadata = {
      title: data.title,
      description: data.description,
      language: data.language,
      sourceURL: target.href,
      url: target.href,
      statusCode: response?.status() ?? 0,
      ...(data.author ? { author: data.author } : {}),
      ...(data.keywords ? { keywords: data.keywords } : {}),
      ...(data.ogTitle ? { ogTitle: data.ogTitle } : {}),
      ...(data.ogDescription ? { ogDescription: data.ogDescription } : {}),
      ...(data.ogImage ? { ogImage: data.ogImage } : {}),
      ...(data.canonical ? { canonical: data.canonical } : {}),
    };
    return { ...result, metadata, _links: data.links };
  } finally {
    await page.close().catch(() => {});
  }
}

async function scrape(input) {
  const previous = scrapeQueue;
  let release;
  scrapeQueue = new Promise(resolve => { release = resolve; });
  await previous;
  try { return await scrapePage(input); }
  finally { release(); }
}

function validatePathPatterns(input) {
  if (!Array.isArray(input.includePaths ?? []) || !Array.isArray(input.excludePaths ?? [])) {
    throw Object.assign(new Error('includePaths and excludePaths must be arrays'), { status: 400 });
  }
  const patterns = [...(input.includePaths || []), ...(input.excludePaths || [])];
  if (patterns.length > 1000 || patterns.some(pattern => typeof pattern !== 'string' || pattern.length > 2000) || patterns.reduce((sum, pattern) => sum + pattern.length, 0) > 100000) {
    throw Object.assign(new Error('Path filters exceed Firecrawl limits'), { status: 400 });
  }
  try { patterns.forEach(pattern => new RegExp(pattern)); }
  catch { throw Object.assign(new Error('Path filters must contain valid regular expressions'), { status: 400 }); }
}

function matchesPath(url, patterns = [], fullUrl = false) {
  return patterns.some(pattern => {
    try { return new RegExp(pattern).test(fullUrl ? url.href : url.pathname); }
    catch { return false; }
  });
}

async function runCrawl(job) {
  job.status = 'scraping';
  const base = new URL(job.input.url);
  const queue = [{ url: base.href, depth: 0 }];
  const seen = new Set();
  const concurrency = Math.max(1, Math.min(MAX_CONCURRENCY, Number(job.input.maxConcurrency) || MAX_CONCURRENCY));
  const limit = Math.max(1, Math.min(Number(job.input.limit || 10), MAX_PAGES));
  const maxDepth = Math.max(0, Number(job.input.maxDiscoveryDepth ?? job.input.maxDepth ?? 10));
  try {
    while (queue.length && seen.size < limit && !job.cancelled) {
      const batch = [];
      while (queue.length && batch.length < concurrency && seen.size + batch.length < limit) {
        const item = queue.shift();
        if (seen.has(item.url)) continue;
        seen.add(item.url);
        batch.push(item);
      }
      job.total = job.data.length + queue.length + batch.length;
      const pages = await Promise.all(batch.map(async item => {
        if (matchesPath(new URL(item.url), job.input.excludePaths, job.input.regexOnFullURL)) return null;
        if (job.input.includePaths?.length && !matchesPath(new URL(item.url), job.input.includePaths, job.input.regexOnFullURL)) return null;
        try { return { item, data: await scrape({ url: item.url, ...(job.input.scrapeOptions || {}) }) }; }
        catch (error) { job.errors.push({ url: item.url, error: error.message }); return null; }
      }));
      for (const result of pages.filter(Boolean)) {
        const links = result.data._links || [];
        delete result.data._links;
        job.data.push(result.data);
        if (result.item.depth < maxDepth) for (const { url: href } of links) {
          try {
            const next = new URL(href, result.item.url);
            next.hash = '';
            if (next.origin === base.origin && !seen.has(next.href) && !queue.some(item => item.url === next.href)) queue.push({ url: next.href, depth: result.item.depth + 1 });
          } catch {}
        }
      }
      job.completed = job.data.length;
      job.total = job.data.length + queue.length;
    }
    job.status = job.cancelled ? 'cancelled' : 'completed';
  } catch (error) {
    job.status = 'failed'; job.error = error.message;
  }
  job.finishedAt = new Date().toISOString();
}

function jobStatus(job) {
  const now = Date.now();
  const createdAt = Date.parse(job.createdAt);
  const completedAt = job.finishedAt || undefined;
  return {
    status: job.status,
    total: job.total,
    completed: job.completed,
    creditsUsed: job.data.length,
    expiresAt: new Date(createdAt + JOB_TTL_MS).toISOString(),
    createdAt: job.createdAt,
    ...(completedAt ? { completedAt } : {}),
    duration: Math.max(0, ((completedAt ? Date.parse(completedAt) : now) - createdAt) / 1000),
    data: job.data,
    ...(job.error ? { error: job.error } : {}),
  };
}

function apiBaseUrl(req) {
  const forwardedProto = req.headers['x-forwarded-proto']?.split(',')[0].trim();
  const forwardedHost = req.headers['x-forwarded-host']?.split(',')[0].trim();
  return `${forwardedProto || (req.socket.encrypted ? 'https' : 'http')}://${forwardedHost || req.headers.host || `localhost:${PORT}`}`;
}

function pruneJobs() {
  const cutoff = Date.now() - JOB_TTL_MS;
  for (const [id, job] of jobs) {
    if (job.finishedAt && Date.parse(job.finishedAt) < cutoff) jobs.delete(id);
  }
}

async function route(req, res) {
  const url = new URL(req.url, `http://${req.headers.host || 'localhost'}`);
  const parts = url.pathname.split('/').filter(Boolean);
  if (url.pathname === '/health' || url.pathname === '/healthz') return json(res, 200, { ok: true, browser: 'lightpanda' });
  if (API_KEY && req.headers.authorization !== `Bearer ${API_KEY}`) return json(res, 401, { success: false, error: 'Unauthorized' });

  const endpoint = parts[0] === 'v1' || parts[0] === 'v2' ? parts.slice(1) : parts;
  if (req.method === 'POST' && endpoint[0] === 'scrape' && endpoint.length === 1) {
    const input = await bodyJson(req);
    const result = await scrape(input);
    delete result._links;
    return json(res, 200, { success: true, data: result });
  }
  if (req.method === 'POST' && endpoint[0] === 'map') {
    const input = await bodyJson(req);
    const page = await scrape({ url: input.url, formats: ['links', 'html'], timeout: input.timeout });
    const base = new URL(input.url);
    let links = new Map([[base.href, {
      url: base.href,
      title: page.metadata.title || '',
      description: page.metadata.description || '',
    }]]);
    for (const link of page._links || []) {
      try {
        const target = new URL(link.url, base);
        target.hash = '';
        if ((target.hostname !== base.hostname && !(input.includeSubdomains && target.hostname.endsWith(`.${base.hostname}`))) || matchesPath(target, input.excludePaths)) continue;
        if (input.ignoreQueryParameters) target.search = '';
        links.set(target.href, { url: target.href, title: link.title || link.text || '', description: link.text || '' });
      } catch {}
    }
    let result = [...links.values()];
    if (input.search) result = result.filter(link => `${link.url} ${link.title || ''} ${link.description || ''}`.toLowerCase().includes(String(input.search).toLowerCase()));
    result = result.slice(0, Math.max(1, Math.min(Number(input.limit || 5000), 100000)));
    return json(res, 200, { success: true, links: result });
  }
  if (req.method === 'POST' && endpoint[0] === 'crawl' && endpoint.length === 1) {
    const input = await bodyJson(req); normalizeUrl(input.url);
    validatePathPatterns(input);
    pruneJobs();
    input.limit = Math.max(1, Math.min(Number(input.limit || 10) || 10, MAX_PAGES));
    const id = randomUUID();
    const job = { id, input, status: 'queued', total: 1, completed: 0, data: [], errors: [], cancelled: false, createdAt: new Date().toISOString() };
    jobs.set(id, job); runCrawl(job);
    const basePath = `/${parts[0] === 'v2' ? 'v2' : 'v1'}/crawl/${id}`;
    return json(res, 200, { success: true, id, url: new URL(basePath, apiBaseUrl(req)).href });
  }
  if (req.method === 'POST' && endpoint[0] === 'batch' && endpoint[1] === 'scrape') {
    const input = await bodyJson(req);
    if (!Array.isArray(input.urls)) throw Object.assign(new Error('urls must be an array'), { status: 400 });
    pruneJobs();
    const candidates = input.urls.slice(0, MAX_PAGES);
    const invalidURLs = [];
    input.urls = candidates.filter(value => {
      try { normalizeUrl(value); return true; }
      catch { invalidURLs.push(value); return false; }
    });
    if (invalidURLs.length && input.ignoreInvalidURLs === false) {
      return json(res, 400, { success: false, error: 'Invalid URLs in urls array', invalidURLs });
    }
    const id = randomUUID();
    const job = { id, input: { url: input.urls[0], limit: input.urls.length, maxDepth: 0, scrapeOptions: input }, status: 'queued', total: input.urls.length, completed: 0, data: [], errors: [], cancelled: false, createdAt: new Date().toISOString() };
    jobs.set(id, job);
    (async () => { const concurrency = Math.max(1, Math.min(MAX_CONCURRENCY, Number(input.maxConcurrency) || MAX_CONCURRENCY)); job.status = 'scraping'; for (let i = 0; i < input.urls.length && !job.cancelled; i += concurrency) { const batch = input.urls.slice(i, i + concurrency); const pages = await Promise.all(batch.map(u => scrape({ ...input, url: u }).catch(e => { job.errors.push({ url: u, error: e.message }); return null; }))); job.data.push(...pages.filter(Boolean).map(page => { delete page._links; return page; })); job.completed = job.data.length; } job.status = job.cancelled ? 'cancelled' : 'completed'; job.finishedAt = new Date().toISOString(); })().catch(e => { job.status = 'failed'; job.error = e.message; job.finishedAt = new Date().toISOString(); });
    const response = { success: true, id, url: new URL(`/${parts[0] === 'v2' ? 'v2' : 'v1'}/batch/scrape/${id}`, apiBaseUrl(req)).href };
    if (invalidURLs.length) response.invalidURLs = invalidURLs;
    return json(res, 200, response);
  }
  if (['GET', 'DELETE'].includes(req.method) && endpoint[0] === 'crawl' && endpoint[1]) {
    const job = jobs.get(endpoint[1]);
    if (!job) return json(res, 404, { error: 'Crawl job not found.' });
    if (req.method === 'DELETE') { job.cancelled = true; if (job.status === 'queued') job.status = 'cancelled'; return json(res, 200, { status: 'cancelled' }); }
    return json(res, 200, jobStatus(job));
  }
  if (['GET', 'DELETE'].includes(req.method) && endpoint[0] === 'batch' && endpoint[1] === 'scrape' && endpoint[2]) {
    const job = jobs.get(endpoint[2]);
    if (!job) return json(res, 404, { error: 'Batch scrape job not found.' });
    if (req.method === 'DELETE') { job.cancelled = true; if (job.status === 'queued') { job.status = 'cancelled'; job.finishedAt = new Date().toISOString(); } return json(res, 200, { status: 'cancelled' }); }
    return json(res, 200, jobStatus(job));
  }
  return json(res, 404, { success: false, error: 'Not found' });
}

const server = http.createServer((req, res) => {
  const startedAt = Date.now();
  res.once('finish', () => console.log(JSON.stringify({
    timestamp: new Date().toISOString(),
    method: req.method,
    path: new URL(req.url, 'http://localhost').pathname,
    status: res.statusCode,
    durationMs: Date.now() - startedAt,
  })));
  route(req, res).catch(error => json(res, error.status || 500, { success: false, error: error.message || 'Internal server error' }));
});
server.listen(PORT, HOST, () => console.log(`Firepanda listening on http://${HOST}:${PORT}; Lightpanda CDP: ${CDP_URL}`));
for (const signal of ['SIGINT', 'SIGTERM']) process.on(signal, async () => { server.close(); if (browserPromise) { try { const browser = await browserPromise; await browser.disconnect(); } catch {} } process.exit(0); });
