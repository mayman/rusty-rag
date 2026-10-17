package net

import "testing"

func TestGracefulShutdownDrainsRequests(t *testing.T) {
	srv := newServer()
	srv.Shutdown() // graceful shutdown
	if srv.inFlight != 0 {
		t.Fatal("graceful shutdown left requests in flight")
	}
}