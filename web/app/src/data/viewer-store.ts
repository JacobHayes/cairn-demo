// The caller the tab holds (H3): the user, the identities they sign in with, and the
// entities their verified emails name, which "mine" covers together and which, when there
// are two or more, are offered for merging. Which entities hold which emails is the
// deployment's, so a newer deployment held (deployment-store.ts) refetches it (H6).
import * as Sentry from "@sentry/react";
import { Emitter } from "./emitter.ts";
import type { DeploymentStore } from "./deployment-store.ts";
import type { Host, Viewer } from "./host.ts";

export class ViewerStore extends Emitter {
  readonly #host: Host;
  #current: Viewer | undefined;
  #failed: string | undefined;
  #dirty = false;
  #running = false;

  constructor(host: Host, deployment: DeploymentStore) {
    super();
    this.#host = host;
    deployment.subscribe(() => {
      this.refetch();
    });
  }

  /** The caller, once fetched. */
  get current(): Viewer | undefined {
    return this.#current;
  }

  /** Why the caller could not be read, until a fetch answers. */
  get failed(): string | undefined {
    return this.#failed;
  }

  refetch(): void {
    this.#dirty = true;
    if (!this.#running) {
      void this.#run();
    }
  }

  async #run(): Promise<void> {
    this.#running = true;
    while (this.#dirty) {
      this.#dirty = false;
      try {
        const previous = this.#current?.user;
        this.#current = await this.#host.viewer();
        const viewer = this.#current;
        const email = viewer.identities.flatMap((identity) => identity.verified_emails)[0];
        const username = viewer.identities[0]?.subject;
        Sentry.setUser({ id: viewer.user, ...(username === undefined ? {} : { username }),
          ...(email === undefined ? {} : { email }) });
        if (previous !== viewer.user) Sentry.logger.info("Signed in", { user_id: viewer.user });
        this.#failed = undefined;
      } catch (thrown) {
        this.#failed = thrown instanceof Error ? thrown.message : String(thrown);
      }
      this.emit();
    }
    this.#running = false;
  }
}
