// Rendering for the text an agent's tools produce, as opposed to the prose an
// agent writes.
//
// Ported from the mjolnir viewer (`mj-remote/src/remote_viewer.html`, the
// "markdown-lite rendering" and "code tinting" sections), which is the same
// project under the same licence. The port is deliberate rather than
// inspirational: this layer had been through real phone use, and what it knows
// is the awkward part — that a tool result arrives as a bare unfenced dump with
// no filename in sight, that some of those dumps are five thousand lines long,
// and that a shell command reads far better when its program, flags and paths
// are told apart.
//
// The same rule as everywhere else on this page: nodes and `textContent`, never
// markup as a string.
//
// What is not ported is Mjolnir's diff *computation*. It receives the old and
// new text of every edit and diffs them in the browser; Mjolnir publishes only a
// counted summary, so those routines would have nothing to run on. A diff that
// arrives as text is still tinted, just not recomputed.

const FOLD_LINE_LIMIT = 24;
const CODE_TINT_CHAR_LIMIT = 120000;
const DETECT_SAMPLE_CHARS = 2500;
const DETECT_MIN_SCORE = 5;

function el(name, className) {
  const node = document.createElement(name);
  if (className) node.className = className;
  return node;
}

function token(parent, text, className) {
  if (!text) return;
  if (!className) {
    parent.append(document.createTextNode(text));
    return;
  }
  const span = el('span', className);
  span.textContent = text;
  parent.append(span);
}

function countLines(text) {
  let lines = 1;
  for (let index = 0; index < text.length; index += 1) {
    if (text[index] === '\n') lines += 1;
  }
  return lines;
}

// ---------------------------------------------------------------------------
// Shell commands
// ---------------------------------------------------------------------------

/// Whether a token reads as a path.
///
/// Something with a slash only reads as a path when it carries another path
/// signal — a leading marker, more than one slash, or an extension — which
/// keeps prose like "and/or" plain while still catching `src/**/*.rs` and
/// `Cargo.toml`.
export function isPathLike(value) {
  return (
    /^[~./]/.test(value) || value.split('/').length > 2 || /\.\w+$/.test(value.replace(/\/$/, ''))
  );
}

