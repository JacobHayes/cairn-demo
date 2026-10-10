// The node's journey state a section edits in place (B5, B6, E3, F5): its pin, routed
// through the decision that feeds it when one does; its snooze (what to set aside, until when);
// a force include lifted; its weight; and its participations. Each is one patch whose rejection shows here,
// a pin's with the moves that resolve a contradictory chain.
import type { Schema } from "@cairn/client";
import { useId, type ReactNode } from "react";

import { flag } from "../flags.ts";
import { dateWords } from "../timeline/model.ts";
import { Button, Field } from "../ui/kit.tsx";
import { isTerminal, nodeOf, transition, type Mutation, type NodeDetail, type Ready } from "./model.ts";
import { NodeLink } from "./parts.tsx";
import { Rejected } from "./Rejected.tsx";
import { entityName } from "./sections.tsx";
import { nextMonday, setAsideOptions, tomorrow, withinScope } from "./snooze.ts";
import { useFormDraft, useNodeWrite, type NodeWrite, type Seen } from "./write.ts";

type Draft = ReturnType<typeof useFormDraft<string>>;

/** A one-field form: opened with a starting value, kept as a draft, sent as one patch. */
function DraftForm({
  write,
  draft,
  form,
  start,
  label,
  input,
  mutations,
}: {
  write: NodeWrite;
  draft: Draft;
  form: string;
  start: string;
  label: string;
  input: (value: string, change: (value: string) => void) => ReactNode;
  mutations: (value: string) => Mutation[];
}) {
  if (draft.draft === undefined) {
    return (
      <Button disabled={write.disabled} onClick={() => { draft.open(start, write.seen); }}>
        {label}
      </Button>
    );
  }
  const seen = draft.draft;
  const { value } = seen;
  const save = async () => {
    if (await write.run(mutations(value), seen)) {
      draft.close();
    }
  };
  return (
    <span className="row" data-testid={`${form}-form`} onBlur={(event) => {
      if (flag("inspector-autosave") && !event.currentTarget.contains(event.relatedTarget)
          && !write.disabled && value.trim() !== "" && value !== start) void save();
    }}>
      {input(value, draft.change)}
      <Button primary disabled={write.disabled || value.trim() === ""} onClick={() => void save()}>
        Save
      </Button>
      <Button onClick={() => { draft.close(); write.dismiss(); }}>Cancel</Button>
    </span>
  );
}

const dateInput = (label: string) => (value: string, change: (value: string) => void) => (
  <Field type="date" aria-label={label} value={value} onChange={(event) => { change(event.target.value); }} />
);

/**
 * E3: a decision's answer while it is in effect: decided and relevant (crates/engine
 * `answer_in_effect`). A decision out of scope or undecided keeps its answer, but what it
 * drove (a role, a milestone's pin) is empty.
 */
export function answerInEffect(view: Ready, decision: string): Schema<"AnswerValue"> | undefined {
  const decided = view.journey.graph.state?.nodes?.[decision]?.state === "decided";
  const relevant = view.derived.nodes[decision]?.relevance.value === "relevant";
  return decided && relevant ? view.journey.graph.state?.answers?.[decision] : undefined;
}

/**
 * F2, E3: the node's pin. A milestone a date decision feeds is pinned only through that
 * decision: setting its date answers the decision, and clearing it reopens the decision.
 */
