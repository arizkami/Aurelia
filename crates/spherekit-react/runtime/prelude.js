// Minimal host environment for running bundled browser/React code in a bare V8 context.
//
// SphereKit's isolate starts from an empty context: ECMAScript and nothing
// else. There is no DOM, no Node, no timer queue and no console, so a bundle
// built for the browser has to be given the few globals it assumes exist
// before it is evaluated.
//
// Timers are the load-bearing part. React's scheduler reads setTimeout during
// module evaluation, so the bundle throws on import even when the application
// only ever renders through the synchronous path and never schedules a
// callback at all. Nothing here drives itself: the isolate has no event loop,
// so the host pumps due callbacks with __runTimers() once a frame, and console
// output accumulates for __drainConsole() rather than going to a stdout that
// does not exist.
(function (global) {
  "use strict";

  var timers = new Map();
  var nextTimer = 1;
  var queue = [];

  function schedule(callback, delay, args, repeat) {
    if (typeof callback !== "function") return 0;
    var id = nextTimer++;
    var entry = { id: id, callback: callback, args: args, due: now() + (delay || 0), repeat: repeat ? (delay || 0) : -1 };
    timers.set(id, entry);
    queue.push(entry);
    return id;
  }

  var start = Date.now();
  function now() {
    return Date.now() - start;
  }

  global.setTimeout = function (cb, delay) {
    return schedule(cb, delay, Array.prototype.slice.call(arguments, 2), false);
  };
  global.setInterval = function (cb, delay) {
    return schedule(cb, delay, Array.prototype.slice.call(arguments, 2), true);
  };
  global.clearTimeout = function (id) {
    timers.delete(id);
  };
  global.clearInterval = global.clearTimeout;
  global.setImmediate = function (cb) {
    return schedule(cb, 0, Array.prototype.slice.call(arguments, 1), false);
  };
  global.clearImmediate = global.clearTimeout;

  // Frame callbacks land in the same queue as the timers. A library that
  // probes for requestAnimationFrame and finds it missing usually falls back to
  // a 16ms setTimeout anyway, so pointing it at the queue the host already
  // pumps keeps both paths on one clock.
  global.requestAnimationFrame = function (cb) {
    return schedule(
      function () {
        cb(now());
      },
      0,
      [],
      false,
    );
  };
  global.cancelAnimationFrame = global.clearTimeout;

  if (typeof global.queueMicrotask !== "function") {
    var resolved = Promise.resolve();
    global.queueMicrotask = function (cb) {
      resolved.then(cb);
    };
  }

  global.performance = global.performance || { now: now, timeOrigin: start };

  var lines = [];
  function format(args) {
    var parts = [];
    for (var i = 0; i < args.length; i++) {
      var value = args[i];
      if (typeof value === "string") parts.push(value);
      else {
        try {
          parts.push(JSON.stringify(value));
        } catch (e) {
          parts.push(String(value));
        }
      }
    }
    return parts.join(" ");
  }
  function record(level) {
    return function () {
      lines.push(level + ": " + format(arguments));
    };
  }
  global.console = global.console || {};
  global.console.log = record("log");
  global.console.info = record("info");
  global.console.warn = record("warn");
  global.console.error = record("error");
  global.console.debug = record("debug");
  global.console.trace = record("trace");
  global.console.group = record("group");
  global.console.groupEnd = function () {};
  global.console.table = record("table");
  global.__drainConsole = function () {
    var out = lines.join("\n");
    lines = [];
    return out;
  };

  // Drives due timers once. The native host calls this each frame.
  global.__runTimers = function () {
    var due = [];
    var current = now();
    for (var i = 0; i < queue.length; i++) {
      var entry = queue[i];
      if (timers.has(entry.id) && entry.due <= current) due.push(entry);
    }
    queue = queue.filter(function (entry) {
      return timers.has(entry.id) && due.indexOf(entry) < 0;
    });
    for (var j = 0; j < due.length; j++) {
      var task = due[j];
      if (!timers.has(task.id)) continue;
      if (task.repeat >= 0) {
        task.due = now() + task.repeat;
        queue.push(task);
      } else {
        timers.delete(task.id);
      }
      task.callback.apply(null, task.args || []);
    }
    return due.length;
  };

  global.window = global.window || global;
  global.self = global.self || global;
  global.navigator = global.navigator || { userAgent: "spherekit-v8" };
})(globalThis);
"prelude ready";
