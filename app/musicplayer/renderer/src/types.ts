/** The shape the Rust `player.state()` snapshot arrives in. */
export interface PlayerState {
  /** False when no output device could be opened. Every control is inert. */
  readonly hasDevice: boolean;
  /** True while a track is loaded and not paused. */
  readonly playing: boolean;
  /** Index into the library, or null when nothing is loaded. */
  readonly index: number | null;
  /** Playback position in seconds. */
  readonly position: number;
  /** Track length in seconds, when the decoder reported one. */
  readonly duration: number | null;
  /** Output gain, 0..1. */
  readonly volume: number;
  /** The most recent failure, or null. */
  readonly error: string | null;
}

/** One row in the playlist. */
export interface LibraryTrack {
  readonly title: string;
  readonly album: string;
}

/** The state before Rust has sent its first snapshot. */
export const IDLE_STATE: PlayerState = {
  hasDevice: false,
  playing: false,
  index: null,
  position: 0,
  duration: null,
  volume: 0,
  error: null,
};

/**
 * Formats seconds as `m:ss`.
 *
 * Returns an em dash rather than `0:00` for an unknown length: a stream whose
 * duration the decoder could not report is not a zero-length track, and showing
 * one makes the scrubber look broken.
 */
export function formatTime(seconds: number | null | undefined): string {
  if (seconds === null || seconds === undefined || !Number.isFinite(seconds)) return "—";
  const total = Math.max(0, Math.floor(seconds));
  const minutes = Math.floor(total / 60);
  const rest = total % 60;
  return `${minutes}:${rest.toString().padStart(2, "0")}`;
}
