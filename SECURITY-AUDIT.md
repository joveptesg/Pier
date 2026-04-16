# Pier Security Audit

**Date:** 2026-04-15  
**Auditor:** Internal  
**Version:** 0.1.0  

---

## Executive Summary

Pier stores service environment variables (database passwords, API keys, tokens) in plain text across multiple locations. API endpoints return secrets to authenticated users without masking. File permissions on sensitive files are not hardened.

**Overall Risk:** HIGH — acceptable for single-admin self-hosted PaaS, but requires hardening before multi-user or production deployment.

---

## Findings

### CRITICAL

#### SEC-001: Plain-text secrets in SQLite database
- **Location:** `services.env_json` column in `pier.db`
- **Impact:** Anyone with filesystem access to `pier.db` can read all service secrets (DB passwords, API keys, tokens)
- **Remediation:** Encrypt `env_json` at rest using AES-256-GCM with key from `PIER_SECRET` env var
- **Status:** FIXED

#### SEC-002: Environment variables exposed in Canvas API
- **Location:** `GET /api/v1/canvas` — `src/api/canvas.rs`
- **Impact:** Canvas endpoint calls `docker inspect` and returns ALL container env vars (including secrets) in the response. Visible in browser DevTools.
- **Remediation:** Move dependency detection to backend. Canvas API returns only dependency edges, not env_json.
- **Status:** FIXED

#### SEC-003: Secrets in resource detail API response
- **Location:** `GET /api/v1/resources/{id}` — `src/api/resources.rs`
- **Impact:** Returns full `env_json` with all secret values to any authenticated user
- **Remediation:** Mask secret values in API response (show keys only, values as `••••••••`)
- **Status:** FIXED

### HIGH

#### SEC-004: Database passwords exposed in databases API
- **Location:** `GET /api/v1/resources/{id}/databases` — `src/api/databases.rs`
- **Impact:** Returns `stored_password` for PostgreSQL/MySQL databases
- **Remediation:** Mask password in list response. Only show on explicit "reveal" action.
- **Status:** FIXED

#### SEC-005: S3 access keys exposed in list API
- **Location:** `GET /api/v1/s3` — `src/api/s3.rs`
- **Impact:** Returns `access_key` in list response
- **Remediation:** Mask access_key in list. Only show on explicit reveal.
- **Status:** NOTED (not critical — access_key is not secret by itself)

#### SEC-006: .env files written with default permissions
- **Location:** `src/deploy/mod.rs` — `write_env_file()`
- **Impact:** `.env` files in `data/stacks/*/` may be world-readable depending on umask
- **Remediation:** Set file permissions to 0600 (owner read/write only) after writing
- **Status:** FIXED

#### SEC-007: SQLite database file permissions
- **Location:** `data/pier.db`
- **Impact:** Database file may be world-readable
- **Remediation:** Set permissions to 0600 on database file at startup
- **Status:** FIXED

### MEDIUM

#### SEC-008: No rate limiting on API endpoints
- **Location:** All API routes
- **Impact:** Brute-force attacks on login, API abuse
- **Remediation:** Add rate limiting middleware (future phase)
- **Status:** NOTED

#### SEC-009: No CSRF protection
- **Location:** State-changing API endpoints (POST/PUT/DELETE)
- **Impact:** Cross-site request forgery possible if admin session is active
- **Remediation:** Add CSRF tokens for state-changing operations (future phase)
- **Status:** NOTED

#### SEC-010: Session management
- **Location:** Authentication middleware
- **Impact:** Session tokens stored in cookies without explicit security attributes
- **Remediation:** Ensure HttpOnly, Secure (when HTTPS), SameSite=Strict flags (future phase)
- **Status:** NOTED

### LOW

#### SEC-011: Git source tokens properly hidden
- **Location:** `GET /api/v1/sources` — `src/api/sources.rs`
- **Impact:** None — access tokens and private keys are NOT returned in API responses
- **Status:** OK (no action needed)

#### SEC-012: systemd hardening in place
- **Location:** `scripts/pier.service`
- **Impact:** Positive — `ProtectSystem=strict`, `ProtectHome=true`, `NoNewPrivileges=true`
- **Status:** OK (already hardened)

---

## Remediation Summary

| ID | Severity | Fix | Status |
|----|----------|-----|--------|
| SEC-001 | CRITICAL | AES-256 encryption for env_json | FIXED |
| SEC-002 | CRITICAL | Remove env vars from canvas API | FIXED |
| SEC-003 | CRITICAL | Mask secrets in resource API | FIXED |
| SEC-004 | HIGH | Mask DB passwords in API | FIXED |
| SEC-005 | HIGH | Mask S3 keys | NOTED |
| SEC-006 | HIGH | chmod 600 on .env files | FIXED |
| SEC-007 | HIGH | chmod 600 on pier.db | FIXED |
| SEC-008 | MEDIUM | Rate limiting | NOTED |
| SEC-009 | MEDIUM | CSRF protection | NOTED |
| SEC-010 | MEDIUM | Session hardening | NOTED |
| SEC-011 | LOW | Git tokens hidden | OK |
| SEC-012 | LOW | systemd hardened | OK |

---

## Architecture Notes

### Why not HashiCorp Vault?
Vault adds operational complexity (separate service, unsealing, HA) that is disproportionate for a single-admin self-hosted PaaS. The AES-256 encryption approach provides sufficient at-rest protection for this use case. Vault integration can be added as an optional feature for enterprise deployments.

### Encryption Key Management
- `PIER_SECRET` environment variable (32-byte key, base64 encoded)
- Auto-generated on first run if not set
- Stored in `/opt/pier/.env` (only accessible by pier user)
- Used for AES-256-GCM encryption of all sensitive data in SQLite

### Trust Model
- Single admin user with full access
- All API endpoints behind authentication
- Secrets accessible only through authenticated UI/API
- Filesystem access = full access (standard for self-hosted tools)
