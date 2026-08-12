# Tempo tracing

Tempo is an optional observation sink for the shadow responder. Configure only
the OTLP/HTTP `/v1/traces` endpoint and keep its queue bounded. AI-SRE exports
trace and span metadata such as phase, provider alias, duration, and stable
correlation IDs; prompts, tool results, log bodies, evidence text, secrets, and
approval tokens are never trace attributes.

If Tempo is unavailable, slow, or full, the exporter drops events and increments
its bounded loss counter. It never blocks intake, journal commits, provider
fallback, or recovery. The incident journal remains authoritative after Tempo
restart or retention expiry.
