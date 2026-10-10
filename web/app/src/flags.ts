// Small UI experiments. URL choices persist across navigation, and evaluations accompany issues.
import { featureFlags } from "./telemetry.ts";

const defaults = { "graph-ladder-v2": false, "next-list-compact": false, "inspector-autosave": false };
export type Flag = keyof typeof defaults;

export function flag(name: Flag): boolean {
  let value = defaults[name];
  try {
    const stored = localStorage.getItem(`cairn:flag:${name}`);
    if (stored !== null) value = stored === "true";
    for (const choice of new URLSearchParams(location.search).getAll("flag").flatMap((choice) => choice.split(","))) {
      const [key, setting] = choice.split(":");
      if (key === name || key === `!${name}`) {
        value = key === name && setting !== "false";
        localStorage.setItem(`cairn:flag:${name}`, String(value));
      }
    }
  } catch {
    // Storage is optional in a private or embedded browser.
  }
  featureFlags.addFeatureFlag(name, value);
  return value;
}
