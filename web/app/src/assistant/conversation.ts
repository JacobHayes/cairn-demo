// One conversation as the panel shows it (I5), held per host and target for the tab rather
// than by a mounted panel: what the host keeps, the tab's kept turns beside it, a turn in
// flight, and any turn the host answered but did not keep. A panel closed while a turn runs
// shows its answer when opened again, and a read begun before an answer arrived never
// replaces the answer.
import * as Sentry from "@sentry/react";
import type { HttpFailure } from "@cairn/client";

import { targetKey, type AssistantHost, type AssistantTarget, type Conversation } from "../data/assistant.ts";
import { readDraft, writeDraft } from "../data/drafts.ts";
import { Emitter } from "../data/emitter.ts";
import { answeredEntries, holdsTurn, keeping, keptTurn, latestAnswerAt, type KeptTurn, type Unsaved } from "./model.ts";

export type Read = { status: "loading" } | { status: "failed"; message: string } | { status: "ready"; conversation: Conversation };

/** What the panel shows of one conversation. */
export interface ConversationView {
  read: Read;
  kept: readonly KeptTurn[];
  unsaved: readonly Unsaved[];
  /** The words of the turn in flight, if one is. */
  sending: string | undefined;
  /** Why the last turn was not answered, or its conversation not read again. */
  failure: HttpFailure | undefined;
}

function message(thrown: unknown): string {
  return thrown instanceof Error ? thrown.message : String(thrown);
}

/** The tab's kept turns of conversation `id` (session storage, beside the drafts). */
function keptKey(id: string): string {
  return `assistant-turns:${id}`;
}

function keptOf(conversation: string): KeptTurn[] {
  return readDraft<KeptTurn[]>(keptKey(conversation)) ?? [];
}

export class AssistantConversation extends Emitter {
  readonly #assistant: AssistantHost;
  readonly #target: AssistantTarget;
  #view: ConversationView = { read: { status: "loading" }, kept: [], unsaved: [], sending: undefined, failure: undefined };
  /** Bumped by each read begun and each answer installed: an older read is dropped. */
  #generation = 0;

  constructor(assistant: AssistantHost, target: AssistantTarget) {
    super();
    this.#assistant = assistant;
    this.#target = target;
  }

  get view(): ConversationView {
    return this.#view;
  }

  #set(change: Partial<ConversationView>): void {
    this.#view = { ...this.#view, ...change };
    this.emit();
  }

  /** Reads the conversation again, as the panel opens. */
  refresh(): void {
    this.#generation += 1;
    const generation = this.#generation;
    this.#assistant.conversation(this.#target).then(
      (conversation) => {
        if (generation === this.#generation) {
          this.#set({ read: { status: "ready", conversation }, kept: keptOf(conversation.conversation) });
        }
      },
      (thrown: unknown) => {
        if (generation === this.#generation) {
          this.#set({ read: { status: "failed", message: message(thrown) } });
        }
      },
    );
  }

  /** I5: sends `text` as one turn; answers whether the host answered it. */
  async send(text: string): Promise<boolean> {
    if (this.#view.sending !== undefined) {
      return false;
    }
    this.#set({ sending: text, failure: undefined });
    Sentry.logger.info("Assistant turn started");
    const turned = await this.#assistant.turn(this.#target, text);
    Sentry.metrics.count("cairn.assistant.turn", 1, { attributes: { outcome: turned.outcome } });
    if (turned.outcome === "failed") Sentry.logger.error("Assistant turn failed", { message: turned.error.message });
    else Sentry.logger.info("Assistant turn answered", { ended: turned.reply.ended });
    if (turned.outcome === "failed") {
      this.#set({ sending: undefined, failure: turned.error });
      return false;
    }
    const { reply } = turned;
    const held = this.#view.read.status === "ready" ? this.#view.read.conversation.messages.length : 0;
    const unsaved = (after: number): Unsaved[] => [...this.#view.unsaved, { after, entries: answeredEntries(text, reply, new Date().toISOString()) }];
    this.#generation += 1;
    try {
      const after = await this.#assistant.conversation(this.#target);
      this.#generation += 1;
      if (!holdsTurn(after.messages, text, reply)) {
        this.#set({ read: { status: "ready", conversation: after }, unsaved: unsaved(after.messages.length), sending: undefined });
        return true;
      }
      const at = latestAnswerAt(after.messages);
      const kept = at === undefined ? keptOf(after.conversation) : keeping(keptOf(after.conversation), keptTurn(at, reply));
      writeDraft(keptKey(after.conversation), kept);
      this.#set({ read: { status: "ready", conversation: after }, kept, sending: undefined });
    } catch (thrown) {
      const failure = { status: 0, message: `the reply arrived, but the conversation could not be read again (${message(thrown)})` };
      this.#set({ unsaved: unsaved(held), sending: undefined, failure });
    }
    return true;
  }
}

/** The tab's conversations, by host and target. */
const held = new WeakMap<AssistantHost, Map<string, AssistantConversation>>();

/** `target`'s conversation over `assistant`, the same one for the whole tab. */
export function conversationOf(assistant: AssistantHost, target: AssistantTarget): AssistantConversation {
  let byTarget = held.get(assistant);
  if (byTarget === undefined) {
    byTarget = new Map();
    held.set(assistant, byTarget);
  }
  const key = targetKey(target);
  let conversation = byTarget.get(key);
  if (conversation === undefined) {
    conversation = new AssistantConversation(assistant, target);
    byTarget.set(key, conversation);
  }
  return conversation;
}
