// Runs once in the page: project content before serializing it over CDP.
(async options => {
  // Read DOM text without matching source-code strings embedded in scripts.
  function* textNodes() {
    const walker = document.createTreeWalker(document.body || document.documentElement, 4);
    let node;
    while ((node = walker.nextNode())) {
      if (!node.parentElement?.closest('script,style,noscript,template')) yield node.textContent || '';
    }
  }
  const hasText = needle => {
    let tail = '';
    for (const text of textNodes()) {
      const joined = tail + text;
      if (joined.includes(needle)) return true;
      tail = needle.length > 1 ? joined.slice(1 - needle.length) : '';
    }
    return false;
  };
  if (options.waitForSelector || options.waitForText) {
    let textDirty = true, textReady = false;
    // Watch the document root so replacing <body> also invalidates the cache.
    // Selectors still poll: property changes such as :checked need not mutate attributes.
    const observer = options.waitForText && typeof MutationObserver === 'function' ?
      new MutationObserver(() => { textDirty = true; }) : null;
    const ready = () => {
      if (options.waitForSelector && !document.querySelector(options.waitForSelector)) return false;
      if (!options.waitForText) return true;
      if (!observer || textDirty) {
        textReady = hasText(options.waitForText);
        textDirty = false;
      }
      return textReady;
    };
    // Validate selector syntax before scheduling timers. Both conditions must hold together.
    if (options.waitForSelector) document.querySelector(options.waitForSelector);
    const end = Date.now() + options.readyTimeout;
    try {
      observer?.observe(document.documentElement, {subtree:true, childList:true, characterData:true});
      // Poll in the page, keeping this wait and extraction to one CDP round trip.
      while (!ready()) {
        if (Date.now() >= end) throw new Error('Content readiness timed out');
        await new Promise(resolve => setTimeout(resolve, Math.min(50, Math.max(1, end - Date.now()))));
      }
    } finally {
      observer?.disconnect();
    }
  }
  let content;
  const projected = options.formats.filter(f => ['markdown', 'text', 'html'].includes(f));
  // Map and rawHtml-only requests do not need a second DOM tree at all.
  if (projected.length) {
    const root = document.documentElement.cloneNode(true);
    root.querySelectorAll('script,style,noscript,template,svg').forEach(e => e.remove());
    content = root.querySelector('body') || root;
    if (options.onlyMainContent) {
      // SPAs keep inactive views in the DOM; a hidden <main> must not displace visible content.
      root.querySelectorAll('[hidden],[aria-hidden="true"],[style]').forEach(e => {
        const style = e.style;
        if (e.hasAttribute('hidden') || e.getAttribute('aria-hidden') === 'true' ||
            style?.display === 'none' || style?.visibility === 'hidden') e.remove();
      });
      root.querySelectorAll('nav,footer,aside').forEach(e => e.remove());
      // Article/section headers contain headlines and bylines; only discard page chrome.
      root.querySelectorAll('header').forEach(e => {
        if (!e.closest('main,article,section,[role="main"]')) e.remove();
      });
      const main = root.querySelector('main,[role="main"]');
      // A list of articles is a listing page, not permission to drop all but its first item.
      const articles = root.querySelectorAll('article');
      content = main || (articles.length === 1 && articles[0]) || content;
    }
  }
  // A deliberately small DOM-to-Markdown renderer for common article content.
  const escape = s => s.replace(/([\\`*_[\]])/g, '\\$1');
  const base = document.baseURI || location.href;
  const linkURL = anchor => {
    const href = anchor.getAttribute('href');
    // Named anchors are plain text; an empty href is a valid link to the base.
    if (href === null) return '';
    try {
      const url = new URL(href, base);
      return url.protocol === 'http:' || url.protocol === 'https:' ? url.href : '';
    } catch { return ''; }
  };
  const ticks = (text, minimum) => {
    let length = minimum;
    for (const run of text.matchAll(/`+/g)) length = Math.max(length, run[0].length + 1);
    return '`'.repeat(length);
  };
  const blockTags = new Set(['p','div','section','article','main','ul','ol','blockquote','table','tr']);
  const render = n => {
    if (n.nodeType === 3) return escape((n.textContent || '').replace(/\s+/g, ' '));
    if (n.nodeType !== 1) return '';
    const tag = n.tagName.toLowerCase();
    const body = () => {
      let text = '';
      for (const child of n.childNodes) text += render(child);
      return text;
    };
    if (tag === 'pre') {
      const text = n.textContent.replace(/\r\n?/g, '\n');
      // Choose a fence longer than any literal backtick run; never alter code to fit it.
      const fence = ticks(text, 3);
      return '\n\n' + fence + '\n' + text + (text.endsWith('\n') ? '' : '\n') + fence + '\n\n';
    }
    if (tag === 'code') {
      const text = n.textContent.replace(/\r\n?|\n/g, ' ');
      const fence = ticks(text, 1);
      const pad = text.startsWith('`') || text.endsWith('`') ||
        (text.startsWith(' ') && text.endsWith(' ') && !/^ +$/.test(text)) ? ' ' : '';
      return fence + pad + text + pad + fence;
    }
    if (/^h[1-6]$/.test(tag)) return '\n\n' + '#'.repeat(Number(tag[1])) + ' ' + body().trim() + '\n\n';
    if (tag === 'a') {
      const url = linkURL(n);
      return url ? '[' + body().trim() + '](<' + url.replace(/>/g, '%3E') + '>)' : body();
    }
    if (tag === 'strong' || tag === 'b') return '**' + body() + '**';
    if (tag === 'em' || tag === 'i') return '*' + body() + '*';
    if (tag === 'li') return '\n- ' + body().trim();
    if (tag === 'br') return '\n';
    if (tag === 'hr') return '\n\n---\n\n';
    if (blockTags.has(tag)) return '\n\n' + body().trim() + '\n\n';
    if (tag === 'td' || tag === 'th') return body() + ' | ';
    return body();
  };
  const data = {metadata: {
    title: document.title || '', url: location.href,
    description: (document.querySelector('meta[name="description"]') || {}).content || ''
  }};
  const compact = markdown => {
    let fence = 0, blank = false;
    // Compact prose without collapsing meaningful blank lines inside fenced code.
    return markdown.split('\n').filter(line => {
      const delimiter = line.match(/^(`{3,})$/);
      if (delimiter && (!fence || delimiter[1].length === fence)) {
        fence = fence ? 0 : delimiter[1].length;
      }
      if (fence || line.trim()) { blank = false; return true; }
      if (blank) return false;
      blank = true;
      return true;
    }).join('\n').trim();
  };
  for (const format of options.formats) {
    if (format === 'markdown') data.markdown = compact(render(content));
    if (format === 'text') data.text = (content.textContent || '').trim();
    if (format === 'html') data.html = content.innerHTML;
    if (format === 'rawHtml') data.rawHtml = document.documentElement.outerHTML;
    if (format === 'links') {
      const seen = new Set();
      data.links = [];
      // Stop before allocating records for every anchor on an unusually large page.
      for (const a of document.querySelectorAll('a[href]')) {
        // Discovery deduplicates documents; Markdown retains in-page fragments.
        const url = linkURL(a).split('#')[0];
        if (!url || seen.has(url)) continue;
        // Exactly 10,000 links is complete; flag truncation only when another is dropped.
        if (data.links.length === options.maxLinks) { data.linksTruncated = true; break; }
        seen.add(url);
        data.links.push({url, title:(a.textContent || '').trim()});
      }
    }
  }
  // Heuristics are warnings, not proof that access failed. Never discard the page.
  let sample = '';
  for (const text of textNodes()) {
    sample += text.slice(0, 8192 - sample.length);
    if (sample.length === 8192) break;
  }
  sample = sample.replace(/\s+/g, ' ').trim().toLowerCase();
  const title = (document.title || '').trim().toLowerCase();
  const warnings = [];
  const warn = (code, message) => warnings.push({code, message});
  const short = sample.length < 1000;
  const challengeFrame = short && Array.from(document.querySelectorAll('iframe[src]')).some(frame => {
    try {
      const host = new URL(frame.getAttribute('src'), document.baseURI || location.href).hostname;
      return host === 'geo.captcha-delivery.com' || host === 'challenges.cloudflare.com';
    } catch { return false; }
  });
  if (/^(just a moment(?:\.{3}|…)?|checking your browser(?:\s*[-–].*)?|client challenge|vercel security checkpoint|attention required.*)$/.test(title) ||
      (short && /performing security verification|checking your browser before accessing|we're verifying your browser/.test(sample)) || challengeFrame) {
    warn('suspected_block', 'The page appears to contain a security challenge instead of the requested content.');
  }
  if (/^(?:error\s+)?[45]\d\d\b/.test(title) || /^error page(?:\s*[|:–-]|$)/.test(title) ||
      (short && /^(?:error\s+)?[45]\d\d\b/.test(sample))) {
    warn('suspected_error_page', 'The page appears to be an error document; this is a content heuristic.');
  }
  if ((projected.length && projected.every(f => !data[f].trim())) ||
      (!projected.length && options.formats.includes('links') && !data.links.length)) {
    warn('empty_content', 'The requested content is empty; a readiness condition may be needed.');
  }
  if (warnings.length) data.warnings = warnings;
  return data;
})
