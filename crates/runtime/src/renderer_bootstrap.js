'use strict';
// Real renderer global bindings; no SDK or product object is installed here.
globalThis.require(ADDON_PATH).start(
  SOCKET_PATH, globalThis.require, globalThis.module,
  globalThis.__filename, globalThis.__dirname,
  function (dispatch, name, constructible) {
    if (constructible) return function (...args) { return dispatch(this, new.target, args); };
    return ({ [name](...args) { return dispatch(this, new.target, args); } })[name];
  },
  function (value, key) { return value[key]; },
  function (value, key, next) { return value[key] = next; },
  function (operation, left, right) {
    switch (operation) {
      case 'add': return left + right;
      case 'subtract': return left - right;
      case 'less-than': return left < right;
      case 'greater-than': return left > right;
      default: throw new TypeError('Unsupported binary operation');
    }
  },
  function (value, constructor) { return value instanceof constructor; },
  false, // Electron already installed genuine main-world bindings.
  function (value, key) { return delete value[key]; },
  (function(){const begin=function*(value){for(const key in value)yield key};const next=Object.getPrototypeOf(begin({})).next;const apply=Reflect.apply;return [begin,function(iterator){return apply(next,iterator,[])}];})()
);
