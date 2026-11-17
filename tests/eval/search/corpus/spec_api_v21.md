# Platform API v2.1

API v2.1 is the current stable version of the Marlowe platform API.

## Authentication

Send a bearer token in the Authorization header. Tokens expire after one hour and can be refreshed with the refresh endpoint.

## Rate limits

Each API key may send 600 requests per minute. Requests above the limit receive HTTP status 429 together with a Retry-After header.

## Pagination

List endpoints use cursor pagination. The default page size is 50 items and the maximum is 200.

## Versioning

Breaking changes are announced at least six months before they take effect.
