// Single source of truth for colors consumed by canvas/JS code. CSS owns the palette
// (the :root token block in styles.css); JS reads it once per load so the two can't
// drift. Every getter carries a literal fallback so a missing stylesheet (or a token
// typo) degrades to the current look instead of invisible ink.

const cache = new Map();

export function cssVar(name, fallback = "") {
  if (!cache.has(name)) {
    const v = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
    cache.set(name, v || fallback);
  }
  return cache.get(name) || fallback;
}

// AI pipeline-stage colors (timeline ribbons + tooltip chips).
export function stageColors() {
  return {
    done: cssVar("--stage-done", "#3b7d6e"),
    processing: cssVar("--stage-processing", "#3f6fb0"),
    pending: cssVar("--stage-pending", "#3a3f4a"),
    error: cssVar("--stage-error", "#c0473d"),
    skipped: cssVar("--stage-skipped", "#556058"),
  };
}

// Detection-overlay box colors (people get a hashed hue when identified — that stays in JS).
export function detColors() {
  return {
    object: cssVar("--det-object", "#2ee6d6"),
    plate: cssVar("--det-plate", "#ffd24a"),
    personUnknown: cssVar("--det-person-unknown", "#ffae57"),
  };
}

// Event severity (timeline markers, chips drawn on canvas).
export function severityColors() {
  return {
    info: cssVar("--sev-info", "#9fb3c8"),
    warning: cssVar("--sev-warning", "#ffae57"),
    critical: cssVar("--sev-critical", "#ff7a7a"),
  };
}

// Dashboard chart series palette, in series order.
export function chartPalette() {
  return [
    cssVar("--chart-1", "#e0563f"),
    cssVar("--chart-2", "#d9a441"),
    cssVar("--chart-3", "#3f8ee0"),
    cssVar("--chart-4", "#5fb56a"),
    cssVar("--chart-5", "#9b6fd4"),
  ];
}

// Live check (not cached): rAF-driven animations should re-read this each pass.
export function prefersReducedMotion() {
  return window.matchMedia("(prefers-reduced-motion: reduce)").matches;
}
