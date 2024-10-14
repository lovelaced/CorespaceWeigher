# Consumption Tracker

## Overview

> [!NOTE]
> This is a fork of Region X's Corespace Weigher, which removes the CSV/historical functionality and replaces it with an SSE stream. Not for production use (maybe?).

A Rust application that tracks weight consumption for parachains on the Polkadot network and broadcasts real-time updates to connected clients using Server-Sent Events (SSE).

## Table of Contents

- [Overview](#overview)
- [Features](#features)
- [Prerequisites](#prerequisites)
- [Installation](#installation)
- [Configuration](#configuration)
- [Usage](#usage)
- [Client Integration](#client-integration)
- [API Reference](#api-reference)
- [Contributing](#contributing)
- [License](#license)

---

## Overview

The **Parachain Weight Consumption Tracker** is a Rust-based application designed to monitor the weight consumption of parachains on the Polkadot network. It connects to registered parachains, tracks finalized blocks, calculates weight consumption metrics, and broadcasts these updates to connected clients via a Server-Sent Events (SSE) endpoint.

This tool is useful for developers and network operators who need to monitor the performance and resource utilization of parachains in real-time.

---

## Features

- **Real-Time Monitoring**: Tracks finalized blocks and calculates weight consumption metrics for each parachain.
- **Server-Sent Events (SSE) Endpoint**: Provides a `/events` endpoint for clients to receive live updates.
- **Data Caching**: Caches the latest consumption data for new clients upon connection.
- **Concurrent Tracking**: Utilizes asynchronous tasks to track multiple parachains concurrently.
- **CORS Support**: Configurable Cross-Origin Resource Sharing (CORS) to allow cross-origin requests.

---

## Prerequisites

- **Network Access**: Ability to connect to parachain RPC endpoints.

---

## Installation

1. **Clone the Repository**

   ```bash
   git clone https://github.com/lovelaced/CorespaceWeigher.git
   cd CorespaceWeigher
   ```

2. **Build the Application**

   ```bash
   cargo build --release
   ```

---

## Configuration

The application can be configured via environment variables or a `.env` file in the project root.

### Environment Variables

- **`SSE_IP`**: IP address for the SSE server to bind to. Default is `127.0.0.1`.
- **`SSE_PORT`**: Port for the SSE server. Default is `9001`.

### `.env` File

Create a `.env` file in the project root to set environment variables:

```dotenv
SSE_IP=127.0.0.1
SSE_PORT=9001
```

---

## Usage

1. **Run the Application**

   ```bash
   cargo run --release
   ```

   The application will start tracking parachains and serve the SSE endpoint at the configured IP and port.

2. **Monitor the Logs**

   The application uses `env_logger` for logging. You can set the log level via the `RUST_LOG` environment variable:

   ```bash
   RUST_LOG=info cargo run --release
   ```

---

## Client Integration

Clients can connect to the SSE endpoint to receive real-time updates on parachain weight consumption.

### SSE Endpoint

- **URL**: `http://{SSE_IP}:{SSE_PORT}/events`

### Data Format

Each event sent to the client has the following properties:

- **Event ID**: A unique identifier in the format `{para_id}-{block_number}`.
- **Event Type**: `"consumptionUpdate"`.
- **Data**: JSON-formatted string containing the `ConsumptionUpdate` structure.

#### `ConsumptionUpdate` Structure

```json
{
  "para_id": 1000,
  "relay": "Polkadot",
  "block_number": 123456,
  "extrinsics_num": 5,
  "ref_time": {
    "normal": 0.5,
    "operational": 0.3,
    "mandatory": 0.2
  },
  "proof_size": {
    "normal": 0.4,
    "operational": 0.35,
    "mandatory": 0.25
  },
  "total_proof_size": 0.9
}
```

### Example Client Code

#### JavaScript (Browser)

```javascript
const eventSource = new EventSource('http://localhost:9001/events');

eventSource.addEventListener('consumptionUpdate', (event) => {
  const data = JSON.parse(event.data);
  console.log('Received data:', data);
});

eventSource.onerror = (error) => {
  console.error('SSE error:', error);
};
```

#### React Hook Example

```javascript
import { useState, useEffect } from 'react';

export const useWeightConsumption = (url) => {
  const [data, setData] = useState({});

  useEffect(() => {
    const eventSource = new EventSource(url);

    eventSource.addEventListener('consumptionUpdate', (event) => {
      const parsedData = JSON.parse(event.data);
      setData((prevData) => ({
        ...prevData,
        [parsedData.para_id]: parsedData,
      }));
    });

    eventSource.onerror = (error) => {
      console.error('SSE error:', error);
    };

    return () => {
      eventSource.close();
    };
  }, [url]);

  return data;
};
```

---

## API Reference

### SSE Events

- **Endpoint**: `/events`
- **Method**: `GET`
- **Headers**:
  - `Content-Type`: `text/event-stream`
  - `Cache-Control`: `no-cache`
  - `Connection`: `keep-alive`

### Event Fields

- **`id`**: Unique event identifier.
- **`event`**: Event type (`"consumptionUpdate"`).
- **`data`**: JSON string of `ConsumptionUpdate`.

---

## Contributing

Contributions are welcome!

---

## Contact

For any questions or support, please open an issue.
