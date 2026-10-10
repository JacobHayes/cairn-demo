// What sits over the graph (5.8): the detail ladder at the top left, the View menu at the top
// right (Signals, Show origins, Key), the trace bar for the selection, the Select mode's band,
// and the count of what is hidden at the bottom left. Quiet by rule: one control each.
import { Panel } from "@xyflow/react";
import type { ReactNode } from "react";

import { flag } from "../flags.ts";
import { Menu } from "../screens/Menu.tsx";
import { Button, Segmented } from "../ui/kit.tsx";
import { STEP_WORDS } from "./ladder.ts";
import { LENS_MEANING, LENS_WORDS } from "./lens.ts";
import { LENSES, type Lens, type Step } from "./model.ts";

/**
 * C2: the detail ladder (Stages, Decisions, Work, All), which replaces the kind checkboxes. The
 * trace bar or the Select band shares its row, and wraps under it when the canvas is narrow.
 */
export function Ladder({ steps, step, onStep, children }: { steps: readonly Step[]; step: Step; onStep: (step: Step) => void; children?: ReactNode }) {
  return (
    <Panel position="top-left" className="canvas-panel canvas-top">
      <Segmented label="Detail" value={step} options={steps.map((each) => ({ value: each, label: flag("graph-ladder-v2") ? `${String(steps.indexOf(each) + 1)}. ${STEP_WORDS[each]}` : STEP_WORDS[each] }))} onChange={onStep} data-testid="ladder" />
      {children}
    </Panel>
  );
}

/** The four kinds of line, drawn as the canvas draws them, for View, Key. */
function Key() {
  const samples = [
    { marker: "arrow", dash: undefined, words: "Needs: the line ends in an arrow." },
    { marker: "diamond", dash: "2 3", words: "Applies only if: the line ends in a hollow diamond." },
    { marker: "bar", dash: "2 3", words: "Waits for the stage to open: the line ends in a bar." },
    { marker: "none", dash: "2 3", words: "Dates only: a faint dotted line that does not block." },
  ] as const;
  return (
    <ul className="canvas-key" data-testid="canvas-key">
      {samples.map((sample) => (
        <li key={sample.marker}>
          <svg width="44" height="14" viewBox="0 0 44 14" aria-hidden="true">
            <path d={sample.marker === "none" ? "M2 7 H42" : "M2 7 H32"} stroke="currentColor" fill="none" strokeDasharray={sample.dash} strokeOpacity={sample.marker === "none" ? 0.5 : 1} />
            {sample.marker === "arrow" ? <path d="M42 7 L33 2.500 L33 11.500 Z" fill="currentColor" /> : null}
            {sample.marker === "diamond" ? <path d="M42 7 L37.500 2.500 L33 7 L37.500 11.500 Z" fill="var(--color-surface)" stroke="currentColor" /> : null}
            {sample.marker === "bar" ? <path d="M40 2 V12" stroke="currentColor" strokeWidth="2" /> : null}
          </svg>
          <span>{sample.words}</span>
        </li>
      ))}
    </ul>
  );
}

/** How the graph is drawn, never which nodes it holds: the Signals lens, origins, and the key to the lines. */
export function ViewMenu({ lens, origins, onLens, onOrigins }: { lens: Lens | undefined; origins: boolean; onLens: (lens: Lens | undefined) => void; onOrigins: (on: boolean) => void }) {
  return (
    <Panel position="top-right" className="canvas-panel canvas-view">
      {lens === undefined ? null : (
        <span className="canvas-lens-chip" data-testid="lens-chip">
          Signals: {LENS_WORDS[lens]}
          <button type="button" aria-label="Turn the signals off" onClick={() => { onLens(undefined); }}>
            &times;
          </button>
        </span>
      )}
      <Menu label="View" testId="view-menu" role="dialog" align="end" trigger="View &#9662;">
        {() => (
          <div className="stack" data-testid="view-panel">
            <Segmented
              label="Signals"
              value={lens ?? "off"}
              options={[{ value: "off", label: "Off" }, ...LENSES.map((each) => ({ value: each, label: LENS_WORDS[each] }))]}
              onChange={(value) => { onLens(value === "off" ? undefined : (LENSES.find((each) => each === value))); }}
              data-testid="signals-choice"
            />
            <span className="muted small">{lens === undefined ? "Put one number on each card." : LENS_MEANING[lens]}</span>
            <label className="toggle" data-testid="show-origins">
              <input type="checkbox" checked={origins} onChange={(event) => { onOrigins(event.target.checked); }} />
              Show where each node came from
            </label>
            <details>
              <summary>Key to the lines</summary>
              <Key />
            </details>
          </div>
        )}
      </Menu>
    </Panel>
  );
}

/** The trace bar (5.7): what is selected, how much it needs and unblocks, and what lies outside this view. */
export function TraceBar({ title, needs, unblocks, outside, onReveal, onClear }: { title: string; needs: number; unblocks: number; outside: number; onReveal: (() => void) | undefined; onClear: () => void }) {
  return (
    <div className="canvas-trace" data-testid="trace-bar">
      <strong>{title}</strong>
      <span>needs {needs}</span>
      <span>unblocks {unblocks}</span>
      {outside === 0 ? null : (
        <span>
          {outside} outside this view
          {onReveal === undefined ? null : (
            <>
              {" "}
              <button type="button" className="link" data-testid="trace-reveal" onClick={onReveal}>
                Reveal
              </button>
            </>
          )}
        </span>
      )}
      <button type="button" className="link" aria-label="Clear the selection" data-testid="trace-clear" onClick={onClear}>
        Esc
      </button>
    </div>
  );
}

/** The Select mode's band (5.9). Editing structure has its own full-width band under the header (authoring/JourneyAuthoring.tsx). */
export function SelectBand({ onDone }: { onDone: () => void }) {
  return (
    <div className="canvas-trace" data-testid="select-band">
      <strong>Selecting</strong>
      <span>shift-click or shift-drag</span>
      <Button onClick={onDone}>Done</Button>
    </div>
  );
}

/** What the view hides by default, counted where it was hidden (5.2): "4 not relevant hidden - Show". */
export function HiddenCount({ count, onShow }: { count: number; onShow: () => void }) {
  if (count === 0) {
    return null;
  }
  return (
    <Panel position="bottom-left" className="canvas-panel canvas-hidden" data-testid="hidden-count">
      {count} not relevant hidden{" "}
      <button type="button" className="link" data-testid="hidden-show" onClick={onShow}>
        Show
      </button>
    </Panel>
  );
}
