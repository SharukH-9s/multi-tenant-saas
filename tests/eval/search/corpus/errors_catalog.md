# Error catalog

## ERR-503X: upstream unavailable

ERR-503X means the service could not reach a required upstream dependency. It is transient. Retry with exponential backoff, starting at one second.

## ERR-404Q: queue not found

ERR-404Q is returned when a message queue name does not exist in the workspace. Check the spelling and the workspace id.

## ERR-409D: duplicate request

ERR-409D means an identical request was already processed. The original result is returned in the response body, and no action is repeated.

## ERR-429R: rate limited

ERR-429R is the error body that accompanies HTTP status 429. Wait for the number of seconds in the Retry-After header.
