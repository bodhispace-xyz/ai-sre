# Provider reauthentication

OpenAI OAuth refresh state is rotating service state, not deployment policy.
Keep it in the backup-excluded `0700` cache and replace files atomically. A
restore therefore requires reauthentication; it must not restore an old refresh
token from PBS. Gemini and DeepSeek API keys are projected as static `0400`
files and must be replaced atomically through the secret manager.

If a provider fails authentication, leave the provider disabled and continue
with the deterministic report and the remaining admitted providers. Never turn
an absent budget ceiling into unlimited spend: an absent or stale price catalog
means deterministic fallback and an explicit Gate A failure.