export function PinEditor({ view, detail }: { view: Ready; detail: NodeDetail }) {
  const write = useNodeWrite(view, `pin:${detail.node.key}`, detail.node.key);
  const key = detail.node.key;
  const pinForm = useFormDraft<string>(write.journey, key, "pin");
  const fedBy = detail.fedBy;
  const fedAnswer = fedBy === undefined ? undefined : answerInEffect(view, fedBy.key);
  const current = fedBy === undefined ? detail.pin : fedAnswer !== undefined && "date" in fedAnswer ? fedAnswer.date : undefined;
  const set = (date: string): Mutation[] =>
    fedBy === undefined ? [{ op: "set_pin", node: key, date }] : [{ op: "answer", decision: fedBy.key, value: { date } }];
  const clear: Mutation = fedBy === undefined ? { op: "clear_pin", node: key } : transition(fedBy.key, "reopen");
  return (
    <div className="stack" data-testid="pin">
      <span>
        {current === undefined ? null : "Pinned to "}
        <strong data-testid="pin-date">{current === undefined ? "No pin" : dateWords(current, view.derived.today)}</strong>
        {fedBy === undefined ? null : (
          <span className="muted small" data-testid="pin-through">
            {" "}
            set by answering <NodeLink view={view} node={fedBy.key} />: editing it here answers that decision
          </span>
        )}
      </span>
      <span className="row">
        <DraftForm
          write={write}
          draft={pinForm}
          form="pin"
          start={current ?? view.key.today}
          label={current === undefined ? "Pin a date" : "Change the pin"}
          input={dateInput("Pin date")}
          mutations={set}
        />
        {current === undefined ? null : (
          <Button disabled={write.disabled} onClick={() => void write.run([clear])}>
            {fedBy === undefined ? "Unpin" : "Unpin (reopens the decision)"}
          </Button>
        )}
      </span>
      <Rejected view={view} write={write} onResolved={pinForm.close} />
    </div>
  );
}

/** What a snooze waits for: its date, or the node. */
function SnoozeUntil({ view, target }: { view: Ready; target: Schema<"SnoozeTarget"> }) {
  return "date" in target ? target.date : <NodeLink view={view} node={target.node} />;
}

/** B6: a descendant held by its container's snooze, which is the one to lift (the node has no snooze of its own to). */
function SnoozedThrough({ view, write, container, target }: { view: Ready; write: NodeWrite; container: string; target: Schema<"SnoozeTarget"> }) {
  return (
    <span className="row" data-testid="snoozed-via" data-via={container}>
      <span>
        Snoozed through <NodeLink view={view} node={container} />, until <SnoozeUntil view={view} target={target} />
      </span>
      <Button disabled={write.disabled} onClick={() => void write.run([{ op: "unsnooze", node: container }])}>
        Unsnooze {nodeOf(view, container)?.title ?? container}
      </Button>
    </span>
  );
}

type Until = "tomorrow" | "monday" | "date" | "node";

const UNTIL_OPTIONS = [
  { value: "tomorrow", label: "Tomorrow" },
  { value: "monday", label: "Next Monday" },
  { value: "date", label: "A date" },
  { value: "node", label: "When something is done" },
] as const;

/** B6: the node, or a container above it with its open count: what a snooze sets aside. */
function SetAside({ options, group, aside, onChange }: { options: ReturnType<typeof setAsideOptions>; group: string; aside: string; onChange: (node: string) => void }) {
  return (
    <fieldset className="stack">
      <legend className="small muted">Set aside</legend>
      {options.map((option) => (
        <label key={option.node} className="choice">
          <input type="radio" name={`${group}-aside`} checked={aside === option.node} onChange={() => { onChange(option.node); }} />
          <span>{option.open === undefined ? option.title : `All of ${option.title}`}</span>
          {option.open === undefined ? null : <span className="choice-effect mono muted">{option.open} open</span>}
        </label>
      ))}
    </fieldset>
  );
}

/** B6: what the snooze form holds: what to set aside and until when, kept across a reload. */
export interface SnoozeDraft {
  aside: string;
  until: Until;
  date: string;
  target: string;
}

export type SnoozeForm = ReturnType<typeof useFormDraft<SnoozeDraft>>;

/** Opens the snooze form on the node itself, until tomorrow. */
export function openSnooze(form: SnoozeForm, view: Ready, node: string, seen: Seen): void {
  form.open({ aside: node, until: "tomorrow", date: tomorrow(view.derived.today), target: "" }, seen);
}

/**
 * B6: the snooze form (shown in place of the buttons while its draft is open): what to set aside
 * (the node, or a container above it with its open count, nearest first) and until when.
 * Setting a container aside holds everything beneath it; the engine refuses a wait on the
 * set-aside work itself. Sent against the revisions its author saw.
 */
