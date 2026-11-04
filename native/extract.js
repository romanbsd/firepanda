// Runs once in the page: project content before serializing it over CDP.
(options => {
  let content;
  // Map and rawHtml-only requests do not need a second DOM tree at all.
  if (options.formats.some(f => ['markdown', 'text', 'html'].includes(f))) {
    const root = document.documentElement.cloneNode(true);
    root.querySelectorAll('script,style,noscript,template,svg').forEach(e => e.remove());
    if (options.onlyMainContent) {
      root.querySelectorAll('nav,footer,header,aside').forEach(e => e.remove());
    }
    content = (options.onlyMainContent && root.querySelector('main,article,[role="main"]')) || root.querySelector('body') || root;
  }
  // A deliberately small DOM-to-Markdown renderer for common article content.
  const escape = s => s.replace(/([\\`*_[\]])/g, '\\$1');
  const ticks = (text, minimum) => {
    let length = minimum;
    for (const run of text.matchAll(/`+/g)) length = Math.max(length, run[0].length + 1);
    return '`'.repeat(length);
  };
  const render = n => {
    if (n.nodeType === 3) return escape((n.textContent || '').replace(/\s+/g, ' '));
    if (n.nodeType !== 1) return '';
    const tag = n.tagName.toLowerCase();
    const body = () => Array.from(n.childNodes).map(render).join('');
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
      const href = n.getAttribute('href');
      let url = ''; try { url = new URL(href, document.baseURI || location.href).href; } catch {}
      return /^https?:/.test(url) ? '[' + body().trim() + '](<' + url.replace(/>/g, '%3E') + '>)' : body();
    }
    if (tag === 'strong' || tag === 'b') return '**' + body() + '**';
    if (tag === 'em' || tag === 'i') return '*' + body() + '*';
    if (tag === 'li') return '\n- ' + body().trim();
    if (tag === 'br') return '\n';
    if (tag === 'hr') return '\n\n---\n\n';
    if (['p','div','section','article','main','ul','ol','blockquote','table','tr'].includes(tag)) return '\n\n' + body().trim() + '\n\n';
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
        const url = a.href.split('#')[0];
        if (!/^https?:\/\//.test(url) || seen.has(url)) continue;
        // Exactly 10,000 links is complete; flag truncation only when another is dropped.
        if (data.links.length === 10000) { data.linksTruncated = true; break; }
        seen.add(url);
        data.links.push({url, title:(a.textContent || '').trim()});
      }
    }
  }
  return data;
})