const SHELL_TOKEN =
  /(\s+|&&|\|\||[|;()<>]+|"(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|`(?:[^`\\]|\\.)*`|[^\s|;&()<>]+)/g;

function shellTokens(command) {
  return command.match(SHELL_TOKEN) || [];
}

const IS_WHITESPACE = /^\s+$/;
const IS_OPERATOR = /^(?:&&|\|\||[|;()<>]+)$/;
const RESETS_PIPELINE = /^(?:&&|\|\||[|;])$/;
const IS_ENV_ASSIGNMENT = /^[A-Za-z_][A-Za-z0-9_]*=/;
const IS_FLAG = /^-{1,2}[A-Za-z0-9][\w-]*(?:=.*)?$/;
const IS_QUOTED = /^(['"`])[\s\S]*\1$/;

/// Tint one shell command in place.
///
/// The first word of each pipeline stage is the program, the second is its
/// subcommand, and an operator starts the count again — which is why `git` and
/// `commit` in `git commit && cargo test` both read as they should.
export function appendCommandTokens(parent, command) {
  let expectProgram = true;
  let wordIndex = 0;
  for (const value of shellTokens(command)) {
    if (IS_WHITESPACE.test(value)) {
      token(parent, value);
      continue;
    }
    if (IS_OPERATOR.test(value)) {
      token(parent, value, 'cmd-op');
      if (RESETS_PIPELINE.test(value)) {
        expectProgram = true;
        wordIndex = 0;
      }
      continue;
    }
    if (expectProgram && IS_ENV_ASSIGNMENT.test(value)) {
      token(parent, value, 'cmd-env');
      continue;
    }
    if (expectProgram) {
      token(parent, value, 'cmd-program');
      expectProgram = false;
      wordIndex = 1;
      continue;
    }
    if (IS_FLAG.test(value) || value === '--') token(parent, value, 'cmd-flag');
    else if (IS_QUOTED.test(value)) token(parent, value, 'cmd-string');
    else if (isPathLike(value)) token(parent, value, 'cmd-path');
    else if (wordIndex === 1) token(parent, value, 'cmd-subcommand');
    else token(parent, value);
    wordIndex += 1;
  }
}

/// A shell command as its own row, for a `!command` the person ran.
export function renderCommand(command) {
  const line = el('pre', 'command-line');
  appendCommandTokens(line, command.replace(/^\s*[$❯]\s*/, ''));
  return line;
}

// ---------------------------------------------------------------------------
// JSON
// ---------------------------------------------------------------------------

export function tryParseJson(text) {
  try {
    const value = JSON.parse(text);
    return typeof value === 'object' && value !== null ? value : undefined;
  } catch {
    return undefined;
  }
}

const JSON_TOKEN =
  /"(?:[^"\\]|\\.)*"(\s*:)?|-?\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b|\btrue\b|\bfalse\b|\bnull\b/g;

/// Tint pretty-printed JSON: keys apart from string values, and numbers and
/// keywords apart from both.
export function appendJsonTokens(parent, pretty) {
  JSON_TOKEN.lastIndex = 0;
  let last = 0;
  let match;
  while ((match = JSON_TOKEN.exec(pretty))) {
    if (match.index > last) {
      parent.append(document.createTextNode(pretty.slice(last, match.index)));
    }
    const value = match[0];
    if (value.startsWith('"') && match[1]) {
      // A quoted string followed by a colon is a key, and the colon is
      // punctuation rather than part of it.
      token(parent, value.slice(0, value.length - match[1].length), 'tok-key');
      parent.append(document.createTextNode(match[1]));
    } else if (value.startsWith('"')) {
      token(parent, value, 'tok-str');
    } else if (value === 'true' || value === 'false' || value === 'null') {
      token(parent, value, 'tok-kw');
    } else {
      token(parent, value, 'tok-num');
    }
    last = match.index + value.length;
  }
  if (last < pretty.length) parent.append(document.createTextNode(pretty.slice(last)));
}

// ---------------------------------------------------------------------------
// Code tinting: one cached single-pass regex per language family
// ---------------------------------------------------------------------------
//
// A linear scan classifies comments, strings, macros and attributes, keywords,
// capitalised type names, call sites and numbers. Plain identifiers and
// punctuation never match, so untinted runs coalesce into large text nodes
// rather than one node per word. An unknown language renders untinted, which
// is the right answer for a log.

const CODE_LANG_FAMILY = {
  rust: 'rust',
  rs: 'rust',
  js: 'js',
  jsx: 'js',
  ts: 'js',
  tsx: 'js',
  mjs: 'js',
  cjs: 'js',
  javascript: 'js',
  typescript: 'js',
  python: 'python',
  py: 'python',
  python3: 'python',
  shell: 'shell',
  sh: 'shell',
  bash: 'shell',
  zsh: 'shell',
  fish: 'shell',
  shellscript: 'shell',
  console: 'shell',
  go: 'go',
  golang: 'go',
  c: 'c',
  h: 'c',
  cc: 'c',
  cpp: 'c',
  cxx: 'c',
  hpp: 'c',
  java: 'java',
  kt: 'java',
  kotlin: 'java',
  ruby: 'ruby',
  rb: 'ruby',
  sql: 'sql',
  toml: 'conf',
  ini: 'conf',
  yaml: 'conf',
  yml: 'conf',
  conf: 'conf',
  properties: 'conf',
  env: 'conf',
  dockerfile: 'conf',
  makefile: 'conf',
  css: 'css',
  scss: 'css',
  less: 'css',
  html: 'markup',
  xml: 'markup',
  svg: 'markup',
  vue: 'markup',
  diff: 'diff',
  patch: 'diff',
};

const CODE_FAMILY_SYNTAX = {
  rust: {
    comments: 'slash',
    chars: true,
    types: true,
    calls: true,
    mac: '#!?\\[[^\\]\\n]*\\]|\\b[a-z_]\\w*!(?!=)',
    keywords:
      'as async await break const continue crate dyn else enum extern false fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait true type union unsafe use where while',
  },
  js: {
    comments: 'slash',
    templates: true,
    types: true,
    calls: true,
    mac: '@[A-Za-z_][\\w.]*',
    keywords:
      'abstract as async await break case catch class const continue debugger declare default delete do else enum export extends false finally for from function if implements import in instanceof interface keyof let namespace new null of readonly return satisfies static super switch this throw true try type typeof undefined var void while with yield',
  },
  python: {
    comments: 'hash',
    types: true,
    calls: true,
    mac: '^[ \\t]*@[\\w.]+',
    keywords:
      'and as assert async await break class continue def del elif else except finally for from global if import in is lambda nonlocal not or pass raise return try while with yield False None True',
  },
  shell: {
    comments: 'hash',
    vars: true,
    keywords:
      'alias begin break case cd command continue do done echo elif else end esac eval exec exit export fi for function if in local read readonly return select set shift source switch then trap unset until while',
  },
  go: {
    comments: 'slash',
    templates: true,
    types: true,
    calls: true,
    keywords:
      'break case chan const continue default defer else fallthrough false for func go goto if import interface iota map nil package range return select struct switch true type var',
  },
  c: {
    comments: 'slash',
    types: true,
    calls: true,
    mac: '^[ \\t]*#[ \\t]*\\w+',
    keywords:
      'auto bool break case catch char class const constexpr continue default delete do double else enum extern false float for goto if inline int long namespace new nullptr operator private protected public return short signed sizeof static struct switch template this throw true try typedef typename union unsigned using virtual void volatile while',
  },
  java: {
    comments: 'slash',
    types: true,
    calls: true,
    mac: '@[A-Za-z_][\\w.]*',
    keywords:
      'abstract assert boolean break byte case catch char class const continue default do double else enum extends final finally float for fun if implements import instanceof int interface is long native new null object override package private protected public return short static super switch synchronized this throw throws transient true try val var void volatile when while false',
  },
  ruby: {
    comments: 'hash',
    types: true,
    calls: true,
    mac: '@{1,2}[A-Za-z_]\\w*',
    keywords:
      'alias and begin break case class def do else elsif end ensure false for if in module next nil not or redo require rescue retry return self super then true undef unless until when while yield',
  },
  sql: {
    comments: 'sql',
    calls: true,
    flags: 'gim',
    keywords:
      'add all alter and as asc begin between by case create default delete desc distinct drop else end exists foreign from group having if in index inner insert into is join key left like limit not null offset on or order outer primary references right select set table then union unique update values view when where',
  },
  conf: { comments: 'hash', keys: true },
  css: { comments: 'slash', keys: true, calls: true },
  markup: { comments: 'markup', tags: true },
  diff: { diff: true },
};

const codeTintRegexCache = new Map();

function codeTintRegex(family) {
  const cached = codeTintRegexCache.get(family);
  if (cached) return cached;
  const syntax = CODE_FAMILY_SYNTAX[family];
  const parts = [];
  if (syntax.diff) {
    parts.push(
      '(?<cmt>^(?:@@|diff|index|---|\\+\\+\\+)[^\\n]*)',
      '(?<ins>^\\+[^\\n]*)',
      '(?<del>^-[^\\n]*)',
    );
  } else {
    if (syntax.comments === 'slash')
      parts.push('(?<cmt>\\/\\/[^\\n]*|\\/\\*[\\s\\S]*?(?:\\*\\/|$))');
    else if (syntax.comments === 'hash') parts.push('(?<cmt>(?<=^|\\s)#[^\\n]*)');
    else if (syntax.comments === 'sql')
      parts.push('(?<cmt>--[^\\n]*|\\/\\*[\\s\\S]*?(?:\\*\\/|$))');
    else if (syntax.comments === 'markup') parts.push('(?<cmt><!--[\\s\\S]*?(?:-->|$))');
    if (syntax.tags) parts.push('(?<tag><\\/?[A-Za-z][\\w.:-]*|\\/?>)');
    // A Rust single quote is a character literal of exactly one character, so
    // a lifetime like `<'a, T>` must not be eaten as an unterminated string.
    const strings = ['"(?:[^"\\\\\\n]|\\\\.)*"?'];
    strings.push(syntax.chars ? "'(?:[^'\\\\\\n]|\\\\.)'" : "'(?:[^'\\\\\\n]|\\\\.)*'?");
    if (syntax.templates) strings.push('`(?:[^`\\\\]|\\\\.)*`?');
    parts.push(`(?<str>${strings.join('|')})`);
    if (syntax.mac) parts.push(`(?<mac>${syntax.mac})`);
    if (syntax.keys) parts.push('(?<key>^[ \\t]*[\\w$.-]+(?=[ \\t]*[:=]))');
    if (syntax.vars) parts.push('(?<vari>\\$(?:\\{[^}\\n]*\\}|\\w+))');
    if (syntax.keywords) parts.push(`(?<kw>\\b(?:${syntax.keywords.split(' ').join('|')})\\b)`);
    if (syntax.types) parts.push('(?<type>\\b[A-Z][A-Za-z0-9_]*\\b)');
    if (syntax.calls) parts.push('(?<fn>\\b[A-Za-z_]\\w*(?=\\s*\\())');
    parts.push('(?<num>\\b\\d[\\w.]*\\b)');
  }
  const expression = new RegExp(parts.join('|'), syntax.flags || 'gm');
  codeTintRegexCache.set(family, expression);
  return expression;
}

function codeTokenClass(groups) {
  if (groups.cmt !== undefined) return 'tok-cmt';
  if (groups.str !== undefined) return 'tok-str';
  if (groups.mac !== undefined) return 'tok-mac';
  if (groups.kw !== undefined) return 'tok-kw';
  if (groups.type !== undefined) return 'tok-type';
  if (groups.fn !== undefined) return 'tok-fn';
  if (groups.key !== undefined || groups.vari !== undefined || groups.tag !== undefined) {
    return 'tok-key';
  }
  if (groups.num !== undefined) return 'tok-num';
  if (groups.ins !== undefined) return 'tok-add';
  if (groups.del !== undefined) return 'tok-del';
  return '';
}

/// Tint `body` into `parent`.
///
/// An unknown language, or a body past the size bound, is written as one text
/// node. The bound matters: tinting is linear, but a megabyte of log is worth
/// nobody's battery.
export function appendCodeTokens(parent, body, lang) {
  const family = CODE_LANG_FAMILY[lang];
  if (!family || body.length > CODE_TINT_CHAR_LIMIT) {
    parent.textContent = body;
    return;
  }
  const expression = codeTintRegex(family);
  expression.lastIndex = 0;
  let last = 0;
  let match;
  while ((match = expression.exec(body))) {
    if (!match[0]) {
      expression.lastIndex += 1;
      continue;
    }
    const className = codeTokenClass(match.groups);
    if (!className) continue;
    if (match.index > last) {
      parent.append(document.createTextNode(body.slice(last, match.index)));
    }
    token(parent, match[0], className);
    last = match.index + match[0].length;
  }
  if (last < body.length) parent.append(document.createTextNode(body.slice(last)));
}

// ---------------------------------------------------------------------------
// Language sniffing
// ---------------------------------------------------------------------------
//
// Tool results usually arrive as bare unfenced text with no filename in sight,
// so the language has to be sniffed from the content. Fingerprints count
// distinctive constructs over the head of the dump, and only a clear
// multi-hit winner tints — so logs and prose stay plain.

const LANG_FINGERPRINTS = [
  ['diff', /^@@ -\d|^diff --git|^index \w{7}|^--- |^\+\+\+ /gm],
  [
    'rust',
    /\bfn \w+|\blet (?:mut )?\w|\bpub (?:fn|struct|enum|trait|mod|use|const|static|async|crate|\w+:)|\bimpl[ <]|\buse \w+::|#!?\[|\.unwrap\(\)|\bSelf\b|&mut |\bmatch \w+[\w.]* \{|\b[a-z_]\w*!\(/g,
  ],
  [
    'python',
    /\bdef \w+\(|\bimport \w|\bfrom [\w.]+ import |\bself\.\w|\belif |\bexcept[ :]|^[ \t]*@\w|\blambda /gm,
  ],
  [
    'js',
    /\bconst \w+ *=|\blet \w+ *=|\bvar \w+ *=| => |=>\s*\{|\bfunction[ (]|\bexport (?:default|const|function|class)|\bimport \{|\brequire\(|===|!==|\bawait |\bconsole\.\w/g,
  ],
  ['go', /\bfunc \w+\(|\bfunc \(\w| := |^package \w+$|\bfmt\.\w|\bdefer |\bgo func\b|\bchan /gm],
  [
    'java',
    /\bpublic (?:class|static|final|void)|\bprivate (?:final )?\w+ \w+|\bSystem\.out|@Override\b|\bthrows \w|\bextends \w+ \{/g,
  ],
  [
    'c',
    /#include\s*[<"]|\bprintf\s*\(|\bvoid \w+\s*\(|\bint \w+ *[=(;]|\bchar \*|\bsizeof\(|\bstd::|\btemplate *</g,
  ],
  [
    'shell',
    /(?:^|[|&;] *)(?:cargo|git|npm|pnpm|yarn|sudo|curl|grep|rg|sed|awk|rm|cp|mv|mkdir|echo|cd|ls|cat|tail|head|python3?|node|make|docker|kubectl|ssh|export) |&& |\$\(|\$\{|^#!\//gm,
  ],
  ['ruby', /\bdef \w+$|^[ \t]*end$|\bputs |\brequire ["']|\bdo \|\w/gm],
  [
    'sql',
    /\bselect .+ from |\binsert into |\bcreate (?:table|index|view)|\bgroup by |\border by |\bleft join |\bwhere \w+ *=/gi,
  ],
  ['html', /<\/[a-z][\w-]*>|<[a-z][\w-]* [^<>\n]*>|<!doctype|<!--/gi],
  ['conf', /^\[[\w. -]+\]$|^[\w.-]+ *= [^=\n]/gm],
];

function looksLikeDiff(text) {
  return (
    /^(?:diff --git |@@ -\d+(?:,\d+)? \+\d+(?:,\d+)? @@)/m.test(text) ||
    (/^--- [^\n]+$/m.test(text) && /^\+\+\+ [^\n]+$/m.test(text))
  );
}

/// The language `text` looks like, or the empty string when nothing wins
/// clearly enough to be worth tinting.
export function detectLang(text, minScore = DETECT_MIN_SCORE) {
  if (looksLikeDiff(text)) return 'diff';
  const sample = text.slice(0, DETECT_SAMPLE_CHARS);
  let best = '';
  let bestScore = 0;
  let runnerUp = 0;
  for (const [lang, expression] of LANG_FINGERPRINTS) {
    expression.lastIndex = 0;
    const score = (sample.match(expression) || []).length;
    if (score > bestScore) {
      runnerUp = bestScore;
      bestScore = score;
      best = lang;
    } else if (score > runnerUp) {
      runnerUp = score;
    }
  }
  return bestScore >= minScore && bestScore >= runnerUp * 2 ? best : '';
}

// ---------------------------------------------------------------------------
// Blocks
// ---------------------------------------------------------------------------

/// A collapsible block whose content is built the first time it is opened.
///
/// This is what makes a five-thousand-line tool dump cost nothing until
/// somebody asks for it, which on a phone is the difference between a usable
/// transcript and an unusable one.
export function foldBlock(label, build, open = false) {
  const details = el('details', 'block-fold');
  const summary = el('summary');
  summary.textContent = label;
  details.append(summary);
  let built = false;
  const buildOnce = () => {
    if (built) return;
    built = true;
    details.append(build());
  };
  details.addEventListener('toggle', () => {
    if (details.open) buildOnce();
  });
  if (open) {
    details.open = true;
    buildOnce();
  }
  return details;
}

function jsonBlock(value) {
  const pretty = JSON.stringify(value, null, 2);
  const build = () => {
    const pre = el('pre', 'code-block');
    pre.dataset.lang = 'json';
    const code = el('code');
    appendJsonTokens(code, pretty);
    pre.append(code);
    return pre;
  };
  const lines = countLines(pretty);
  return lines > FOLD_LINE_LIMIT ? foldBlock(`json · ${lines} lines`, build) : build();
}

/// One block of code, tinted, folded when it is long, and pretty-printed when
/// it turns out to be JSON.
///
/// A fence is code by declaration, so sniffing may accept a weaker signal here
/// than the unfenced path does.
export function codeBlock(body, lang) {
  if (lang === 'json' || (!lang && /^\s*[[{]/.test(body))) {
    const parsed = tryParseJson(body);
    if (parsed !== undefined) return jsonBlock(parsed);
  }
  const tint = lang || detectLang(body, 2);
  const build = () => {
    const pre = el('pre', 'code-block');
    if (tint) pre.dataset.lang = tint;
    const code = el('code');
    appendCodeTokens(code, body, tint);
    pre.append(code);
    return pre;
  };
  const lines = countLines(body);
  return lines > FOLD_LINE_LIMIT ? foldBlock(`${tint || 'code'} · ${lines} lines`, build) : build();
}

/// A whole tool result.
///
/// Everything a tool emits is preformatted by nature — it is output, not
/// prose — so this never runs the Markdown renderer over it. What it does is
/// notice the shapes worth rendering properly: a JSON payload, something that
/// sniffs as a known language, and anything long enough to be worth folding.
export function renderToolOutput(text) {
  const body = String(text ?? '').replace(/\n+$/, '');
  const parsed = /^\s*[[{]/.test(body) ? tryParseJson(body) : undefined;
  if (parsed !== undefined) return jsonBlock(parsed);
  const lang = detectLang(body);
  if (lang) return codeBlock(body, lang);
  const build = () => {
    const pre = el('pre', 'entry-text');
    pre.textContent = body;
    return pre;
  };
  const lines = countLines(body);
  return lines > FOLD_LINE_LIMIT ? foldBlock(`${lines} lines`, build) : build();
}
