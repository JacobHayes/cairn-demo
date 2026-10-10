# Runtime chaos levels come from one small file

- Question: how should the requested runtime chaos experiment change both API faults
  and pool delays without a restart, despite the usual ban on behavior configuration?
- Call: honor the explicitly requested `CAIRN_CHAOS_FILE` as an experimental host
  control. Keep `CAIRN_CHAOS=1` or `true` as the normal startup default, otherwise off.
  One host thread reads the regular file at startup and every five seconds and updates
  the existing atomic selectors. Missing or invalid content uses that startup default,
  with one warning per process, and later valid content recovers automatically.
  Read at most the existing 64-byte id bound plus one overflow byte; levels are short
  names. Incident uses 90/256 slow searches at 1500-2500 ms and 38/256 pool delays at
  250 ms. Normal rates and list errors retain their existing values.
- Alternatives: modification-time caching saves a tiny read but adds state; an admin
  endpoint adds authentication and an API surface. A bounded read every five seconds
  keeps request workers free of file I/O and needs no dependency or database state.
- What would change it: retiring the experiment removes the control; a permanent
  operational control would require revisiting the documented configuration surface.
