# mcp-proxy

A standalone binary for connecting STDIO based MCP clients to HTTP (SSE) based MCP servers.

Note: the proxy supports both SSE according to the `2024-11-05` as well as streamable HTTP according to the `2025-03-26` specification.
It may happen though that the connecting client does **not** support the version sent by the server.

## Installation

The latest releases are available on the [releases page](https://github.com/tidewave-ai/mcp_proxy_rust/releases).

### macOS

Depending on your Mac, you can download the latest version with one of the following commands:

Apple Silicon:

```bash
curl -sL https://github.com/tidewave-ai/mcp_proxy_rust/releases/latest/download/mcp-proxy-aarch64-apple-darwin.tar.gz | tar xv
```

Intel:

```bash
curl -sL https://github.com/tidewave-ai/mcp_proxy_rust/releases/latest/download/mcp-proxy-x86_64-apple-darwin.tar.gz | tar xv
```

which will put the `mcp-proxy` binary in the current working directory (`pwd`).

Note that the binaries are not notarized, so if you download the release with the browser, you won't be able to open it.

Alternatively, you can remove the quarantine flag with:

```bash
xattr -d com.apple.quarantine /path/to/mcp-proxy
```

### Linux

You can download the latest release from the [Releases page](https://github.com/tidewave-ai/mcp_proxy_rust/releases) or with one command, depending on your architecture:

x86:

```bash
curl -sL https://github.com/tidewave-ai/mcp_proxy_rust/releases/latest/download/mcp-proxy-x86_64-unknown-linux-musl.tar.gz | tar zxv
```

arm64 / aarch64:

```bash
curl -sL https://github.com/tidewave-ai/mcp_proxy_rust/releases/latest/download/mcp-proxy-aarch64-unknown-linux-musl.tar.gz | tar zxv
```

### Windows

You can download the latest release from the [Releases page](https://github.com/tidewave-ai/mcp_proxy_rust/releases) or with the following Powershell command:

```powershell
curl.exe -L -o mcp-proxy.zip https://github.com/tidewave-ai/mcp_proxy_rust/releases/latest/download/mcp-proxy-x86_64-pc-windows-msvc.zip; Expand-Archive -Path mcp-proxy.zip -DestinationPath .
```

## Building from scratch

The proxy is built in Rust. If you have Rust and its tools installed, the project can be built with `cargo`:

```bash
cargo build --release
```

Then, the binary will be located at `target/release/mcp-proxy`.

## Usage

If you have an SSE MCP server available at `http://localhost:4000/tidewave/mcp`, a client like Claude Desktop would then be configured as follows.

### On macos/Linux

```json
{
  "mcpServers": {
    "my-server": {
      "command": "/path/to/mcp-proxy",
      "args": ["http://localhost:4000/tidewave/mcp"]
    }
  }
}
```

### On Windows

```json
{
  "mcpServers": {
    "my-server": {
      "command": "c:\\path\\to\\mcp-proxy.exe",
      "args": ["http://localhost:4000/tidewave/mcp"]
    }
  }
}
```

## Configuration

`mcp-proxy` either accepts the SSE URL as argument or using the environment variable `SSE_URL`. For debugging purposes, you can also pass `--debug`, which will log debug messages on stderr.

Other supported flags:

* `--max-disconnected-time` the maximum amount of time for trying to reconnect while disconnected. When not set, defaults to infinity.
* `--override-protocol-version` to override the protocol version reported by the proxy. This is useful when using the proxy with a client that expects a different protocol version, when the only reason for mismatching protocols is the use of streamable / SSE transports.
