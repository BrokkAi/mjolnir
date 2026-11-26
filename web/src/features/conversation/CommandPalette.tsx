import type { ViewerMjCommand } from '../../api/types';

/**
 * `/` completion over the daemon's published command list. The list is never
 * hardcoded here: the server knows what this session's harness advertises, and
 * a second copy in the browser is a copy that drifts.
 */

export interface PaletteItem {
  insert: string;
  label: string;
  hint: string;
}

/** The query after a leading `/`, or `null` when the text is not a slash word. */
export function slashQuery(text: string): string | null {
  const match = /^\/(\S*)$/.exec(text);
  return match ? (match[1] ?? '').toLowerCase() : null;
}

export function paletteMatches(text: string, commands: readonly ViewerMjCommand[]): PaletteItem[] {
  const query = slashQuery(text);
  if (query === null) return [];
  return commands
    .filter(
      command =>
        command.name.toLowerCase().startsWith(query) ||
        command.description.toLowerCase().includes(query),
    )
    .map(command => ({
      insert: `/${command.name} `,
      label: `/${command.name}${command.argument ? ` <${command.argument}>` : ''}`,
      hint: command.description,
    }));
}

export interface CommandPaletteProps {
  items: PaletteItem[];
  selected: number;
  onHover: (index: number) => void;
  onPick: (item: PaletteItem) => void;
}

export function CommandPalette({ items, selected, onHover, onPick }: CommandPaletteProps) {
  if (items.length === 0) return null;
  return (
    <div
      className="absolute inset-x-0 bottom-full z-30 mb-1 overflow-hidden rounded-xl border border-line bg-surface shadow-xl"
      data-testid="command-palette"
    >
      <ul role="listbox" aria-label="Commands" className="max-h-56 overflow-y-auto overscroll-contain py-1">
        {items.map((item, index) => (
          <li key={item.insert} role="option" aria-selected={index === selected}>
            <button
              type="button"
              onMouseEnter={() => onHover(index)}
              onClick={() => onPick(item)}
              className={`flex w-full items-baseline gap-2 px-2.5 py-1.5 text-left ${
                index === selected ? 'bg-raised' : ''
              }`}
            >
              <span className="shrink-0 font-mono text-[12.5px] text-fg">{item.label}</span>
              <span className="min-w-0 flex-1 truncate text-[11px] text-muted">{item.hint}</span>
            </button>
          </li>
        ))}
      </ul>
      <p className="border-t border-line px-2.5 py-1 text-[10.5px] text-muted">
        <span className="font-mono text-fg">!command</span> runs a shell command in this session
      </p>
    </div>
  );
}
