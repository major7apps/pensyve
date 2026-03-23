"""RBAC enforcement middleware for the Pensyve API.

Maps API key identity to namespace roles and gates write operations
behind Writer+ permissions. Read operations require Reader+ (currently
all authenticated users).
"""

import os

import structlog

from fastapi import HTTPException, Request

logger = structlog.get_logger()

# Role hierarchy: Owner > Writer > Reader
ROLE_HIERARCHY = {"owner": 3, "writer": 2, "reader": 1}


def _get_caller_role(request: Request) -> str:
    """Determine the caller's role from the request context.

    In single-tenant mode (no PENSYVE_RBAC_ENABLED), all authenticated
    callers are treated as owners. In multi-tenant mode, the role is
    derived from server-side configuration (not client headers).
    """
    if os.environ.get("PENSYVE_RBAC_ENABLED", "false").lower() != "true":
        return "owner"

    # Default role for all authenticated callers. Future: derive from API key -> role mapping.
    return os.environ.get("PENSYVE_DEFAULT_ROLE", "writer")


def require_role(required: str):
    """FastAPI dependency that checks the caller has at least `required` role.

    Usage:
        @app.post("/v1/remember", dependencies=[Depends(require_role("writer"))])
    """
    required_level = ROLE_HIERARCHY.get(required, 1)

    async def _check(request: Request):
        caller_role = _get_caller_role(request)
        caller_level = ROLE_HIERARCHY.get(caller_role, 0)
        if caller_level < required_level:
            logger.warning(
                "rbac_denied",
                caller_role=caller_role,
                required=required,
                path=str(request.url.path),
            )
            raise HTTPException(
                status_code=403,
                detail=f"Insufficient permissions: requires {required} role",
            )

    return _check
