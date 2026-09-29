# Protected deployment inbox

Receipt ingestion is disabled by default. It does not start validation, prepare a
repair, approve an action, or publish to GitHub. The application still needs
separate source-freshness and repair-dispatch integration.

## Deployment settings

Set `AI_SRE_DEPLOYMENT_INBOX` to an absolute local Linux directory. The directory,
all ancestors, and final receipt files must belong to root and have no group or
other write permissions. Symlinks are not allowed. Mount the inbox read-only for
the non-root AI-SRE service. Use local storage; this reader does not provide a
deadline for a stalled filesystem. Do not point it at a network filesystem.

Optional bounds:

| Variable | Default | Accepted range |
| --- | --- | --- |
| `AI_SRE_DEPLOYMENT_INBOX_MAX_ENTRIES` | 256 | 1–4096 |
| `AI_SRE_DEPLOYMENT_INBOX_MAX_BYTES` | 16777216 | 1–67108864 |
| `AI_SRE_DEPLOYMENT_INBOX_POLL_SECONDS` | 15 | 1–3600 |

Setting a bound without an inbox path is an error. Invalid values do not fall
back to defaults. Each receipt also retains the existing 1 MiB limit.

## Publication and revocation

The trusted delivery process publishes immutable regular files by atomic rename.
Final names end in `.json`; their stems contain only ASCII letters, digits,
hyphens, and underscores, with a maximum of 192 bytes. The file name is not the
deployment identity: the protected receipt's `deployment_id` supplies that value.
Hard links, nested directories, malformed receipts, and unexpected names reject
the scan. Files starting with `.receipt-` are temporary, never imported, but still
count toward the directory-entry limit.

For example, `deployment-123.json` can hold the original receipt. A later
`deployment-123-revoked.json` can hold a complete v1 receipt with the same
deployment identity and `revoked: true`. The trusted producer/delivery process
must supply that record; AI-SRE must not manufacture it. The existing journal
rules permanently invalidate an identity on revocation or contradiction. Replaying
the original file cannot undo that decision.

Retain final publications for restart and replay. Do not overwrite an original
file or edit it in place. Deleting a file is **not** revocation and does not erase
journal history. Retention/compaction needs a separate reviewed protocol; when
the configured bound is reached, the importer fails rather than drops evidence.

## Application behavior

The application scans at startup, on the configured timer, and before processing
each incident. The single application journal owner commits each receipt and its
audit fact together. Exact replay does not append duplicate facts. Reads check
current evidence age; polling does not extend the original health window.

The reader first loads the complete bounded scan on a blocking worker thread,
so file reading and JSON parsing do not block the application's async thread.
It discards a scan if directory metadata changes during the read and retries the
whole scan, with a maximum of three attempts. A stable invalid scan fails without
retry. Continuous publication can still exhaust the bound and stop intake;
there is no unbounded retry loop or acceptance of a partial scan.

The application awaits one scan at a time. It retains sole journal ownership and
yields to other runtime tasks between receipt commits. Individual SQLite calls
remain synchronous. Cancelling the application does not cancel a filesystem read
already running on the blocking thread; the local-storage requirement remains.
A journal failure can leave earlier receipt transactions committed; the worker
stops before processing another incident. Restart safely replays those receipts.

An enabled inbox is a required dependency: a failed startup scan rejects startup;
a later failed scan stops the supervised worker. This conservative policy also
interrupts shadow investigation. Operators must restore the evidence source;
there is no automatic fallback that ignores malformed or missing evidence.

An empty inbox can be read successfully, but it grants no qualification. A complete
scan does not prove that the upstream delivery process is current or that all
revocations arrived. It also says nothing about Git mirror freshness. Do not use
scan success as repair readiness. Live producer hooks, delivery/revocation
acceptance, mirror synchronization, and production enrollment remain activation
requirements.

## Verification

Ordinary tests cover bounded file selection, temporary-file exclusion, rejected
links, configuration bounds, idempotent journal replay, aging, and revocation.
The ignored `protected_linux_scan_ingests_revocation_and_rejects_unsafe_entries`
library test must run explicitly as root in a disposable Linux VM. It uses
synthetic receipts, creates a process-specific directory under `/root`, and removes
that directory after success. Never run it as root on a production host.
