"""RBAC enforcement middleware for the Pensyve API.

Maps API key identity to namespace roles and gates write operations
behind Writer+ permissions. Read operations require Reader+ (currently
all authenticated users).
"""

import os
import logging

from fastapi import HTTPException, Request

logger = logging.getLogger(__name__)

# Role hierarchy: Owner > Writer > Reader
ROLE_HIERARCHY = {"owner": 3, "writer": 2, "reader": 1}


def _get_caller_role(request: Request) -> str:
    """Determine the caller's role from the request context.

    In single-tenant mode (no PENSYVE_RBAC_ENABLED), all authenticated
    callers are treated as owners. In multi-tenant mode, the role would
    be resolved from the API key -> namespace ACL mapping.
    """
    if os.environ.get("PENSYVE_RBAC_ENABLED", "false").lower() != "true":
        return "owner"

    # Future: look up role from API key -> namespace ACL table
    # For now, check X-Pensyve-Role header (set by upstream gateway/proxy)
    role = request.headers.get("X-Pensyve-Role", "reader").lower()
    if role not in ROLE_HIERARCHY:
        role = "reader"
    return role


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
                "RBAC denied: caller_role=%s required=%s path=%s",
                caller_role, required, request.url.path,
            )
            raise HTTPException(
                status_code=403,
                detail=f"Insufficient permissions: requires {required} role",
            )

    return _check
