// The shared write path every screen uses: a patch drafted against the revision its author
// saw, with a fresh client-generated id (H5), submitted through the client wrapper's safe
// retry with the page engine's touched-set check, its save recorded with the warnings it caused (D7). Version skew
// stops it before anything is sent, and stops automatic retries mid-way (ARCHITECTURE, Web
// UI). Committed state always comes from the host: the tick that follows refetches it.
import * as Sentry from "@sentry/react";
import { submit, type HttpFailure, type Schema } from "@cairn/client";

import type { Host, Markdown, Patch } from "./host.ts";
import { consequenceLines, warningOf, type Activity, type ConsequenceLine, type TitleOf, type Unlocked } from "./activity.ts";
import type { SkewLatch } from "./skew.ts";
import type { SyncStatus } from "./sync.ts";

export type Mutation = Schema<"Mutation">;
export type Rejection = Schema<"Rejection">;

/** A write as a screen asks for it. */
export interface WriteIntent {
  target: Patch["target"];
  /** The target's revision the author saw when they started this edit. */
  baseRevision: number;
  /** The deployment revision it was validated against, for a patch writing an entity reference (E6). */
  deploymentRevision?: number;
  mutations: Mutation[];
  note?: Markdown;
}

export type WriteResult =
  | { outcome: "landed"; answer: Schema<"PatchAnswer">; resubmitted: number; warning: string | undefined }
  | { outcome: "rejected"; rejection: Rejection; resubmitted: number }
  | { outcome: "failed"; failure: HttpFailure }
  | { outcome: "stopped" };

/** A fresh patch id (H5): `p_` and 32 hex digits from the browser's random UUID. */
export function newPatchId(): string {
  return `p_${crypto.randomUUID().replaceAll("-", "")}`;
}

export function patchOf(intent: WriteIntent, id: string = newPatchId()): Patch {
  const patch: Patch = {
    id,
    target: intent.target,
    base_revision: intent.baseRevision,
    mutations: intent.mutations,
  };
  return intent.deploymentRevision === undefined ? patch : { ...patch, deployment_revision: intent.deploymentRevision };
}

/** What the chip says of a write that got no answer: it may have landed, so it is not called unsent. */
export function unconfirmed(failure: HttpFailure): string {
  return `Could not confirm the save: ${failure.message}`;
}

/** Why a write was rejected, in a sentence for the chip's popover. */
export function reasonOf(rejection: Rejection): string {
  switch (rejection.rejection) {
    case "stale":
      return "Someone changed this since you opened it.";
    case "invalid": {
      const [first] = rejection.violations;
      const more = rejection.violations.length - 1;
      // The engine ends a message with the rule's code, "(F5)": for the docs, not for people.
      return first === undefined ? "The change breaks a rule." : `${first.message.replace(/\s*\([A-Z]\d+\)$/, "")}${more > 0 ? ` (and ${String(more)} more)` : ""}`;
    }
    case "patch_id_reused":
      return "That change was already sent with different content.";
  }
}

/** The journey a write targets, if it targets one. */
function journeyOf(intent: WriteIntent): string | undefined {
  return typeof intent.target === "object" && "journey" in intent.target ? intent.target.journey : undefined;
}

/** The node a mutation is about: the decision it answers, the node it acts on, or the one its annotation is on. */
function subjectOf(mutation: Mutation): string | undefined {
  if ("decision" in mutation) {
    return mutation.decision;
  }
  if ("annotation" in mutation && typeof mutation.annotation === "object") {
    return mutation.annotation.node ?? undefined;
  }
  return "node" in mutation && typeof mutation.node === "string" ? mutation.node : undefined;
}

/**
 * D7: the node a landed write acted on (the first mutation about one: an evidence annotation or
 * a new entity may come before the action) and what it unlocked, for a pass over the acting
 * frontier; undefined when the write is about no node.
 */
export function unlockedOf(intent: WriteIntent, answer: Schema<"PatchAnswer">, began: number): Unlocked | undefined {
  const journey = journeyOf(intent);
  const by = intent.mutations.map(subjectOf).find((node) => node !== undefined);
  if (journey === undefined || by === undefined) {
    return undefined;
  }
  const nodes = answer.outcome === "applied" ? (answer.consequences?.[journey]?.unlocked ?? []) : [];
  return { journey, by, nodes, began };
}

