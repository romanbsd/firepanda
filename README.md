# Firepanda

A small Firecrawl-compatible HTTP API that renders pages with [Lightpanda](https://lightpanda.io/) over Chrome DevTools Protocol.

## Run it

With Docker Compose:

```sh
docker compose up --build
```

Set `API_KEY` in the environment to require `Authorization: Bearer <key>`. For local development, run Lightpanda separately and set its CDP endpoint:

```sh
lightpanda serve --host 127.0.0.1 --port 9222
corepack pnpm install
LIGHTPANDA_CDP_URL=http://127.0.0.1:9222 pnpm start
```

The API listens on port `4000` by default. `PORT`, `HOST`, `LIGHTPANDA_CDP_URL`, `API_KEY`, `MAX_BODY_BYTES`, `MAX_CRAWL_PAGES`, `MAX_CONCURRENCY`, and `JOB_TTL_MS` can be configured with environment variables. The default CDP endpoint is `http://127.0.0.1:9222`; completed in-memory jobs expire after 24 hours by default.

Requests are logged as JSON to stdout (timestamp, method, path, status, and duration). Follow the API logs with `docker compose logs -f firepanda`, or both API and Lightpanda logs with `docker compose logs -f`.

## Routes

Both `/v1` and `/v2` prefixes are accepted.

| Method | Path | Behavior |
| --- | --- | --- |
| POST | `/v1/scrape` | Scrape one URL and return Firecrawl-style `success` and `data` |
| POST | `/v1/crawl` | Start an in-memory, asynchronous same-origin crawl |
| GET, DELETE | `/v1/crawl/:id` | Read status/results or cancel the crawl |
| POST | `/v1/batch/scrape` | Start an in-memory batch scrape |
| GET, DELETE | `/v1/batch/scrape/:id` | Read batch status/results or cancel the job |
| POST | `/v1/map` | Return discovered same-host links |
| GET | `/health`, `/healthz` | Liveness response |

Example scrape:

```sh
curl http://localhost:4000/v1/scrape \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","formats":["markdown","html","links"]}'
```

Example crawl:

```sh
curl http://localhost:4000/v1/crawl \
  -H 'Content-Type: application/json' \
  -d '{"url":"https://example.com","limit":10,"scrapeOptions":{"formats":["markdown"]}}'
```

The implemented response envelopes follow Firecrawl v2: scrape returns `{ "success": true, "data": { ...formats, "metadata": { ... } } }`; map returns `{ "success": true, "links": [{ "url", "title", "description" }] }`; crawl and batch creation return `{ "success": true, "id", "url" }`; job status returns `status`, `total`, `completed`, `creditsUsed`, timestamps, `duration`, and a `data` array. Supported scrape request options are `url`, `formats`, `onlyMainContent`, `includeTags`, `excludeTags`, `headers`, `mobile`, `waitFor`, and `timeout`. Crawl also accepts `includePaths`, `excludePaths`, `limit`, `maxDiscoveryDepth` (and the v1 `maxDepth` alias), `maxConcurrency`, and `scrapeOptions`. Batch accepts `urls`, `formats`/scrape options, `maxConcurrency`, and `ignoreInvalidURLs`.

Jobs and results live in process memory and are lost on restart. Crawl depth, page limit, and concurrency are capped by configuration. Scrape supports `markdown`, `html`, `rawHtml`, and `links`; headers, `includeTags`, `excludeTags`, `onlyMainContent`, `mobile`, `waitFor`, and `timeout` are supported. Crawls stay on the starting hostname and respect `includePaths`/`excludePaths`.

## Compatibility limits

This is a lightweight replacement for the common scraping workflow, not a full Firecrawl clone. Search, LLM-backed JSON extraction, screenshots, PDF parsing, actions, webhooks, persistent job storage, and Firecrawl-specific billing/usage endpoints are not implemented. Unsupported output formats return HTTP 400. Lightpanda does not render pixels, so screenshot output is intentionally unavailable. Some sites depend on browser APIs Lightpanda does not yet implement; use a full Chromium browser for those pages.

The service has no authentication unless `API_KEY` is set. Keep it on a trusted network or configure a key before exposing it. Treat target URLs as untrusted input: deploy with network egress controls appropriate to your environment.
