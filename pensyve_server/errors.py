"""Structured error types for the Pensyve API."""

from typing import Any

from pydantic import BaseModel


class ErrorResponse(BaseModel):
    """Structured error response returned by all API error paths."""

    error: str
    message: str
    request_id: str
    detail: dict[str, Any] | None = None


class PensyveError(Exception):
    """Base error for all Pensyve API errors."""

    status_code: int = 500
    error_code: str = "internal_error"

    def __init__(self, message: str, detail: dict[str, Any] | None = None):
        self.message = message
        self.detail = detail
        super().__init__(message)


class NotFoundError(PensyveError):
    status_code = 404
    error_code = "not_found"


class AuthenticationError(PensyveError):
    status_code = 401
    error_code = "unauthorized"


class RateLimitError(PensyveError):
    status_code = 429
    error_code = "rate_limited"


class PensyveValidationError(PensyveError):
    """Named to avoid conflict with pydantic.ValidationError."""

    status_code = 422
    error_code = "validation_error"


class PermissionError(PensyveError):
    status_code = 403
    error_code = "forbidden"