/** What a write does, in words for the popover's Recent: "Answered Who runs testing?". */
export function describe(intent: WriteIntent, titleOf: TitleOf): string {
  const journey = journeyOf(intent);
  const [first] = intent.mutations;
  if (first === undefined) {
    return "Saved a change";
  }
  const node = subjectOf(first);
  const title = node === undefined || journey === undefined ? undefined : titleOf(journey, node);
  const of = (verb: string, fallback: string) => (title === undefined ? `${verb} ${fallback}` : `${verb} ${title}`);
  const rest = intent.mutations.length > 1 ? ` and ${String(intent.mutations.length - 1)} more` : "";
  switch (first.op) {
    case "answer":
      return `${of("Answered", "a decision")}${rest}`;
    case "transition": {
      const move = typeof first.transition === "string" ? first.transition : "skip";
      const verb = { start: "Started", stop: "Stopped", complete: "Completed", skip: "Skipped", reopen: "Reopened", reach: "Reached" }[move];
      return `${of(verb, "a step")}${rest}`;
    }
    case "snooze":
      return `${of("Snoozed", "a step")}${rest}`;
    case "unsnooze":
      return `${of("Unsnoozed", "a step")}${rest}`;
    case "set_pin":
    case "shift_pin":
    case "clear_pin":
      return `${of("Changed the date of", "a step")}${rest}`;
    default:
      return `${title === undefined ? "Edited" : `Edited ${title}`}${rest}`;
  }
}

/** The stores a write reports to. */
export interface WriteEnv {
  host: Host;
  skew: SkewLatch;
  activity: Activity;
  sync: SyncStatus;
  titleOf: TitleOf;
}

/**
 * Records a save of this tab: in Recent with the warning sentence its consequences call for
 * (D7), and on the chip as SAVED. The warning is what a receipt shows.
 */
export function recordSave(env: Pick<WriteEnv, "activity" | "sync" | "titleOf">, text: string, lines: ConsequenceLine[], unlocked?: Unlocked): string | undefined {
  const warning = warningOf(lines, env.titleOf);
  const at = new Date();
  env.activity.saved({ at, text, warning, unlocked });
  env.sync.saved(at.toLocaleTimeString([], { hour12: false }));
  return warning;
}

/**
 * Submits `intent` through the host, counted in flight while it is out. A landed write is
 * recorded as a save with its warning sentence (D7); one that never got an answer is a
 * problem on the chip until the next attempt at the same target lands or it is discarded.
 */
export async function write(env: WriteEnv, intent: WriteIntent): Promise<WriteResult> {
  const { host, skew, sync, titleOf } = env;
  const failedKey = `failed:${JSON.stringify(intent.target)}`;
  if (skew.latched) {
    sync.problem(failedKey, {
      kind: "failed",
      message: "Cairn was updated, so this change was not sent. Reload; your unsent edits are kept.",
      label: undefined,
      address: undefined,
      discard: () => {
        sync.resolve(failedKey);
      },
    });
    return { outcome: "stopped" };
  }
  const began = env.activity.begin();
  const started = performance.now();
  const submitted = await sync.track(
    submit(patchOf(intent), (patch) => host.send(patch, intent.note), {
      overlaps: host.overlaps,
      mayRetry: () => !skew.latched,
    }),
  );
  Sentry.metrics.count("cairn.write", 1, { attributes: { outcome: submitted.outcome } });
  Sentry.metrics.distribution("cairn.write.duration", performance.now() - started, { unit: "millisecond" });
  if (submitted.outcome === "failed") Sentry.logger.error("Write failed", { message: submitted.error.message, status: submitted.error.status });
  else Sentry.logger.info("Write answered", { outcome: submitted.outcome });
  switch (submitted.outcome) {
    case "landed": {
      const warning = recordSave(env, describe(intent, titleOf), consequenceLines(submitted.answer), unlockedOf(intent, submitted.answer, began));
      sync.resolve(failedKey);
      return { ...submitted, warning };
    }
    case "rejected":
      return submitted;
    case "failed":
      sync.problem(failedKey, {
        kind: "failed",
        message: unconfirmed(submitted.error),
        label: undefined,
        address: undefined,
        discard: () => {
          sync.resolve(failedKey);
        },
      });
      return { outcome: "failed", failure: submitted.error };
  }
}
