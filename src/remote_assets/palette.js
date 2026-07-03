// Command palette (<mj-command-palette>): a Cmd-K launcher over actions the
// app provides through `commandsProvider`. Light DOM on purpose — it shares
// the page stylesheet and keeps the viewer's no-innerHTML posture: every
// node is built with createElement/textContent.

export class MjCommandPalette extends HTMLElement {
  constructor() {
    super();
    /** Returns [{ label, hint?, run }] — re-evaluated on every open. */
    this.commandsProvider = () => [];
    this._items = [];
    this._active = 0;
    this._built = false;
  }

  connectedCallback() {
    if (this._built) {
      return;
    }
    this._built = true;

    this._dialog = document.createElement("dialog");
    this._dialog.className = "palette-dialog";

    this._input = document.createElement("input");
    this._input.type = "text";
    this._input.placeholder = "Type a command…";
    this._input.setAttribute("aria-label", "Command palette");
    this._input.autocomplete = "off";
    this._input.spellcheck = false;

    this._list = document.createElement("ul");
    this._list.className = "palette-list";
    this._list.setAttribute("role", "listbox");

    this._dialog.append(this._input, this._list);
    this.append(this._dialog);

    this._input.addEventListener("input", () => {
      this._active = 0;
      this._render();
    });
    this._input.addEventListener("keydown", (event) => {
      if (event.key === "ArrowDown") {
        event.preventDefault();
        this._setActive(this._active + 1);
      } else if (event.key === "ArrowUp") {
        event.preventDefault();
        this._setActive(this._active - 1);
      } else if (event.key === "Enter") {
        event.preventDefault();
        this._choose(this._active);
      }
      // Escape closes the native dialog on its own.
    });
    // A click on the backdrop (the dialog element itself) dismisses.
    this._dialog.addEventListener("click", (event) => {
      if (event.target === this._dialog) {
        this.close();
      }
    });
  }

  open() {
    if (!this._built || this._dialog.open) {
      return;
    }
    this._all = this.commandsProvider() || [];
    this._input.value = "";
    this._active = 0;
    this._render();
    this._dialog.showModal();
    this._input.focus();
  }

  close() {
    if (this._built && this._dialog.open) {
      this._dialog.close();
    }
  }

  toggle() {
    if (this._built && this._dialog.open) {
      this.close();
    } else {
      this.open();
    }
  }

  _filtered() {
    const needle = this._input.value.trim().toLowerCase();
    if (!needle) {
      return this._all;
    }
    return this._all.filter((command) =>
      `${command.label} ${command.hint || ""}`.toLowerCase().includes(needle),
    );
  }

  _render() {
    this._items = this._filtered();
    if (this._active >= this._items.length) {
      this._active = Math.max(0, this._items.length - 1);
    }
    this._list.replaceChildren();
    if (!this._items.length) {
      const empty = document.createElement("li");
      empty.className = "palette-empty";
      empty.textContent = "No matching commands";
      this._list.append(empty);
      return;
    }
    this._items.forEach((command, index) => {
      const item = document.createElement("li");
      item.className = "palette-item";
      item.setAttribute("role", "option");
      item.classList.toggle("active", index === this._active);
      const label = document.createElement("span");
      label.className = "palette-label";
      label.textContent = command.label;
      item.append(label);
      if (command.hint) {
        const hint = document.createElement("span");
        hint.className = "palette-hint";
        hint.textContent = command.hint;
        item.append(hint);
      }
      item.addEventListener("pointerenter", () => this._setActive(index));
      item.addEventListener("click", () => this._choose(index));
      this._list.append(item);
    });
  }

  _setActive(index) {
    if (!this._items.length) {
      return;
    }
    this._active = Math.min(Math.max(index, 0), this._items.length - 1);
    [...this._list.children].forEach((el, i) => {
      el.classList.toggle("active", i === this._active);
    });
    const active = this._list.children[this._active];
    active?.scrollIntoView({ block: "nearest" });
  }

  _choose(index) {
    const command = this._items[index];
    if (!command) {
      return;
    }
    this.close();
    command.run();
  }
}

customElements.define("mj-command-palette", MjCommandPalette);
