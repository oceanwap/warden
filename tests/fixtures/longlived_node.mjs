// Node test app with long-lived connections, built-ins only (the WebSocket
// handshake and framing are written by hand, as the `ws` library does it).
// Endpoints:
//   /whoami        "<pid>"
//   /slow?ms=N     answers "<pid>" after N ms (a normal request in flight)
//   /sse           text/event-stream: "id: n\ndata: <pid> n\n\n" every 100 ms, forever
//   /ws            WebSocket: sends "<pid>" on open, echoes text messages
import crypto from "node:crypto";
import http from "node:http";

const pid = String(process.pid);

const server = http.createServer((req, res) => {
  const url = new URL(req.url, "http://x");
  if (url.pathname === "/slow") {
    setTimeout(() => res.end(pid), Number(url.searchParams.get("ms") || 500));
    return;
  }
  if (url.pathname === "/sse") {
    res.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-store" });
    let n = 0;
    const tick = () => res.write(`id: ${n}\ndata: ${pid} ${n++}\n\n`);
    tick();
    const t = setInterval(tick, 100);
    res.on("close", () => clearInterval(t));
    return;
  }
  res.end(pid);
});

// RFC 6455 server side: unmasked frames out, masked frames in.
function frame(opcode, payload) {
  const len = payload.length;
  const head = len < 126 ? Buffer.from([0x80 | opcode, len]) : Buffer.from([0x80 | opcode, 126, len >> 8, len & 255]);
  return Buffer.concat([head, payload]);
}

server.on("upgrade", (req, socket) => {
  if ((req.headers.upgrade || "").toLowerCase() !== "websocket") return socket.destroy();
  const accept = crypto
    .createHash("sha1")
    .update(req.headers["sec-websocket-key"] + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11")
    .digest("base64");
  socket.write(
    `HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: ${accept}\r\n\r\n`,
  );
  socket.write(frame(1, Buffer.from(pid)));
  let buf = Buffer.alloc(0);
  let closing = false;
  socket.on("error", () => {});
  socket.on("end", () => socket.end()); // the server's sockets allow half-open
  socket.on("data", (d) => {
    buf = Buffer.concat([buf, d]);
    while (buf.length >= 6) {
      const op = buf[0] & 15;
      let len = buf[1] & 127;
      let off = 2;
      if (len === 126) {
        if (buf.length < 4) return;
        len = buf.readUInt16BE(2);
        off = 4;
      }
      if (buf.length < off + 4 + len) return;
      const mask = buf.subarray(off, off + 4);
      const data = Buffer.from(buf.subarray(off + 4, off + 4 + len));
      for (let i = 0; i < len; i++) data[i] ^= mask[i & 3];
      buf = buf.subarray(off + 4 + len);
      if (op === 1) socket.write(frame(1, data));
      else if (op === 9) socket.write(frame(10, data));
      else if (op === 8) {
        // The peer closed (or answered our close): answer once, then end.
        if (!closing && socket.writable) socket.write(frame(8, data.subarray(0, 2)));
        closing = true;
        socket.end();
      }
    }
  });
});

server.listen(Number(process.env.PORT), "127.0.0.1");
