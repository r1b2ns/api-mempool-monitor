# api-mempool-monitor

A lightweight Bitcoin transaction monitor API built in Rust. Connect once and receive real-time status updates for any transaction in the mempool via **Server-Sent Events (SSE)** — no WebSocket overhead, no repeated polling from the client.

Powered by the public [mempool.space](https://mempool.space) API.

---

## Features

- **Real-time SSE stream** — server polls [mempool.space](https://mempool.space) on your behalf and pushes updates as they happen
- **Configurable polling interval** — from 5 to 60 seconds, tuned per request
- **Auto-close on confirmation** — stream ends automatically when the transaction is included in a block
- **Resilient** — handles `not_found` and transient API errors gracefully without dropping the connection
- **Zero authentication required** — uses the public mempool.space REST API

---

## Tech Stack

| Layer | Crate |
|---|---|
| HTTP server | [Axum](https://github.com/tokio-rs/axum) 0.7 |
| Async runtime | [Tokio](https://tokio.rs) |
| HTTP client | [reqwest](https://github.com/seanmonstar/reqwest) 0.12 |
| Streaming | [futures](https://docs.rs/futures) + [tokio-stream](https://docs.rs/tokio-stream) |
| Serialization | [serde](https://serde.rs) + serde_json |
| Logging | [tracing](https://docs.rs/tracing) |

---

## Getting Started

### Prerequisites

- Rust 1.75+ — install via [rustup](https://rustup.rs):

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

### Build & Run

```bash
git clone https://github.com/your-username/api-mempool-monitor.git
cd api-mempool-monitor
cargo run --release
```

The server starts on `http://0.0.0.0:3000`.

### Log level

Control verbosity via the `RUST_LOG` environment variable:

```bash
RUST_LOG=api_mempool=debug cargo run
```

---

## API Reference

### `GET /tx/:txid/watch`

Opens an SSE stream that polls the status of a Bitcoin transaction.

#### Path parameter

| Parameter | Description |
|---|---|
| `txid` | Bitcoin transaction ID (64-char hex) |

#### Query parameters

| Parameter | Type | Default | Description |
|---|---|---|---|
| `interval_secs` | integer | `10` | Polling interval in seconds (clamped to 5–60) |
| `stop_on_confirmed` | boolean | `true` | Close the stream automatically after confirmation |

#### Examples

```bash
# Basic usage — defaults (10s interval, closes on confirmation)
curl -N "http://localhost:3000/tx/YOUR_TXID_HERE/watch"

# Custom interval of 30 seconds
curl -N "http://localhost:3000/tx/YOUR_TXID_HERE/watch?interval_secs=30"

# Keep streaming after confirmation
curl -N "http://localhost:3000/tx/YOUR_TXID_HERE/watch?stop_on_confirmed=false"
```

#### SSE Events

| Event | When | Description |
|---|---|---|
| `tx_status` | every tick | Transaction is pending in the mempool |
| `confirmed` | once | Transaction was included in a block |
| `not_found` | every tick | Transaction not found (mempool or chain) |
| `error` | every tick | Transient error calling the mempool.space API |

> When `stop_on_confirmed=true` (default), the server closes the stream after emitting the `confirmed` event.

#### Event payload (JSON)

All events share the same JSON structure:

```json
{
  "txid": "a1b2c3d4e5f6...",
  "confirmed": true,
  "block_height": 880421,
  "block_hash": "00000000000000000002...",
  "block_time": 1740000000,
  "fee": 1500,
  "vsize": 141
}
```

| Field | Type | Description |
|---|---|---|
| `txid` | string | Transaction ID |
| `confirmed` | bool | Whether the transaction is confirmed |
| `block_height` | integer \| null | Block height (null if unconfirmed) |
| `block_hash` | string \| null | Block hash (null if unconfirmed) |
| `block_time` | integer \| null | Block timestamp in Unix seconds (null if unconfirmed) |
| `fee` | integer \| null | Transaction fee in satoshis |
| `vsize` | integer \| null | Virtual size in vbytes |

For `not_found` and `error` events, the payload is:

```json
{
  "txid": "a1b2c3d4e5f6...",
  "message": "Transaction not found in mempool or blockchain"
}
```

---

### `GET /health`

Returns `ok` — useful for container health checks and load balancer probes.

```bash
curl http://localhost:3000/health
# ok
```

---

## Consuming the SSE stream

### JavaScript (Browser / Node.js)

```javascript
const txid = 'YOUR_TXID_HERE';
const source = new EventSource(`http://localhost:3000/tx/${txid}/watch?interval_secs=15`);

source.addEventListener('tx_status', (e) => {
  const data = JSON.parse(e.data);
  console.log('Pending in mempool:', data);
});

source.addEventListener('confirmed', (e) => {
  const data = JSON.parse(e.data);
  console.log('Confirmed at block', data.block_height);
  source.close(); // stream already closed server-side, but good practice
});

source.addEventListener('not_found', (e) => {
  console.warn('Not found:', JSON.parse(e.data).message);
});

source.addEventListener('error', (e) => {
  console.error('API error:', JSON.parse(e.data).message);
});
```

### Python

```python
import json
import sseclient
import requests

txid = 'YOUR_TXID_HERE'
url = f'http://localhost:3000/tx/{txid}/watch'

response = requests.get(url, stream=True)
client = sseclient.SSEClient(response)

for event in client.events():
    data = json.loads(event.data)
    if event.event == 'tx_status':
        print(f"Pending — fee: {data.get('fee')} sats, vsize: {data.get('vsize')} vbytes")
    elif event.event == 'confirmed':
        print(f"Confirmed at block {data['block_height']}")
        break
    elif event.event == 'not_found':
        print(f"Not found: {data['message']}")
    elif event.event == 'error':
        print(f"Error: {data['message']}")
```

### cURL

```bash
curl -N --no-buffer \
  -H "Accept: text/event-stream" \
  "http://localhost:3000/tx/YOUR_TXID_HERE/watch"
```

---

## Project Structure

```
api-mempool-monitor/
├── Cargo.toml
└── src/
    ├── main.rs        # Axum router setup and server bootstrap
    ├── mempool.rs     # HTTP client for mempool.space REST API
    └── sse.rs         # SSE handler with polling logic
```

---

## Roadmap

- [ ] Docker image
- [ ] Support for Testnet / Signet (`?network=testnet`)
- [ ] Rate limiting per IP
- [ ] Metrics endpoint (Prometheus)
- [ ] Self-hosted mempool.space node support (`MEMPOOL_BASE_URL` env var)

---

## Contributing

Contributions are welcome! Feel free to open issues and pull requests.

1. Fork the repository
2. Create a feature branch: `git checkout -b feat/my-feature`
3. Commit your changes: `git commit -m 'feat: add my feature'`
4. Push to the branch: `git push origin feat/my-feature`
5. Open a Pull Request

Please follow [Conventional Commits](https://www.conventionalcommits.org/) for commit messages.

---

## License

This project is licensed under the **MIT License** — see the [LICENSE](LICENSE) file for details.
