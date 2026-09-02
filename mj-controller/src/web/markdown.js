// Markdown for agent-controlled text, rendered as DOM nodes.
//
// Everything on this page that an agent can influence goes through here, and
// nothing here ever produces markup as a string. There is no `innerHTML`, no
// `insertAdjacentHTML`, and no attribute built by concatenation: text becomes
// text nodes and structure becomes elements. That is the whole security
// argument, and it is why the renderer returns a DocumentFragment rather than
// a string a caller could be tempted to assign somewhere.

import { codeBlock } from './tool-output.js';

/// Schemes a link may use. A link is a navigation the reader did not choose,
/// so `javascript:` and `data:` are refused outright rather than sanitised,
/// and anything unrecognised is refused with them.
const SAFE_SCHEME = /^(https?:\/\/|mailto:)/i;

/// Characters that are invisible in a rendered link and meaningful to a URL
/// parser. `java\tscript:` is one navigation to a browser and a different
/// string to a naive scheme test, so they come out before the test.
const INVISIBLE = new RegExp('[\\u0000-\\u0020\\u007f]', 'g');

/// The href a link may carry, or null when it may not be a link at all.
///
/// The cleaned value is the one used, so a refused character cannot survive
/// into the attribute by being stripped for the test and restored for the DOM.
export function safeHref(raw) {
  const cleaned = String(raw ?? '').replace(INVISIBLE, '');
  return SAFE_SCHEME.test(cleaned) ? cleaned : null;
}

function text(value) {
  return document.createTextNode(value);
}

function element(name, className) {
  const node = document.createElement(name);
  if (className) node.className = className;
  return node;
}

// ---------------------------------------------------------------------------
// Inline markup
// ---------------------------------------------------------------------------

/// Length of the run of `character` starting at `index`.
function runLength(source, index, character) {
  let run = 0;
  while (source[index + run] === character) run += 1;
  return run;
}

/// Parse `[label](target)` starting at `index`, or null.
///
/// The label is scanned with a depth counter, so a label containing brackets —
/// which agents produce constantly when quoting code — does not end the link
/// early.
function parseLink(source, index) {
  let depth = 0;
  let cursor = index;
  for (; cursor < source.length; cursor += 1) {
    if (source[cursor] === '[') depth += 1;
    else if (source[cursor] === ']') {
      depth -= 1;
      if (depth === 0) break;
    }
  }
  if (depth !== 0 || source[cursor + 1] !== '(') return null;
  const close = source.indexOf(')', cursor + 2);
  if (close === -1) return null;
  return {
    label: source.slice(index + 1, cursor),
    target: source.slice(cursor + 2, close),
    end: close + 1,
  };
}

const EMPHASIS = [
  ['**', 'strong'],
  ['__', 'strong'],
  ['~~', 'del'],
  ['*', 'em'],
  ['_', 'em'],
];

/// What the markup at `index` is, or null when the character there is
/// ordinary text.
///
/// Returning an `apply` rather than a node lets a refused link contribute its
/// label to the parent without becoming an element of its own.
function readInline(source, index) {
  const character = source[index];

  // Code first, so a backticked `*star*` stays a star.
  if (character === '`') {
    const run = runLength(source, index, '`');
    const marker = '`'.repeat(run);
    const close = source.indexOf(marker, index + run);
    if (close !== -1) {
      const body = source.slice(index + run, close);
      return {
        end: close + run,
        apply(parent) {
          const code = element('code');
          // One padding space on each side is Markdown's way of writing a
          // backtick inside a code span, and is not part of the code.
          code.textContent = body.replace(/^ (.*) $/s, '$1');
          parent.append(code);
        },
      };
    }
  }

  if (character === '[') {
    const link = parseLink(source, index);
    if (link) {
      const href = safeHref(link.target);
      return {
        end: link.end,
        apply(parent) {
          if (!href) {
            // A refused scheme still had something to say. Keep the words and
            // drop only the navigation.
            renderInline(parent, link.label);
            return;
          }
          const anchor = element('a');
          anchor.setAttribute('href', href);
          anchor.setAttribute('rel', 'noreferrer noopener');
          anchor.setAttribute('target', '_blank');
          renderInline(anchor, link.label);
          parent.append(anchor);
        },
      };
    }
  }

  for (const [marker, tag] of EMPHASIS) {
    if (!source.startsWith(marker, index)) continue;
    const from = index + marker.length;
    const close = source.indexOf(marker, from);
    if (close === -1 || close === from) continue;
    const body = source.slice(from, close);
    return {
      end: close + marker.length,
      apply(parent) {
        const node = element(tag);
        renderInline(node, body);
        parent.append(node);
      },
    };
  }

  return null;
}

