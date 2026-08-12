# Restore and quarantine

Restore only a completed, digest-valid application snapshot plus its manifest.
Never copy a live SQLite directory into service data. Run SQLite integrity and
journal replay checks in a temporary location, then start the restored service
with the external restore epoch and all mutation credentials absent.

The restored instance must enter quarantine. It may serve read-only reports and
replay evidence, but it must not resume pending approvals, dispatch work, or
load a write credential. An operator must reconcile the epoch and journal
through the later supervised-recovery gate; a restart or missing epoch never
clears quarantine.
