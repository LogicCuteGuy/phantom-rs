# phantom

Makes hosted Bedrock/MCPE servers show up as LAN servers, specifically for consoles.

You can now play on remote servers (not Realms!) on your Xbox and PS4 with friends.

It's like having a LAN server that's not actually there, spooky.

## Installing

You can either:

- Download a prebuilt binary from [Releases](https://github.com/jhead/phantom/releases)
- Build locally with Cargo

Binary names:

- Windows: `phantom-rs.exe`
- macOS/Linux: `phantom-rs`

If needed on macOS/Linux:

```bash
chmod u+x ./phantom-rs
```

## Usage

Open up a command prompt (Windows) or terminal (macOS & Linux) to the location
where you downloaded or built `phantom-rs`, then run the command below.
The server should show up on your LAN server list within a few seconds. If not,
you did something wrong. Or I did ;)

```
Usage: phantom-rs [OPTIONS]

Options:
	--server <SERVER>         Required: Bedrock/MCPE server host:port (example: 1.2.3.4:19132)
	--bind <BIND>             Optional: IP address to listen on (default: 0.0.0.0)
	--bind_port <BIND_PORT>   Optional: Proxy bind port (default: 0 = random)
	--timeout <TIMEOUT>       Optional: Client idle timeout in seconds (default: 60)
	--debug                   Optional: Enable debug logging
	-6, --ipv6                Optional: Enable IPv6 support on port 19133 (experimental)
	--remove_ports            Optional: Remove ports from pong packets (experimental)
	--workers <WORKERS>       Optional: Number of worker listeners (default: 1)
	--server_protocol <SERVER_PROTOCOL>
	                          Optional: Upstream server protocol for 7551 discovery transport
	                          values: raknet | nethernet (default: raknet)
	--relay <RELAY>           Optional: Relay WebSocket URL (example: ws://127.0.0.1:8080)
	--room <ROOM>             Optional: Relay room name (default: bedrock1)
	--relay_role <ROLE>       Optional: Relay mode: host | player
	--direct_relay            Optional: Shortcut for relay host mode (no local bind proxy)
```

Bind/client-facing discovery is currently RakNet-only.

Relay examples:

- Relay host mode (equivalent to app_host.js):

```bash
cargo run -- --relay ws://127.0.0.1:8080 --room bedrock1 --relay_role host --server 127.0.0.1:19132
```

- Relay player mode (equivalent to app_player.js):

```bash
cargo run -- --relay ws://127.0.0.1:8080 --room bedrock1 --relay_role player --bind_port 19132
```

- Direct relay shortcut (host mode, no local bind proxy):

```bash
cargo run -- --direct_relay --relay ws://127.0.0.1:8080 --room bedrock1 --server 127.0.0.1:19132
```

- Relay host with NetherNet upstream (query 7551 and answer RakNet MOTD to players):

```bash
cargo run -- --relay ws://127.0.0.1:8080 --room bedrock1 --relay_role host --server 127.0.0.1:17551 --server_protocol nethernet
```

**Example**

Connect to a server at IP `lax.mcbr.cubed.host` port `19132`:

```bash
./phantom-rs --server lax.mcbr.cubed.host:19132
```

![fVoNSdU](https://user-images.githubusercontent.com/360153/85956959-18003880-b93e-11ea-811e-424b33b98528.png)

Same as above but bind to a specific local IP:

```bash
./phantom-rs --bind 10.0.0.5 --server lax.mcbr.cubed.host:19132
```

Same as above but bind the proxy server to port 19133:
   
```bash
./phantom-rs --bind_port 19133 --server lax.mcbr.cubed.host:19132
```

Same as above but bind the proxy server to local IP 10.0.0.5 and port 19133:
   
```bash
./phantom-rs --bind 10.0.0.5 --bind_port 19133 --server lax.mcbr.cubed.host:19132
```

**Running multiple instances**

If you have multiple Bedrock servers, you can run phantom multiple times on
the same device to allow all of your servers to show up on the LAN list. All
you have to do is start one instance of phantom for each server and set the
`--server` flag appropriately. You don't need to use `--bind` or change the port.
But you probably do need to make sure you have a firewall rule that allows
all UDP traffic for the phantom executable.

Legacy single-dash flags like `-server` are still accepted for compatibility,
but `--server`/`--bind` style is recommended.

**A note on `--bind`:**

The port is randomized by default and specifically omitted from the flag because
the port that phantom runs on is irrelevant to the user. phantom must bind to
port 19132 on all interfaces (or at least the broadcast address) to receive
ping packets from LAN devices. So phantom will always do that and there's no
way to configure otherwise, but you can also pick which IP you want the proxy
itself to listen on, just in case you need that. You shouldn't though.

As long as the device you run phantom from is on the same LAN, the default
settings should allow other LAN devices to see it when you open Minecraft.

**A note on `--bind_port`:**

The port used by the proxy server can be defined with the `--bind_port` flag.
It can be useful if you are behind a firewall or using Docker and want to open only
necessary ports to phantom. Note that you'll always need to open port 19132 in addition
to the bind port for phantom to work.

This flag can be used with or without the `--bind` flag. 
Default value is 0, which means a random port will be used.

## Building

This repository now includes a Rust implementation with both library and binary
targets in the same `src/` directory:

- `src/lib.rs` for reusable proxy/protocol logic
- `src/main.rs` for the CLI executable entrypoint

Build with Cargo:

```bash
cargo build --release
```

Built binary path:

- Windows: `target/release/phantom-rs.exe`
- macOS/Linux: `target/release/phantom-rs`

Run directly with Cargo:

```bash
cargo run -- --server lax.mcbr.cubed.host:19132
```

The Rust crate is the implementation for this repository.

## How does this work?

On Minecraft platforms that support LAN servers, the game will broadcast a
server ping packet to every device on the same network and display any valid
replies as connectable servers. This tool runs on your computer - desktop,
laptop, Raspberry Pi, etc. - and pretends to be a LAN server, acting as a proxy,
passing all traffic from your game through your computer and to the server
(and back), so that Minecraft thinks you're connected to a LAN server, but
you're really playing on a remote server. As soon as you start it up, you should
see the fake server listed under LAN and, upon selecting it, connect to the real
Bedrock/MCPE server hosted elsewhere.

For an optimal experience, run this on a device that is connected via ethernet
and not over WiFi, since a wireless connection could introduce some lag. Your
game device can be connected to WiFi. Your remote server can be running on a
computer, a VM, or even with a Minecraft hosting service.

## Supported platforms

- This tool should work on Windows, macOS, and Linux.
- ARM builds are available for Raspberry Pi and similar SOCs.
- Minecraft for Windows 10, iOS/Android, Xbox One, and PS4 are currently supported.
- **Nintendo Switch is not supported.**

Note that you almost definitely need to create a firewall rule for this to work.
On macOS, you'll be prompted automatically. On Windows, you may need to go into
your Windows Firewall settings and open up all UDP ports for phantom.