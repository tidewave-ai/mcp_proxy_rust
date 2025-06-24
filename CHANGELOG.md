# Changelog

## 0.2.2 (2025-06-24)

* Bug fixes
  * Fix backoff overflow after 64 reconnect tries causing endless immediate reconnect tries

## 0.2.1 (2025-06-18)

* Enhancements
  * add `--override-protocol-version` to override the protocol version reported by the proxy

## 0.2.0 (2025-05-20)

* Enhancements
  * support streamable HTTP transport: the proxy tries to automatically detect the correct transport to use
* Bug fixes
  * fix `annotations` field being sent as `null` causing issues in Cursor (upstream bug in the SDK)

## 0.1.1 (2025-05-02)

* Refactor code to use the [Rust MCP SDK](https://github.com/modelcontextprotocol/rust-sdk).

## 0.1.0 (2025-04-29)

Initial release.
