# mcp-proxy

A standalone binary for connecting STDIO based MCP clients to HTTP (SSE) based MCP servers.

Note: At the moment this only works with MCP servers that use the `2024-11-05` specification.

## Installation

First, find the correct archive from [the release page](https://github.com/tidewave-ai/mcp_proxy_rust/releases).
Note that these binaries are not notarized. On macOS, you won't be able to run them if you download them through
a web browser. You can circumvent the quarantine by directly downloading the file using curl, for example:

```bash
$ curl -sL https://github.com/tidewave-ai/mcp_proxy_rust/releases/download/v0.1.1/mcp-proxy-x86_64-apple-darwin.tar.gz | tar xv
```

Alternatively, remove the quarantine flag:

```bash
$ xattr -d com.apple.quarantine /path/to/mcp-proxy
```

After downloading and extracting the release, the proxy is ready to be used.

## Building from scratch

The proxy is built in Rust. If you have Rust and its tools installed, the project can be built with `cargo`:

```bash
$ cargo build --release
```

Then, the binary will be located at `target/release/mcp-proxy`.

## Usage

If you have an SSE MCP server available at `http://localhost:4000/tidewave/mcp`, a client like Claude Desktop would then be configured as follows.

### On macos/Linux

You will also need to know the location of the `escript` executable, so run `which escript` before to get the value of "path/to/escript":

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
