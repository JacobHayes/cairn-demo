// The app's entry: starts the tab's session (boot.ts), then renders the shell and its
// screens under a path router: a screen's address is a real path, and the server answers
// every path it does not keep for itself (`/api/`, `/.well-known/`, `/healthz`) with this
// page, so a deep link or a reload opens its screen.
import "@design/tokens.css";
import "@design/base.css";
import "./ui/app.css";

import * as Sentry from "@sentry/react";
import { initTelemetry } from "./telemetry.ts";

import { lazy, StrictMode } from "react";
import { createRoot } from "react-dom/client";
import { BrowserRouter, Navigate, Route, Routes } from "react-router";

import { boot } from "./boot.ts";
import { LayoutsContext } from "./canvas/hooks.ts";
import { Layouts, LayoutWorker } from "./canvas/layouts.ts";
import { SessionContext } from "./data/react.ts";
import type { Session } from "./data/session.ts";
import { Home } from "./journeys/Home.tsx";
import { JourneyIndex } from "./journeys/JourneyIndex.tsx";
import { BarePageRoute, JourneyDeepLink, JourneyLanding, JourneyPageRoute, LegacyRoute, RouteNodeRedirect, SummaryRoute } from "./screens/JourneyRoutes.tsx";
import { Shell } from "./shell/Shell.tsx";

initTelemetry();
const SentryRoutes = Sentry.wrapReactRouterRouting(Routes);

const MineScreen = lazy(() => import("./journeys/MineScreen.tsx").then((module) => ({ default: module.MineScreen })));
const NewJourney = lazy(() => import("./journeys/NewJourney.tsx").then((module) => ({ default: module.NewJourney })));
const Entities = lazy(() => import("./people/Entities.tsx").then((module) => ({ default: module.Entities })));
const Identity = lazy(() => import("./people/Identity.tsx").then((module) => ({ default: module.Identity })));
const ProposalScreen = lazy(() => import("./proposals/ProposalScreen.tsx").then((module) => ({ default: module.ProposalScreen })));
const Library = lazy(() => import("./routes/Library.tsx").then((module) => ({ default: module.Library })));
const RouteDetailPage = lazy(() => import("./routes/RouteDetail.tsx").then((module) => ({ default: module.RouteDetailPage })));
const RouteCanvasPage = lazy(() => import("./screens/RouteCanvasPage.tsx").then((module) => ({ default: module.RouteCanvasPage })));

function App({ session, layouts }: { session: Session; layouts: Layouts }) {
  return (
    <SessionContext value={session}>
      <LayoutsContext value={layouts}>
        <BrowserRouter>
          <Sentry.ErrorBoundary fallback={<main className="stack panel" role="alert"><h1>This screen could not open</h1><p className="muted">Reload to return to your work.</p><button type="button" onClick={() => { location.reload(); }}>Reload</button></main>}>
          <SentryRoutes>
            <Route element={<Shell />}>
              <Route index element={<Home />} />
              <Route path="journeys" element={<JourneyIndex />} />
              <Route path="mine" element={<MineScreen />} />
              <Route path="mine/:id" element={<Navigate replace to="/mine" />} />
              <Route path="mine/:id/nodes/:key" element={<MineScreen />} />
              <Route path="new" element={<NewJourney />} />
              <Route path="journeys/:id" element={<JourneyLanding />} />
              <Route path="journeys/:id/nodes/:key" element={<JourneyDeepLink />} />
              <Route path="journeys/:id/next/:projection" element={<JourneyPageRoute page="next" />} />
              <Route path="journeys/:id/next/:projection/nodes/:key" element={<JourneyPageRoute page="next" />} />
              <Route path="journeys/:id/plan" element={<BarePageRoute page="plan" />} />
              <Route path="journeys/:id/plan/:projection" element={<JourneyPageRoute page="plan" />} />
              <Route path="journeys/:id/plan/:projection/nodes/:key" element={<JourneyPageRoute page="plan" />} />
              <Route path="journeys/:id/plan/:projection/edges/:edge" element={<JourneyPageRoute page="plan" />} />
              <Route path="journeys/:id/summary" element={<SummaryRoute />} />
              <Route path="journeys/:id/summary/nodes/:key" element={<SummaryRoute />} />
              <Route path="journeys/:id/:old" element={<LegacyRoute />} />
              <Route path="journeys/:id/:old/nodes/:key" element={<LegacyRoute />} />
              <Route path="library" element={<Library />} />
              <Route path="routes" element={<LegacyRoute />} />
              <Route path="routes/:id" element={<RouteCanvasPage />} />
              <Route path="routes/:id/draft" element={<RouteCanvasPage />} />
              <Route path="routes/:id/draft/nodes/:key" element={<RouteCanvasPage />} />
              <Route path="routes/:id/nodes/:key" element={<RouteNodeRedirect />} />
              <Route path="routes/:id/versions" element={<RouteDetailPage />} />
              <Route path="entities" element={<Entities />} />
              <Route path="me" element={<Identity />} />
              <Route path="proposals/:id" element={<ProposalScreen />} />
              <Route path="proposals/:id/nodes/:key" element={<ProposalScreen />} />
            </Route>
          </SentryRoutes>
          </Sentry.ErrorBoundary>
        </BrowserRouter>
      </LayoutsContext>
    </SessionContext>
  );
}

const container = document.getElementById("root");
if (container === null) {
  throw new Error("index.html has no #root");
}
// React's root hooks allow an explicitly undefined stack; Sentry's ErrorInfo uses null.
const reportReactError = Sentry.reactErrorHandler();
const reactErrorHandler = (error: unknown, info: { componentStack?: string | undefined }) => {
  reportReactError(error, { componentStack: info.componentStack ?? null });
};
const root = createRoot(container, {
  onUncaughtError: reactErrorHandler,
  onCaughtError: reactErrorHandler,
  onRecoverableError: reactErrorHandler,
});
boot().then(
  (session) => {
    root.render(
      <StrictMode>
        <App session={session} layouts={new Layouts(new LayoutWorker())} />
      </StrictMode>,
    );
  },
  (thrown: unknown) => {
    Sentry.captureException(thrown);
    root.render(
      <p className="callout callout-bad" role="alert">
        Cairn could not start: {thrown instanceof Error ? thrown.message : String(thrown)}
      </p>,
    );
  },
);
