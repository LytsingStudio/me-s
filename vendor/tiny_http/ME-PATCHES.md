# Local tiny_http maintenance

Based on crates.io `tiny_http` 0.12.0, upstream https://github.com/tiny-http/tiny-http.
Upstream source and integration tests are retained under the original MIT OR Apache-2.0 licenses. Examples and benchmarks are omitted.

Local changes:
- Propagate socket duplication errors rather than panicking in the accept thread.
- Retain the listener and retry acceptance/connection setup failures with 100 ms–1 s backoff; report each repeated error burst once.
- Apply 30-second read/write inactivity timeouts to accepted sockets. This is not an application execution deadline or a total transfer deadline.
- Propagate thread creation failures instead of panicking; failed connection setup drops only that connection and never replays a request.

The me-s and me-gateway request consumers also retain their runtime on listener errors. The business encryption and authentication layers are unchanged.
