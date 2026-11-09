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
  // Resolve an href/src; only HTTP(S) targets become Markdown links or images.
  const resolve = value => {
    if (value === null) return '';
    try {
      const url = new URL(value, base);
      return url.protocol === 'http:' || url.protocol === 'https:' ? url.href : '';
    } catch { return ''; }
  };
  // Named anchors are plain text; an empty href is a valid link to the base.
  const linkURL = anchor => resolve(anchor.getAttribute('href'));
  const target = url => '(<' + url.replace(/>/g, '%3E') + '>)';
  // Jumps within this page (citation markers, tables of contents) read as
  // plain text: their targets repeat the page URL and cost many tokens.
  const page = location.href.split('#')[0];
  const samePage = url => url.includes('#') && url.split('#')[0] === page;
  // markdownLinks: "full" keeps absolute targets, "relative" shortens
  // same-origin ones to their path, "none" keeps only link text / alt text.
  const pageOrigin = new URL(location.href).origin;
  const shown = url => options.markdownLinks === 'relative' && url.startsWith(pageOrigin + '/') ?
    url.slice(pageOrigin.length) : url;
  const ticks = (text, minimum) => {
    let length = minimum;
    for (const run of text.matchAll(/`+/g)) length = Math.max(length, run[0].length + 1);
    return '`'.repeat(length);
  };
  // Text that would read as Markdown structure at the start of a line ("# x",
  // "- x", "1. x", "> x", "---") gets a sentinel; `compact` turns it into a
  // backslash only where the text really starts a line and drops it elsewhere.
  const MARK = '';
  const structural = /^(?:[#>+-]|\d+[.)])(?:\s|$)|^[-=]{2,}\s*$/;
  const blockTags = new Set(['p','div','section','article','main','tr']);
  const indent = (text, width) => text.split('\n').map((line, i) => i && line ? ' '.repeat(width) + line : line).join('\n');
  const children = n => {
    let text = '';
    for (const child of n.childNodes) text += render(child);
    return text;
  };
  const tagOf = n => n.nodeType === 1 ? n.tagName.toLowerCase() : '';
  const list = (n, ordered) => {
    const start = parseInt(n.getAttribute('start'), 10);
    let number = Number.isNaN(start) ? 1 : start, text = '';
    for (const child of n.childNodes) {
      if (tagOf(child) !== 'li') { text += render(child); continue; }
      const marker = ordered ? (number++) + '. ' : '- ';
      // Continuation lines (nested lists, extra paragraphs) align under the item text.
      text += '\n' + marker + indent(children(child).trim(), marker.length);
    }
    return '\n\n' + text.trim() + '\n\n';
  };
  const table = n => {
    const rows = Array.from(n.querySelectorAll('tr')).filter(row => row.closest('table') === n);
    if (!rows.length) return '\n\n' + children(n).trim() + '\n\n';
    const cell = c => children(c).replace(/\s+/g, ' ').trim().replace(/\|/g, '\\|');
    const grid = rows.map(row => Array.from(row.children).filter(c => ['td', 'th'].includes(tagOf(c))).map(cell));
    const width = grid.reduce((widest, row) => Math.max(widest, row.length), 1);
    const line = cells => '| ' + Array.from({length: width}, (_, i) => cells[i] ?? '').join(' | ') + ' |';
    // GFM needs a header row; the first row serves even when it uses <td>.
    return '\n\n' + [line(grid[0]), line(Array(width).fill('---')), ...grid.slice(1).map(line)].join('\n') + '\n\n';
  };
  const render = n => {
    if (n.nodeType === 3) {
      const text = escape((n.textContent || '').replaceAll(MARK, '').replace(/\s+/g, ' '));
      const lead = text.length - text.trimStart().length;
      return structural.test(text.slice(lead)) ? text.slice(0, lead) + MARK + text.slice(lead) : text;
    }
    if (n.nodeType !== 1) return '';
    const tag = tagOf(n);
    const body = () => children(n);
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
      if (!url || samePage(url) || options.markdownLinks === 'none') return body();
      return '[' + body().trim() + ']' + target(shown(url));
    }
    if (tag === 'img') {
      const alt = escape((n.getAttribute('alt') || '').replace(/\s+/g, ' ').trim());
      const src = resolve(n.getAttribute('src'));
      // data: and other non-HTTP sources keep only their alt text.
      return src && options.markdownLinks !== 'none' ? '![' + alt + ']' + target(shown(src)) : alt;
    }
    if (tag === 'strong' || tag === 'b' || tag === 'em' || tag === 'i') {
      const text = body();
      const mark = tag === 'strong' || tag === 'b' ? '**' : '*';
      return text.trim() ? mark + text + mark : text;
    }
    if (tag === 'ul' || tag === 'ol') return list(n, tag === 'ol');
    if (tag === 'li') return '\n- ' + indent(body().trim(), 2);
    if (tag === 'table') return table(n);
    if (tag === 'blockquote') {
      return '\n\n' + body().trim().split('\n').map(line => line ? '> ' + line : '>').join('\n') + '\n\n';
    }
    if (tag === 'br') return '\n';
    if (tag === 'hr') return '\n\n---\n\n';
    if (blockTags.has(tag)) return '\n\n' + body().trim() + '\n\n';
    if (tag === 'td' || tag === 'th') return body() + ' | ';
    return body();
  };
  const data = {metadata: {title: document.title || '', url: location.href}};
  const description = (document.querySelector('meta[name="description"]') || {}).content;
  if (description) data.metadata.description = description;
  // Plain text without source indentation or blank-line runs; <pre> blocks
  // (including plain-text documents) keep their whitespace verbatim.
  const plainText = node => {
    const copy = node.cloneNode(true), verbatim = [];
    copy.querySelectorAll('pre').forEach(pre => {
      verbatim.push(pre.textContent);
      pre.textContent = '\uE001' + (verbatim.length - 1) + '\uE001';
    });
    return (copy.textContent || '').split('\n').map(line => line.replace(/\s+/g, ' ').trim())
      .join('\n').replace(/\n{3,}/g, '\n\n')
      .replace(/\uE001(\d+)\uE001/g, (_, index) => verbatim[index]).trim();
  };
  const compact = markdown => {
    let fence = 0, blank = false;
    // Compact prose without collapsing meaningful blank lines inside fenced code.
    return markdown.split('\n').filter(line => {
      // Fences nested in list items or quotes carry their indent/quote prefix.
      const delimiter = line.match(/^(?:\s*>)*\s*(`{3,})$/);
      if (delimiter && (!fence || delimiter[1].length === fence)) {
        fence = fence ? 0 : delimiter[1].length;
      }
      // A line holding only quote markers is a blank line inside a blockquote.
      if (fence || !/^(?:\s*>)*\s*$/.test(line)) { blank = false; return true; }
      if (blank) return false;
      blank = true;
      return true;
    }).map(line => line.includes(MARK) ? line
      // A sentinel after only quote/list markers starts a line: escape it there.
      .replace(/^((?:\s*>)*\s*(?:(?:[-+]|\d+[.)])\s+)?)\uE000(\d*)/, (_, prefix, digits) => prefix + digits + '\\')
      .replaceAll(MARK, '') : line).join('\n').trim();
  };
  // Cleaned `html` keeps only attributes that carry meaning for a reader;
  // classes, styles, data-*, ids, srcset, and event handlers are layout
  // noise. Inline data: sources and comments go too. `rawHtml` is untouched.
  const meaningful = new Set(['href', 'src', 'alt', 'title', 'colspan', 'rowspan', 'start', 'datetime',
    // Form controls and accessible names: what a field is for and holds.
    'aria-label', 'name', 'placeholder', 'type', 'value']);
  const lean = node => {
    for (const element of node.querySelectorAll('*')) {
      for (const {name, value} of Array.from(element.attributes)) {
        if (!meaningful.has(name) || (name === 'src' && value.startsWith('data:'))) element.removeAttribute(name);
      }
    }
    const comments = [], walker = document.createTreeWalker(node, 128);
    while (walker.nextNode()) comments.push(walker.currentNode);
    comments.forEach(comment => comment.remove());
  };
  for (const format of options.formats) {
    if (format === 'markdown') data.markdown = compact(render(content));
    if (format === 'text') data.text = plainText(content);
    if (format === 'html') { lean(content); data.html = content.innerHTML; }
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
        // Card-style anchors wrap whole blocks; a title is a label, not content.
        data.links.push({url, title:(a.textContent || '').replace(/\s+/g, ' ').trim().slice(0, 256)});
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
