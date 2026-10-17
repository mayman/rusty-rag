"""Refreshes the OAuth access token before it expires."""

import time

TOKEN_ENDPOINT = "/oauth/token"


def refresh_access_token(session, refresh_token):
    """Exchange a refresh token for a new access token."""
    response = session.post(
        TOKEN_ENDPOINT,
        data={"grant_type": "refresh_token", "refresh_token": refresh_token},
    )
    response.raise_for_status()
    body = response.json()
    return body["access_token"], time.time() + body["expires_in"]


def token_is_expiring(expires_at, margin_seconds=60):
    return expires_at - time.time() < margin_seconds
