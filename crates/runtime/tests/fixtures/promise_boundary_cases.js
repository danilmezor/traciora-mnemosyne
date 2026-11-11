'use strict';
// Independently authored disposable fixture driver. It invokes the real Rust
// module entries; no application or SDK object is synthesized.
globalThis.__reaPromiseOriginal = Promise;
globalThis.__reaPromiseOriginalResolve = Promise.resolve;
globalThis.__reaPromiseProbe = async function (spec) {
  const original = Object.getOwnPropertyDescriptor(window, 'Promise');
  const P = original.value;
  const trace = [];
  const sentinel = { owned: 'constructor or instance failure' };
  const proxy = new Proxy(P, {
    construct(target, args, newTarget) {
      trace.push({ op: 'construct', executor: {
        name: args[0].name, length: args[0].length,
        ownPrototype: Object.prototype.hasOwnProperty.call(args[0], 'prototype')
      } });
      if (spec.constructor_throw) throw sentinel;
      return Reflect.construct(target, args, newTarget);
    },
    get(target, key, receiver) {
      if (key === Symbol.hasInstance) {
        trace.push({ op: 'get-instance' });
        if (spec.has_instance) return value => {
          trace.push({ op: 'instance', type: typeof value });
          if (spec.has_instance === 'throw') throw sentinel;
          return spec.has_instance === 'true';
        };
      }
      return Reflect.get(target, key, receiver);
    }
  });
  Object.defineProperty(window, 'Promise', { configurable: true, get() {
    trace.push({ op: 'get-constructor' }); return proxy;
  } });
  let task;
  let synchronousThrow = false;
  try {
    const addon = require(ADDON_PATH);
    task = spec.target === 'handler'
      ? addon.evaluate(spec.code)
      : addon.handleCli(spec.missing ? undefined : spec.code);
  } catch (error) {
    task = P.reject(error); synchronousThrow = true;
  } finally {
    Object.defineProperty(window, 'Promise', original);
  }
  const prefix = trace.slice();
  try { return { id: spec.id, prefix, trace, result: await task, synchronousThrow }; }
  catch (error) { return { id: spec.id, prefix, trace, synchronousThrow, error: {
    type: typeof error, same: error === sentinel, name: error && error.name,
    message: error && error.message, text: typeof error === 'string' ? error : null
  } }; }
};
undefined;
