// A19 in the Library: a retired route is listed like any other, its description optional, so
// one with none renders its row rather than taking the screen down to the load-failure view.
import { renderToStaticMarkup } from "react-dom/server";
import { MemoryRouter } from "react-router";
import { describe, expect, it, vi } from "vitest";

import type { RouteSummary } from "../data/reads.ts";
import { LoadFailure } from "../shell/LoadFailure.tsx";
import { Library } from "./Library.tsx";

const live = vi.hoisted(() => ({ routes: [] as unknown[] }));

vi.mock("../data/react.ts", () => ({
  useLive: (key: string) => ({
    view: key === "routes" ? { status: "ready", value: live.routes } : { status: "ready", value: {} },
    refetch: () => undefined,
  }),
}));

vi.mock("./NewRoute.tsx", () => ({ NewRoute: () => null }));

function library(routes: RouteSummary[]): string {
  live.routes = routes;
  return renderToStaticMarkup(
    <MemoryRouter initialEntries={["/library"]}>
      <LoadFailure screen="library">
        <Library />
      </LoadFailure>
    </MemoryRouter>,
  );
}

describe("the Library's route rows", () => {
  it("renders a retired route with no description instead of the load-failure view", () => {
    const html = library([{ header: { id: "r_old", name: "Old", kind: "process", retired: true }, draft_open: false, latest_version: 1, revision: 3 }]);
    expect(html).toContain('data-route="r_old"');
    expect(html).toContain('data-testid="retired"');
    expect(html).not.toContain('data-testid="load-failure"');
  });

  it("renders a retired route whose description is null", () => {
    const html = library([{ header: { id: "r_null", name: "Null", kind: "process", retired: true, description: null }, draft_open: false, latest_version: 1, revision: 1 }]);
    expect(html).toContain('data-route="r_null"');
    expect(html).not.toContain('data-testid="load-failure"');
  });

  it("previews the first line of a retired route's description, trimmed", () => {
    const html = library([{ header: { id: "r_desc", name: "Desc", kind: "process", retired: true, description: "  First line\nSecond line" }, draft_open: false, latest_version: 1, revision: 1 }]);
    expect(html).toContain(">First line<");
    expect(html).not.toContain("Second line<");
  });
});
