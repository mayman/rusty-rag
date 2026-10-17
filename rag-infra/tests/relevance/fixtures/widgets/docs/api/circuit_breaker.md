# Circuit breaker

The circuit breaker stops calls to a failing upstream. After five failures the
breaker opens, and one probe call decides when it closes again.

While the breaker is open, callers get an error at once instead of waiting for a
timeout, which keeps slow upstreams from tying up every worker thread. Tune the
failure count and the probe interval in the service configuration, and watch the
state changes in the logs to see how often an upstream flaps.