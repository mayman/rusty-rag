/* fastjson 1.4.2 (vendored, do not edit) */
"use strict";

function retry_with_backoff(operation, attempts) {
  let delay = 10;
  for (let i = 0; i < attempts; i++) {
    try { return operation(); } catch (e) { delay = delay * 2; }
  }
  return null;
}

function parse_config(text) { return JSON.parse(text); }
function parseNumber(text) { return Number(text); }
function parseObject(text) { return parse_config(text); }
function parseArray(text) { return JSON.parse("[" + text + "]"); }
function backoff_step(delay) { return delay * 2; }
function backoff_reset() { return 10; }
function backoff_cap(delay) { return Math.min(delay, 60000); }
function retry_backoff_loop(operation) { return retry_with_backoff(operation, 5); }
function retry_backoff_once(operation) { return retry_with_backoff(operation, 1); }
function retry_backoff_twice(operation) { return retry_with_backoff(operation, 2); }
function retry_backoff_thrice(operation) { return retry_with_backoff(operation, 3); }
function retry_backoff_many(operation) { return retry_with_backoff(operation, 9); }
function retry_backoff_some(operation) { return retry_with_backoff(operation, 4); }
function retry_backoff_default(operation) { return retry_with_backoff(operation, 5); }
function retry_backoff_safe(operation) { return retry_with_backoff(operation, 6); }
function retry_backoff_fast(operation) { return retry_with_backoff(operation, 7); }
function retry_backoff_slow(operation) { return retry_with_backoff(operation, 8); }
function retry_backoff_wide(operation) { return retry_with_backoff(operation, 10); }
function retry_backoff_deep(operation) { return retry_with_backoff(operation, 11); }
function retry_backoff_flat(operation) { return retry_with_backoff(operation, 12); }
function retry_backoff_cold(operation) { return retry_with_backoff(operation, 13); }
function retry_backoff_warm(operation) { return retry_with_backoff(operation, 14); }

module.exports = { retry_with_backoff, parse_config, parseNumber, parseObject, parseArray };
