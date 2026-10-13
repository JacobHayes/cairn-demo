// Initialized before boot creates the API client or instantiates the wasm module.
import * as Sentry from "@sentry/react";
import { wasmIntegration } from "@sentry/wasm";
import { useEffect } from "react";
import { createRoutesFromChildren, matchRoutes, useLocation, useNavigationType } from "react-router";

export const featureFlags = Sentry.featureFlagsIntegration();

export function initTelemetry(): void {
  const dsn = import.meta.env["VITE_SENTRY_DSN"] as string | undefined;
  Sentry.init({
    dsn,
    enabled: Boolean(dsn),
    environment: import.meta.env["VITE_SENTRY_ENVIRONMENT"] as string | undefined,
    release: import.meta.env["VITE_SENTRY_RELEASE"] as string,
    dataCollection: { userInfo: true },
    integrations: [
      Sentry.reactRouterBrowserTracingIntegration({ useEffect, useLocation, useNavigationType, createRoutesFromChildren, matchRoutes }),
      Sentry.replayIntegration(),
      Sentry.feedbackIntegration({ colorScheme: "system" }),
      Sentry.browserProfilingIntegration(),
      featureFlags,
      wasmIntegration(),
    ],
    tracesSampleRate: Number(import.meta.env["VITE_SENTRY_TRACES_SAMPLE_RATE"] ?? 1),
    replaysSessionSampleRate: 1,
    replaysOnErrorSampleRate: 1,
    profileSessionSampleRate: 1,
    profileLifecycle: "trace",
  });
}
