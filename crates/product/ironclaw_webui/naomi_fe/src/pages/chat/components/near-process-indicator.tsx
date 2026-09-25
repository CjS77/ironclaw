// Naomi typing at her laptop: a 50px animated webp while the agent works, and
// its first frame (extracted byte-for-byte, no re-encode) as the still used for
// finished runs and for readers who ask for reduced motion. An animated image
// cannot be paused from CSS, so the still has to be a separate file.
const TYPING_ANIMATED_SRC = "/assets/naomi-typing-50px.webp";
const TYPING_STILL_SRC = "/assets/naomi-typing-50px-still.webp";

// Display size in CSS pixels. Kept in step with `.near-process-icon` in
// styles/app.css; the attributes reserve the box before the image loads.
const TYPING_ICON_SIZE = 28;

type NearProcessIndicatorProps = {
  state: "working" | "done";
  label: string;
  elapsed?: string;
};

// Presentational live/working indicator: the animated typing image while the
// agent works, resolving to its still first frame when done. Left-aligned icon
// + label, no container box (see the "live status line" in the agent-activity
// mockup). The image is decorative; the label carries the state.
export function NearProcessIndicator({
  state,
  label,
  elapsed,
}: NearProcessIndicatorProps) {
  const working = state === "working";

  return (
    <div className={`near-process ${working ? "is-busy" : "is-done"}`}>
      {working ? (
        <picture>
          <source
            media="(prefers-reduced-motion: reduce)"
            srcSet={TYPING_STILL_SRC}
          />
          <img
            className="near-process-icon"
            src={TYPING_ANIMATED_SRC}
            width={TYPING_ICON_SIZE}
            height={TYPING_ICON_SIZE}
            alt=""
            aria-hidden="true"
          />
        </picture>
      ) : (
        <img
          className="near-process-icon"
          src={TYPING_STILL_SRC}
          width={TYPING_ICON_SIZE}
          height={TYPING_ICON_SIZE}
          alt=""
          aria-hidden="true"
        />
      )}
      <span className="near-process-label">{label}</span>
      {working && elapsed ? (
        <span className="near-process-elapsed">{elapsed}</span>
      ) : null}
    </div>
  );
}
