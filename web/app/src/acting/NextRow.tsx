// C10: one row of the next list, and of Mine: the display state, the title, and one fact in
// words. The owner and the primary action sit at the right end, drawn on hover and while the
// row is selected; selecting the row (a click, or j and k) opens the inspector for the rest.
import { useEffect, useRef, type MouseEvent } from "react";
import { Link, useNavigate } from "react-router";

import { flag } from "../flags.ts";
import type { Ready } from "../detail/model.ts";
import { statusTone, statusWord } from "../status/words.ts";
import { Badge } from "../ui/kit.tsx";
import type { SortBy } from "./address.ts";
import { reasonOf } from "./reasons.ts";
import { RowActions } from "./RowActions.tsx";
import type { NodeRow } from "./why.ts";

/** A click on the row itself, not on a control inside it, selects it. */
function onRow(event: MouseEvent): boolean {
  return !(event.target instanceof Element && event.target.closest("a, button, select, input, textarea, label"));
}

export interface NextRowProps {
  view: Ready;
  row: NodeRow;
  /** Where the inspector for this row opens. */
  to: string;
  selected: boolean;
  /** The row's place among the top three by rank, when it has one. */
  tag?: number | undefined;
  sort: SortBy;
  testId?: string;
  /** Say who owns the row; Mine leaves it out, as every row there is the viewer's. */
  owner?: boolean;
}

export function NextRow({ view, row, to, selected, tag, sort, testId = "next-item", owner = true }: NextRowProps) {
  const navigate = useNavigate();
  const fact = reasonOf(view, row, sort);
  const item = useRef<HTMLLIElement>(null);
  // j and k move the selection: keep the selected row in view.
  useEffect(() => {
    if (selected) {
      item.current?.scrollIntoView({ block: "nearest" });
    }
  }, [selected]);
  return (
    <li
      ref={item}
      className="next-row"
      data-compact={flag("next-list-compact")}
      data-testid={testId}
      data-node={row.key}
      data-selected={selected}
      data-slack={row.slack_days ?? ""}
      data-gravity={row.gravity}
      data-unlocks={row.unlocks}
      data-rank={row.rank?.rank}
      onClick={(event) => {
        if (onRow(event)) {
          void navigate(to);
        }
      }}
    >
      <div className="next-row-main">
        <span className="next-tag mono" data-testid={tag === undefined ? undefined : "rank-tag"}>
          {tag === undefined ? "" : `#${String(tag)}`}
        </span>
        <Badge tone={statusTone(row.display_state)}>{statusWord(row.display_state, row.kind)}</Badge>
        <span className="next-row-text">
          <Link to={to} className="next-title" data-node={row.key} aria-current={selected ? "true" : undefined}>
            {row.title}
          </Link>
          {fact === undefined ? null : (
            <span className="next-fact muted" data-testid="fact">
              {fact}
            </span>
          )}
        </span>
      </div>
      <RowActions view={view} row={row} to={to} owner={owner} />
    </li>
  );
}
