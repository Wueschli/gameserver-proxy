# TLS handshake flood limits — design

Date: 2026-10-03. Closes the "Native TLS — handshake flood" half of the HANDOVER
follow-up row. Scope: `wayhouse_http::tls::TlsListener`, so every fleet HTTP server that
serves native TLS (controller, aggregator, UI, `wayhouse`'s admin API) gets it at once.

## Problem

`TlsListener` runs every handshake in its own task with a 10 s timeout and no cap.
A client that connects and never speaks costs one task and one fd for 10 s; a flood
of them is bounded only by the process fd limit, and at that limit `accept` fails
with `EMFILE` for everyone. A plain global cap that *refuses* at the cap was tried
and dropped: ~cap idle connects, topped up every 10 s, lock every client out.

## Design

Three limits, all on handshakes still *pending* (accepted, not yet finished):

1. **ClientHello deadline — 3 s.** The ClientHello must arrive within 3 s of the
   accept (`tokio_rustls::LazyConfigAcceptor`); the whole handshake still has the
   10 s `HANDSHAKE_TIMEOUT`. An idle connect costs 3 s, not 10.
2. **Per-source cap — 16.** A source is an IPv4 address or an IPv6 /64 (an
   IPv4-mapped IPv6 peer counts as its IPv4 address). A connection that would be
   the source's 17th pending handshake is closed at once. A real client finishes a
   handshake in one RTT, so 16 concurrent ones from one source is far beyond normal.
3. **Global cap — 512, evicting the oldest.** When 512 handshakes are pending, a new
   connection is still admitted and the *oldest* pending handshake is dropped
   instead. New clients are never refused, so the lockout above cannot happen: to
   evict a real client's handshake an attacker must open 512 connections within
   that client's handshake (one RTT — ~5 000 connects/s at 100 ms). The cap keeps a
   many-source flood well below the fd limit.

A finished, failed or timed-out handshake frees its slot. Hitting either cap logs a
`warn` at most once a minute (every drop is a `debug`).

### Configuration

Constants, like `HANDSHAKE_TIMEOUT` — no new flags. They live in a public
`HandshakeLimits` struct (`Default` = the values above) taken by
`TlsListener::bind_with`; `bind` uses the default. Tests use tiny limits through it,
and flags (`TlsArgs`, `settings.admin.tls`) can be added later without touching the
listener.

### Implementation notes

- Bookkeeping is one `std::sync::Mutex` over a per-source count map, the pending
  ids in admission order (`BTreeMap`) and each pending task's `AbortHandle`. It is
  held for the admission, the handle's registration and the release when a task
  ends, never across a spawn, an abort or an `.await` (either can drop a task, and
  its slot, inline). A task that ends before its handle is registered is simply not
  registered. Only the single accept task admits, so every older handshake is
  registered by the time it could be evicted. This is the control plane's HTTP
  listener, not the data-plane hot path.
- Eviction aborts the oldest handshake task, which drops its socket.
- No new dependency: `LazyConfigAcceptor` is in `tokio-rustls` already.

## Testing

Unit tests for source keys and the admission bookkeeping (per-source refusal,
eviction order, release). Integration tests in `crates/wayhouse-http/tests/tls_server.rs`
over loopback with tiny limits: an over-cap source is closed while another source
(`127.0.0.2`) still gets in; at the global cap the oldest idle connection is closed
and a real client still succeeds; a silent client is dropped at the ClientHello
deadline, not the full timeout; a finished handshake frees its slot.

## Not done

Rate limiting new connections per source (a source can still cycle connects); flags
for the limits; metrics (`wayhouse-http` has no metrics registry — the binaries don't
share one).
