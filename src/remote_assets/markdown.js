// Markdown-lite rendering for transcript entries. Builds DOM nodes
// exclusively via textContent/createElement — never innerHTML — so
// agent-generated content is structurally unable to execute.

export function appendInline(el, text) {
  const re = /`([^`\n]+)`|\*\*([^*\n]+?)\*\*|\[([^\]\n]+)\]\((https?:\/\/[^\s)]+)\)|(https?:\/\/[^\s<>"')\]]+)/g;
  let last = 0;
  let match;
  while ((match = re.exec(text))) {
    if (match.index > last) {
      el.appendChild(document.createTextNode(text.slice(last, match.index)));
    }
    if (match[1] !== undefined) {
      const code = document.createElement("code");
      code.textContent = match[1];
      el.appendChild(code);
    } else if (match[2] !== undefined) {
      const strong = document.createElement("strong");
      strong.textContent = match[2];
      el.appendChild(strong);
    } else {
      const anchor = document.createElement("a");
      anchor.href = match[4] !== undefined ? match[4] : match[5];
      anchor.textContent = match[3] !== undefined ? match[3] : match[5];
      anchor.target = "_blank";
      anchor.rel = "noopener noreferrer";
      el.appendChild(anchor);
    }
    last = match.index + match[0].length;
  }
  if (last < text.length) {
    el.appendChild(document.createTextNode(text.slice(last)));
  }
}

export function appendProse(fragment, text) {
  const lines = text.split("\n");
  let paragraph = [];
  let list = null;
  const flushParagraph = () => {
    if (!paragraph.length) {
      return;
    }
    const p = document.createElement("p");
    appendInline(p, paragraph.join("\n"));
    fragment.appendChild(p);
    paragraph = [];
  };
  const flushList = () => {
    if (list) {
      fragment.appendChild(list.el);
      list = null;
    }
  };
  for (const line of lines) {
    if (!line.trim()) {
      flushParagraph();
      flushList();
      continue;
    }
    const heading = /^(#{1,6})\s+(.*)$/.exec(line);
    if (heading) {
      flushParagraph();
      flushList();
      const el = document.createElement(`h${Math.min(3 + heading[1].length, 6)}`);
      appendInline(el, heading[2]);
      fragment.appendChild(el);
      continue;
    }
    const item = /^\s*(?:([-*])|(\d+)[.)])\s+(.*)$/.exec(line);
    if (item) {
      flushParagraph();
      const ordered = item[2] !== undefined;
      if (!list || list.ordered !== ordered) {
        flushList();
        list = { el: document.createElement(ordered ? "ol" : "ul"), ordered };
      }
      const li = document.createElement("li");
      appendInline(li, item[3]);
      list.el.appendChild(li);
      continue;
    }
    flushList();
    paragraph.push(line);
  }
  flushParagraph();
  flushList();
}

export function renderRichText(text) {
  const fragment = document.createDocumentFragment();
  // Odd-indexed segments sit between ``` fences; an unterminated fence
  // (mid-stream) still renders as a code block.
  const segments = (text || "").split("```");
  segments.forEach((segment, index) => {
    if (index % 2 === 1) {
      const pre = document.createElement("pre");
      pre.className = "code-block";
      const code = document.createElement("code");
      let body = segment;
      const newline = body.indexOf("\n");
      if (newline !== -1 && /^[\w#+./-]*$/.test(body.slice(0, newline).trim())) {
        body = body.slice(newline + 1);
      }
      code.textContent = body.replace(/\n$/, "");
      pre.appendChild(code);
      fragment.appendChild(pre);
    } else if (segment) {
      appendProse(fragment, segment);
    }
  });
  return fragment;
}

export function entryKind(entry) {
  return entry && typeof entry.kind === "string" && entry.kind ? entry.kind : "system";
}

export function entryLabel(kind) {
  switch (kind) {
    case "user":
      return "User";
    case "agent":
      return "Agent";
    case "thought":
      return "Thought";
    case "tool":
      return "Tool";
    default:
      return "System";
  }
}

export function entryIcon(kind) {
  switch (kind) {
    case "user":
      return "U";
    case "agent":
      return "A";
    case "thought":
      return "T";
    case "tool":
      return "#";
    default:
      return "i";
  }
}
