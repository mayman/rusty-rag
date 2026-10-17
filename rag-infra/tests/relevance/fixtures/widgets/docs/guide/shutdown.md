# Shutdown

## Graceful shutdown

On SIGTERM the server stops accepting connections and finishes in-flight requests
before it exits. This graceful shutdown is bounded by a drain timeout.