export function SnoozePanel({ view, detail, form }: { view: Ready; detail: NodeDetail; form: SnoozeForm }) {
  const write = useNodeWrite(view, `snooze:${detail.node.key}`);
  const group = useId();
  const options = setAsideOptions(view, detail);
  const draft = form.draft;
  if (draft === undefined) {
    return null;
  }
  const { aside, until, date, target } = draft.value;
  const change = (patch: Partial<SnoozeDraft>) => {
    form.change({ ...draft.value, ...patch });
  };
  const today = view.derived.today;
  const scope = withinScope(view, aside);
  const chosen: Schema<"SnoozeTarget"> | undefined =
    until === "tomorrow" ? { date: tomorrow(today) } : until === "monday" ? { date: nextMonday(today) } : until === "date" ? (date === "" ? undefined : { date }) : target === "" ? undefined : { node: target };
  return (
    <div className="stack popover-panel" role="dialog" aria-label="Snooze" data-testid="snooze" data-popover="">
      {options.length === 1 ? null : <SetAside options={options} group={group} aside={aside} onChange={(node) => { change({ aside: node }); }} />}
      <fieldset className="stack">
        <legend className="small muted">Until</legend>
        {UNTIL_OPTIONS.map((option) => (
          <label key={option.value} className="choice">
            <input type="radio" name={`${group}-until`} checked={until === option.value} onChange={() => { change({ until: option.value }); }} />
            <span>{option.label}</span>
          </label>
        ))}
      </fieldset>
      {until === "date" ? <Field type="date" aria-label="Snooze until" value={date} onChange={(event) => { change({ date: event.target.value }); }} /> : null}
      {until === "node" ? (
        <select aria-label="Snooze until node" value={target} onChange={(event) => { change({ target: event.target.value }); }}>
          <option value="">Choose</option>
          {(view.journey.graph.nodes ?? [])
            .filter((node) => !scope.has(node.key))
            .map((node) => (
              <option key={node.key} value={node.key}>
                {node.title}
              </option>
            ))}
        </select>
      ) : null}
      <span className="row">
        <Button
          primary
          disabled={write.disabled || chosen === undefined}
          onClick={() => {
            if (chosen !== undefined) {
              void write.run([{ op: "snooze", node: aside, until: chosen }], draft).then((landed) => {
                if (landed) {
                  form.close();
                }
              });
            }
          }}
        >
          Snooze
        </Button>
        <Button onClick={form.close}>Cancel</Button>
      </span>
      <Rejected view={view} write={write} />
    </div>
  );
}

/** B6: a snooze's state and the way to set one: for a card or row, where the inspector's menu is not. */
export function SnoozeEditor({ view, detail }: { view: Ready; detail: NodeDetail }) {
  const write = useNodeWrite(view, `snooze:${detail.node.key}`, detail.node.key);
  const key = detail.node.key;
  const form = useFormDraft<SnoozeDraft>(write.journey, key, "snooze");
  const stored = view.journey.graph.state?.snoozes?.[key];
  const via = detail.derived.snoozed_via ?? undefined;
  const viaStored = via === undefined ? undefined : view.journey.graph.state?.snoozes?.[via];
  // With a container's snooze over it, `snoozed` is the node's own target while that holds, else the container's.
  const holds = via === undefined ? detail.derived.snoozed != null : JSON.stringify(detail.derived.snoozed) === JSON.stringify(stored);
  return (
    <div className="stack" data-testid="snooze-state">
      {stored === undefined ? null : (
        <span className="row">
          <span>
            Snoozed until <SnoozeUntil view={view} target={stored} />
            {holds ? "" : " (no longer holding)"}
          </span>
          <Button disabled={write.disabled} onClick={() => void write.run([{ op: "unsnooze", node: key }])}>
            Unsnooze
          </Button>
        </span>
      )}
      {via === undefined || viaStored === undefined ? null : <SnoozedThrough view={view} write={write} container={via} target={viaStored} />}
      {isTerminal(detail.record.state) ? null : form.draft !== undefined ? <SnoozePanel view={view} detail={detail} form={form} /> : <Button onClick={() => { openSnooze(form, view, key, write.seen); }}>Snooze…</Button>}
      <Rejected view={view} write={write} />
    </div>
  );
}

