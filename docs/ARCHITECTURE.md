# Neutral Browser — architecture

## Stage 1: policy and capability broker

The requester does not supply the authorization decision. A web-content request is evaluated by `Policy`, and `Broker` returns the resulting decision.

```text
WEB CONTENT
    |
    v
Capability Request
    |
    v
Broker
    |
    v
Policy
    |
    +---- DENY ----> OS resource blocked
    |
    `---- ALLOW ---> permitted capability
```

Current bootstrap policy:

- web content has no ambient OS authority;
- filesystem, camera, microphone and clipboard requests are denied;
- network connection is the only capability admitted by this stage;
- non-web subjects are denied;
- the requester cannot set its own decision.

This is a policy/broker prototype, not yet an OS sandbox.

## Stage 2: broker-gated fetch chain (v0.3.0)

The first real slice of the engine chain from the master prompt (`URL -> NETWORK -> RESPONSE`):
a user command fetches a page through the broker. Nothing runs implicitly.

```text
USER COMMAND (--fetch URL)
    |
    v
URL parse (subset: http only; host/path whitelisted against CRLF injection)
    |
    v
Capability derivation (engine-side, not requester-side)
    |
    v
Broker -> Policy -> DENY --> NETWORK=CLOSED (no socket is ever opened)
    |
    ALLOW
    v
TCP connect  (the only TcpStream::connect in the codebase, verified by tests)
    |
    v
HTTP/1.1 GET (Host, Connection: close; no HTTP/2, no TLS in this stage)
    |
    v
Response head parse -> body framing (chunked | Content-Length | read-to-close)
    |
    v
Machine report: SCHEME/HOST/PORT/PATH, BROKER_*, HTTP_STATUS, HEADERS,
BODY_BYTES, BODY_SHA256 (FIPS 180-4), FETCH=OK
```

Module split: `policy.rs` + `broker.rs` carry stage 1 semantics unchanged;
`url.rs`, `http.rs`, `network.rs` are new; `sha256.rs` provides zero-dependency
integrity output.

Honest limitations (anti-facade, master prompt §23): HTTPS/TLS NOT_IMPLEMENTED
(planned: vendored rustls per DEPENDENCY_POLICY.md); HTTP/2 and QUIC
NOT_IMPLEMENTED; redirects NOT_IMPLEMENTED; scope matching in Policy
NOT_IMPLEMENTED (scope is recorded in the capability but not yet checked);
DNS uses the OS resolver, user-configurable DNS NOT_IMPLEMENTED; obs-fold and
compressed transfer codings are rejected, NOT_IMPLEMENTED.

