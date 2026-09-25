import assert from "node:assert/strict";
import { test } from "vitest";
import vm from "node:vm";
import { componentSourceForTest } from "../../../lib/vm-component-harness";
import "../../../test/vm-tsx-setup";

const ANIMATED_SRC = "/assets/naomi-typing-50px.webp";
const STILL_SRC = "/assets/naomi-typing-50px-still.webp";

// Load the presentational component the same way connection-status.test.ts does:
// strip imports, expose the function, and run it in a VM. The vm-tsx-setup shim
// transpiles the JSX into inspectable `{ type, props, children }` nodes.
function loadNearProcessIndicator() {
  const context: vm.Context = { globalThis: {} };
  vm.runInNewContext(
    componentSourceForTest(
      new URL("./near-process-indicator.tsx", import.meta.url),
      "NearProcessIndicator",
    ),
    context,
  );
  return context.globalThis.__testExports.NearProcessIndicator;
}

function findNode(value, predicate, seen = new Set()) {
  if (!value || typeof value !== "object" || seen.has(value)) return null;
  seen.add(value);
  if (Array.isArray(value)) {
    for (const candidate of value) {
      const match = findNode(candidate, predicate, seen);
      if (match) return match;
    }
    return null;
  }
  if (predicate(value)) return value;
  for (const key of ["children", "values"]) {
    const candidates = Array.isArray(value[key]) ? value[key] : [];
    for (const candidate of candidates) {
      const match = findNode(candidate, predicate, seen);
      if (match) return match;
    }
  }
  return null;
}

const byClass = (name) => (node) => node.props?.className === name;
const byType = (type) => (node) => node.type === type;

// The icon is decorative: the label beside it carries the state, so the image
// must stay hidden from assistive tech, as the old SVG was.
function assertDecorativeImage(img) {
  assert.notEqual(img, null, "the indicator image should render");
  assert.equal(img.props.alt, "");
  assert.equal(img.props["aria-hidden"], "true");
  assert.equal(img.props.className, "near-process-icon");
  assert.equal(img.props.width, 28);
  assert.equal(img.props.height, 28);
}

test("NearProcessIndicator working state shows the animated typing image with elapsed time", () => {
  const NearProcessIndicator = loadNearProcessIndicator();
  const rendered = NearProcessIndicator({
    state: "working",
    label: "Working…",
    elapsed: "0:03",
  });

  assert.match(rendered.props.className, /\bnear-process\b/);
  assert.match(rendered.props.className, /\bis-busy\b/);

  // The animated webp plays by default; readers who ask for reduced motion get
  // the still first frame instead, through a <picture> media source.
  const picture = findNode(rendered, byType("picture"));
  assert.notEqual(picture, null, "working state wraps the image in <picture>");
  const source = findNode(picture, byType("source"));
  assert.notEqual(source, null, "reduced-motion source should render");
  assert.equal(source.props.media, "(prefers-reduced-motion: reduce)");
  assert.equal(source.props.srcSet, STILL_SRC);
  const img = findNode(picture, byType("img"));
  assertDecorativeImage(img);
  assert.equal(img.props.src, ANIMATED_SRC);

  // The old inline SVG mark is gone.
  assert.equal(findNode(rendered, byType("svg")), null);

  // State-scoped CSS makes the working label strong; elapsed is shown beside it.
  const label = findNode(rendered, byClass("near-process-label"));
  assert.notEqual(label, null, "working label uses the shared label class");
  assert.equal(label.children[0], "Working…");
  const elapsed = findNode(rendered, byClass("near-process-elapsed"));
  assert.notEqual(elapsed, null, "elapsed should render while working");
  assert.equal(elapsed.children[0], "0:03");
});

test("NearProcessIndicator done state is the still first frame with a muted label", () => {
  const NearProcessIndicator = loadNearProcessIndicator();
  const rendered = NearProcessIndicator({
    state: "done",
    label: "Done",
    elapsed: "0:03",
  });

  assert.match(rendered.props.className, /\bis-done\b/);

  // A finished run never animates, so there is no <picture> or animated source.
  assert.equal(findNode(rendered, byType("picture")), null);
  const img = findNode(rendered, byType("img"));
  assertDecorativeImage(img);
  assert.equal(img.props.src, STILL_SRC);
  assert.equal(findNode(rendered, byType("svg")), null);

  const label = findNode(rendered, byClass("near-process-label"));
  assert.notEqual(label, null, "done label uses the shared label class");
  assert.equal(label.children[0], "Done");
  assert.equal(
    findNode(rendered, byClass("near-process-elapsed")),
    null,
    "elapsed only shows while working",
  );
});