/** B5, Gating: a force include's reason, and lifting it. Making one is `⋯ › Include anyway`. */
export function ForceInclude({ view, detail }: { view: Ready; detail: NodeDetail }) {
  const write = useNodeWrite(view, `force-include:${detail.node.key}`, detail.node.key);
  const key = detail.node.key;
  const forced = detail.overrides?.force_include;
  if (forced == null) {
    return null;
  }
  return (
    <div className="stack" data-testid="force-include">
      <span className="row">
        <span>Included anyway: {forced}</span>
        <Button disabled={write.disabled} onClick={() => void write.run([{ op: "remove_override", node: key, kind: "force_include" }])}>
          Lift
        </Button>
      </span>
      <Rejected view={view} write={write} />
    </div>
  );
}

/** B5: the node's weight in this journey; empty restores the kind's default. */
export function WeightEditor({ view, detail }: { view: Ready; detail: NodeDetail }) {
  const write = useNodeWrite(view, `weight:${detail.node.key}`, detail.node.key);
  const key = detail.node.key;
  const weightForm = useFormDraft<string>(write.journey, key, "weight");
  const weight = detail.node.weight;
  return (
    <div className="stack" data-testid="weight">
      <span className="row">
        <span>Weight: {weight == null ? "the kind's default" : weight}</span>
        <DraftForm
          write={write}
          draft={weightForm}
          form="weight"
          start={weight == null ? "1" : String(weight)}
          label="Change the weight"
          input={(value, change) => (
            <Field type="number" min="0" step="any" aria-label="Weight" value={value} onChange={(event) => { change(event.target.value); }} />
          )}
          mutations={(value) => [{ op: "set_node_field", node: key, value: { weight: Number(value) } }]}
        />
        {weight == null ? null : (
          <Button disabled={write.disabled} onClick={() => void write.run([{ op: "set_node_field", node: key, value: { weight: null } }])}>
            Use the default
          </Button>
        )}
      </span>
      <Rejected view={view} write={write} />
    </div>
  );
}

/** B5, E2: set a kind's participation to explicit entities on this node, or restore inheritance. */
export function ParticipationEditor({ view, detail }: { view: Ready; detail: NodeDetail }) {
  const write = useNodeWrite(view, `participation:${detail.node.key}`, detail.node.key);
  const key = detail.node.key;
  const form = useFormDraft<{ kind: string; chosen: string[] }>(write.journey, key, "participation");
  const kinds = ["k_owner", ...(view.journey.graph.participation_kinds ?? []).map((each) => each.key)];
  const own = Object.keys(nodeOf(view, key)?.participations ?? {});
  const entities = view.inputs.deployment.entities ?? [];
  const draft = form.draft;
  const save = async (kind: string, chosen: string[], seen: Seen) => {
    if (await write.run([{ op: "set_participation", node: key, kind, source: chosen }], seen)) {
      form.close();
    }
  };
  return (
    <div className="stack" data-testid="participation-editor">
      <span className="row">
        <select
          aria-label="Participation kind"
          value={draft?.value.kind ?? ""}
          onChange={(event) => { form.open({ kind: event.target.value, chosen: [] }, draft ?? write.seen); }}
        >
          <option value="">Set a participation</option>
          {kinds.map((each) => (
            <option key={each} value={each}>
              {each.replace(/^k_/, "")}
            </option>
          ))}
        </select>
        {own.map((each) => (
          <Button key={each} disabled={write.disabled} onClick={() => void write.run([{ op: "clear_participation", node: key, kind: each }])}>
            Inherit {each.replace(/^k_/, "")} again
          </Button>
        ))}
      </span>
      {draft === undefined || draft.value.kind === "" ? null : (
        <span className="row">
          {entities.map((entity) => (
            <label key={entity.key}>
              <input
                type="checkbox"
                checked={draft.value.chosen.includes(entity.key)}
                onChange={() => {
                  const { chosen } = draft.value;
                  form.change({ ...draft.value, chosen: chosen.includes(entity.key) ? chosen.filter((each) => each !== entity.key) : [...chosen, entity.key] });
                }}
              />{" "}
              {entityName(view, entity.key)}
            </label>
          ))}
          <Button primary disabled={write.disabled} onClick={() => void save(draft.value.kind, draft.value.chosen, draft)}>
            Save
          </Button>
          <Button onClick={() => { form.close(); }}>Cancel</Button>
        </span>
      )}
      <Rejected view={view} write={write} />
    </div>
  );
}
