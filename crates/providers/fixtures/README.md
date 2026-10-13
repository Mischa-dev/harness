# Provider fixtures

Response bodies the adapter tests parse. They follow each provider's
documented wire format and are written by hand. M0 replaces them with
responses recorded from real calls (scrubbed of ids that could identify an
account) without changing the tests' expectations.

Never commit a fixture that contains a key, a token, or a request header.
