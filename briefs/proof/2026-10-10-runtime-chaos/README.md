# Change degraded service levels without restarting

Set `CAIRN_CHAOS_FILE` to a file containing `off`, `normal`, or `incident`.
The running server reads it every five seconds. `CAIRN_CHAOS=1` or `true` supplies
the normal startup default; otherwise the default is off.

Observed with the same locally built server process over a temporary Turso database:

| File content | Searches | Searches taking at least 1.5 s | Request p95 |
| --- | ---: | ---: | ---: |
| off | 80 | 0 | 43 ms |
| incident | 160 | 60 | 2663 ms |
| normal | 512 | 8 | 252 ms |
| off again | 80 | 0 | 5 ms |
| invalid | 80 | 0 | 4 ms |
| file removed | 80 | 0 | 5 ms |
| recreated with incident | 80 | 22 | 2487 ms |

All searches succeeded. Invalid content and removal restored the off startup default,
with one warning across both failures. Recreating the file restored incident behavior.
A store-only deployment read showed pool delays too: 80 of 160 requests took at least
200 ms, with p95 at 505 ms. The server stopped cleanly after the run.

## Known limits

- Changes take effect on the next read, within about five seconds for a local file.
- Delays are random. Pool acquisitions can add to the 1.5-2.5 second search delay,
  and a request can acquire more than one connection.
