#!/usr/bin/env node
import fs from "node:fs";
import http from "node:http";
import https from "node:https";
import net from "node:net";

const config = {
  listenHost: process.env.WARROOM_IOS_PROXY_HOST || "127.0.0.1",
  listenPort: Number(process.env.WARROOM_IOS_PROXY_PORT || "3443"),
  certPath: process.env.WARROOM_IOS_PROXY_CERT || "/tmp/warroom-ios-tailnet-selfsigned.crt",
  keyPath: process.env.WARROOM_IOS_PROXY_KEY || "/tmp/warroom-ios-tailnet-selfsigned.key",
  webTarget: new URL(process.env.WARROOM_IOS_WEB_TARGET || "http://127.0.0.1:3010"),
  councilTarget: new URL(process.env.WARROOM_IOS_COUNCIL_TARGET || "http://127.0.0.1:8767"),
};

function isPrivateBind(host) {
  if (host === "127.0.0.1" || host === "localhost" || host === "::1") {
    return true;
  }
  const octets = host.split(".").map((part) => Number(part));
  if (octets.length !== 4 || octets.some((part) => !Number.isInteger(part))) {
    return false;
  }
  const [a, b] = octets;
  return (
    a === 10 ||
    (a === 100 && b >= 64 && b <= 127) ||
    (a === 172 && b >= 16 && b <= 31) ||
    (a === 192 && b === 168)
  );
}

if (!isPrivateBind(config.listenHost) && process.env.WARROOM_IOS_PROXY_ALLOW_PUBLIC !== "1") {
  console.error(
    `Refusing to bind ${config.listenHost}. Use loopback, RFC1918, or Tailscale 100.64.0.0/10.`,
  );
  process.exit(2);
}

function routeFor(pathname) {
  return pathname.startsWith("/api/") || pathname === "/api" ||
    pathname.startsWith("/ws/") || pathname === "/ws"
    ? config.councilTarget
    : config.webTarget;
}

function filteredHeaders(headers, target) {
  const hopByHop = new Set([
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
  ]);
  const next = {};
  for (const [key, value] of Object.entries(headers)) {
    if (!hopByHop.has(key.toLowerCase())) {
      next[key] = value;
    }
  }
  next.host = target.host;
  next["x-forwarded-proto"] = "https";
  next["x-forwarded-host"] = headers.host || `${config.listenHost}:${config.listenPort}`;
  return next;
}

const server = https.createServer(
  {
    cert: fs.readFileSync(config.certPath),
    key: fs.readFileSync(config.keyPath),
  },
  (req, res) => {
    const target = routeFor(req.url || "/");
    console.log(`${new Date().toISOString()} HTTP ${req.method} ${req.url} -> ${target.host}`);
    const upstream = http.request(
      {
        hostname: target.hostname,
        port: target.port || "80",
        method: req.method,
        path: req.url,
        headers: filteredHeaders(req.headers, target),
      },
      (upstreamRes) => {
        res.writeHead(upstreamRes.statusCode || 502, upstreamRes.headers);
        upstreamRes.pipe(res);
      },
    );
    upstream.on("error", (error) => {
      console.error(`${new Date().toISOString()} HTTP proxy error ${error.message}`);
      if (!res.headersSent) {
        res.writeHead(502, { "content-type": "text/plain; charset=utf-8" });
      }
      res.end("Bad gateway");
    });
    req.pipe(upstream);
  },
);

server.on("upgrade", (req, socket, head) => {
  const target = routeFor(req.url || "/");
  if (target !== config.councilTarget) {
    socket.end("HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n");
    return;
  }

  console.log(`${new Date().toISOString()} WS ${req.url} -> ${target.host}`);
  const upstream = net.connect(Number(target.port || "80"), target.hostname, () => {
    upstream.write(`${req.method} ${req.url} HTTP/${req.httpVersion}\r\n`);
    for (const [name, value] of Object.entries(req.headers)) {
      const rendered = Array.isArray(value) ? value.join(", ") : value;
      upstream.write(`${name}: ${rendered}\r\n`);
    }
    upstream.write("\r\n");
    if (head.length > 0) {
      upstream.write(head);
    }
    socket.pipe(upstream).pipe(socket);
  });

  upstream.on("error", (error) => {
    console.error(`${new Date().toISOString()} WS proxy error ${error.message}`);
    socket.destroy();
  });
});

server.listen(config.listenPort, config.listenHost, () => {
  console.log(
    `War Room iOS smoke proxy listening on https://${config.listenHost}:${config.listenPort}`,
  );
  console.log(`  web -> ${config.webTarget.href}`);
  console.log(`  api/ws -> ${config.councilTarget.href}`);
});
