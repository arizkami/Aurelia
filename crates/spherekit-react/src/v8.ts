import type { ApiBridgeTransport } from "./bridge";

/**
 * The two globals the Rust host installs on the isolate before it evaluates
 * the bundle.
 *
 * They are declared as optional because the same bundle is expected to run in
 * a browser, in Bun's test runner, and inside the isolate; the transport is
 * what decides whether this particular global object is a SphereKit host, and
 * a hard `declare global` would make every other environment look broken to
 * TypeScript.
 */
interface V8HostGlobals {
  /** Sends one frame and returns a JSON array of reply frames, synchronously. */
  __spherekitSend?: (frame: string) => string;
  /** Installed by {@link createV8Transport} for the host to push events in. */
  __spherekitReceive?: (frame: string) => void;
}

/** Whether `global` is a SphereKit V8 isolate with the host globals installed. */
export function isV8Host(global: typeof globalThis = globalThis): boolean {
  return typeof (global as typeof globalThis & V8HostGlobals).__spherekitSend === "function";
}

/**
 * Creates the in-process transport for SphereKit's V8 isolate.
 *
 * The isolate has no sockets, no message ports and no event loop of its own,
 * so the usual asynchronous transport shape does not apply: `__spherekitSend`
 * is a synchronous native call that returns the frames the host produced while
 * handling the request. Those replies are fed straight back to the subscriber,
 * which is why an `invoke()` issued from an isolate can settle before the
 * calling function returns.
 *
 * Replies are buffered until a subscriber exists. `createApiBridge` subscribes
 * before it sends `hello`, so this only matters when the transport is driven
 * directly — but dropping the host's first frames because of call ordering
 * would be an infuriating bug to find from inside an isolate with no debugger.
 */
export function createV8Transport(global: typeof globalThis = globalThis): ApiBridgeTransport {
  const host = global as typeof globalThis & V8HostGlobals;
  const send = host.__spherekitSend;

  if (typeof send !== "function") {
    throw new Error(
      "SphereKit V8 transport unavailable: the embedder did not install the global " +
        "`__spherekitSend(frame: string): string` on the isolate before evaluating this bundle.",
    );
  }

  let listener: ((message: string) => void) | undefined;
  let buffered: string[] = [];
  let closed = false;

  const deliver = (frame: string): void => {
    for (const line of frame.split("\n")) {
      const message = line.trim();
      if (message.length === 0) continue;
      if (listener) listener(message);
      else buffered.push(message);
    }
  };

  return {
    send(message) {
      if (closed) return;
      const replies = send.call(host, message);
      for (const frame of parseReplies(replies)) deliver(frame);
    },

    subscribe(next) {
      listener = next;
      const pending = buffered;
      buffered = [];
      for (const message of pending) next(message);

      host.__spherekitReceive = (frame: string) => {
        deliver(frame);
      };

      return () => {
        listener = undefined;
        if (host.__spherekitReceive) delete host.__spherekitReceive;
      };
    },

    close() {
      closed = true;
      listener = undefined;
      buffered = [];
      if (host.__spherekitReceive) delete host.__spherekitReceive;
    },
  };
}

/**
 * Parses the reply value of `__spherekitSend`.
 *
 * A host that returns nothing at all is treated as "no replies", because a
 * native function that falls off its end is the most likely shape of a host
 * that simply had nothing to say. Anything else that is not an array of
 * strings is a protocol violation and is reported as one: the alternative is
 * a silently ignored frame and an `invoke()` that never settles.
 */
function parseReplies(replies: unknown): string[] {
  if (replies === undefined || replies === null || replies === "") return [];

  if (typeof replies !== "string") {
    throw new Error(
      `SphereKit V8 host returned ${typeof replies} from __spherekitSend; expected a JSON array of frames.`,
    );
  }

  let parsed: unknown;
  try {
    parsed = JSON.parse(replies);
  } catch {
    throw new Error("SphereKit V8 host returned a non-JSON reply from __spherekitSend.");
  }

  if (!Array.isArray(parsed) || parsed.some((frame) => typeof frame !== "string")) {
    throw new Error("SphereKit V8 host must return a JSON array of frame strings from __spherekitSend.");
  }

  return parsed as string[];
}