/// Render inline markup into `parent`.
///
/// A delimiter that never closes is not markup: it is emitted as the literal
/// character the writer typed, which is what a half-finished streaming message
/// looks like most of the time.
export function renderInline(parent, source) {
  let buffer = '';
  const flush = () => {
    if (!buffer) return;
    parent.append(text(buffer));
    buffer = '';
  };
  let index = 0;
  while (index < source.length) {
    const markup = readInline(source, index);
    if (markup) {
      flush();
      markup.apply(parent);
      index = markup.end;
      continue;
    }
    buffer += source[index];
    index += 1;
  }
  flush();
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

const FENCE = /^\s{0,3}(`{3,}|~{3,})\s*(\S*)\s*$/;
const HEADING = /^\s{0,3}(#{1,6})\s+(.*?)\s*#*\s*$/;
const RULE = /^\s{0,3}([-*_])(\s*\1){2,}\s*$/;
const QUOTE = /^\s{0,3}>\s?(.*)$/;
const BULLET = /^(\s*)([-*+])\s+(.*)$/;
const ORDERED = /^(\s*)(\d{1,9})[.)]\s+(.*)$/;
const TABLE_DIVIDER = /^\s*:?-{1,}:?\s*$/;

/// Split a table row into cells, tolerating the optional outer pipes.
function tableCells(line) {
  const trimmed = line.trim().replace(/^\|/, '').replace(/\|$/, '');
  return trimmed.split('|').map(cell => cell.trim());
}

function isTableDivider(line) {
  const cells = tableCells(line);
  return cells.length > 0 && cells.every(cell => TABLE_DIVIDER.test(cell));
}

/// The class that carries a table column's alignment.
///
/// Alignment is a class rather than a style attribute because the page's
/// content-security policy forbids inline style, and a style attribute is
/// exactly what it forbids.
function alignmentClass(cell) {
  const start = cell.startsWith(':');
  const end = cell.endsWith(':');
  if (start && end) return 'align-center';
  if (end) return 'align-right';
  if (start) return 'align-left';
  return '';
}

function indentWidth(indent) {
  return indent.replace(/\t/g, '  ').length;
}

/// One list, however deeply nested, starting at `start`.
///
/// Returns the list element and the line after it. Nesting is by indentation:
/// a deeper item belongs to the item above it, which is how every agent that
/// writes Markdown produces nested lists.
function readList(lines, start) {
  const first = BULLET.exec(lines[start]) || ORDERED.exec(lines[start]);
  const ordered = !BULLET.exec(lines[start]);
  const baseIndent = indentWidth(first[1]);
  const list = element(ordered ? 'ol' : 'ul');
  let index = start;
  let item = null;
  let pending = [];

  const closeItem = () => {
    if (!item) return;
    if (pending.length) {
      const nested = renderBlocks(pending);
      item.append(nested);
      pending = [];
    }
    item = null;
  };

  while (index < lines.length) {
    const line = lines[index];
    if (!line.trim()) {
      // A blank line ends the list unless the next line continues it.
      const next = lines[index + 1] ?? '';
      const continues = BULLET.exec(next) || ORDERED.exec(next);
      if (!continues || indentWidth(continues[1] ?? '') < baseIndent) break;
      index += 1;
      continue;
    }
    const match = BULLET.exec(line) || ORDERED.exec(line);
    if (match && indentWidth(match[1]) <= baseIndent) {
      closeItem();
      item = element('li');
      renderInline(item, match[3]);
      list.append(item);
      index += 1;
      continue;
    }
    if (!item) break;
    if (match) {
      // A deeper item, or a continuation line, belongs to the open item and is
      // rendered by a nested pass once the item ends.
      pending.push(
        line.slice(
          Math.min(indentWidth(match[1]), line.length - line.trimStart().length + baseIndent + 2),
        ),
      );
      index += 1;
      continue;
    }
    if (indentWidth(line.match(/^\s*/)[0]) > baseIndent) {
      pending.push(line.trimStart());
      index += 1;
      continue;
    }
    break;
  }
  closeItem();
  return { node: list, end: index };
}

/// Render a block sequence into a DocumentFragment.
function renderBlocks(lines) {
  const fragment = document.createDocumentFragment();
  let index = 0;
  while (index < lines.length) {
    const line = lines[index];

    if (!line.trim()) {
      index += 1;
      continue;
    }

    const fence = FENCE.exec(line);
    if (fence) {
      const closing = fence[1][0].repeat(3);
      const body = [];
      index += 1;
      while (index < lines.length && !lines[index].trimStart().startsWith(closing)) {
        body.push(lines[index]);
        index += 1;
      }
      // A fence that never closes still has content worth showing.
      if (index < lines.length) index += 1;
      // Fenced code is tinted, folded when long, and pretty-printed when it
      // turns out to be JSON, by the same layer that renders tool output.
      fragment.append(codeBlock(body.join('\n'), fence[2].toLowerCase().replace(/[^\w.+-]/g, '')));
      continue;
    }

    const rule = RULE.exec(line);
    if (rule) {
      fragment.append(element('hr'));
      index += 1;
      continue;
    }

    const heading = HEADING.exec(line);
    if (heading) {
      const node = element(`h${heading[1].length}`);
      renderInline(node, heading[2]);
      fragment.append(node);
      index += 1;
      continue;
    }

    const quote = QUOTE.exec(line);
    if (quote) {
      const body = [];
      while (index < lines.length) {
        const inner = QUOTE.exec(lines[index]);
        if (!inner) break;
        body.push(inner[1]);
        index += 1;
      }
      const node = element('blockquote');
      node.append(renderBlocks(body));
      fragment.append(node);
      continue;
    }

    if (
      line.includes('|') &&
      index + 1 < lines.length &&
      lines[index + 1].includes('-') &&
      isTableDivider(lines[index + 1])
    ) {
      const alignments = tableCells(lines[index + 1]).map(alignmentClass);
      const table = element('table');
      const head = element('thead');
      const headRow = element('tr');
      tableCells(line).forEach((cell, column) => {
        const node = element('th', alignments[column]);
        renderInline(node, cell);
        headRow.append(node);
      });
      head.append(headRow);
      table.append(head);
      const body = element('tbody');
      index += 2;
      while (index < lines.length && lines[index].includes('|') && lines[index].trim()) {
        const row = element('tr');
        tableCells(lines[index]).forEach((cell, column) => {
          const node = element('td', alignments[column]);
          renderInline(node, cell);
          row.append(node);
        });
        body.append(row);
        index += 1;
      }
      table.append(body);
      // A table can be wider than a phone. It scrolls inside its own box so
      // the page never scrolls sideways.
      const scroller = element('div', 'scroll-x');
      scroller.append(table);
      fragment.append(scroller);
      continue;
    }

    if (BULLET.test(line) || ORDERED.test(line)) {
      const list = readList(lines, index);
      fragment.append(list.node);
      index = list.end;
      continue;
    }

    const paragraph = [];
    while (index < lines.length && lines[index].trim()) {
      const next = lines[index];
      if (
        FENCE.test(next) ||
        HEADING.test(next) ||
        RULE.test(next) ||
        QUOTE.test(next) ||
        BULLET.test(next) ||
        ORDERED.test(next)
      ) {
        break;
      }
      paragraph.push(next.trim());
      index += 1;
    }
    if (paragraph.length) {
      const node = element('p');
      renderInline(node, paragraph.join('\n'));
      fragment.append(node);
      continue;
    }
    // Nothing matched and nothing was consumed: emit the line as text rather
    // than looping forever on it.
    const node = element('p');
    renderInline(node, line.trim());
    fragment.append(node);
    index += 1;
  }
  return fragment;
}

/// Render Markdown source as DOM nodes.
export function renderMarkdown(source) {
  return renderBlocks(
    String(source ?? '')
      .replace(/\r\n?/g, '\n')
      .split('\n'),
  );
}

/// A tool call's diff summaries, as their own rows.
///
/// These arrive from `format_diffstat` in `src/hel_chat/transcript.rs`, which
/// writes the path, two spaces, `+{insertions}`, a space, and `-{deletions}`
/// using a Unicode MINUS SIGN at U+2212 rather than a hyphen. They are not
/// Markdown — a table renderer would mangle them — and the counts are worth
/// styling apart from the path, so they get their own shape.
///
/// The split is deliberately forgiving. A line that does not match the shape
/// still renders as a path, because a diffstat the browser cannot parse is
/// still a file the reader wants to see named.
const DIFFSTAT = /^(.*?)\s\s+\+(\d+)\s[+\u2212-](\d+)\s*$/;

export function renderDiffSummary(lines) {
  const list = element('ul', 'diffstat');
  for (const line of lines) {
    const item = element('li');
    const match = DIFFSTAT.exec(line);
    const name = element('span', 'diffstat-path');
    name.textContent = match ? match[1] : line.trim();
    item.append(name);
    if (match) {
      const added = element('span', 'diffstat-added');
      added.textContent = `+${match[2]}`;
      const removed = element('span', 'diffstat-removed');
      removed.textContent = `\u2212${match[3]}`;
      item.append(added, removed);
    }
    list.append(item);
  }
  return list;
}
