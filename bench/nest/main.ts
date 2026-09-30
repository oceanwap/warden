// Minimal NestJS app for the Phase 4 benchmark stand-in (the real TravelerWE
// API should be benchmarked the same way). Same endpoints as bench/app.
// Under Bun it needs Warden's shim to share the port (node:http ignores reusePort).
import "reflect-metadata";
import { Controller, Get, Module } from "@nestjs/common";
import { NestFactory } from "@nestjs/core";
import { threadId } from "node:worker_threads";

const who = `${process.pid}:${threadId}`;
const payload = { message: "Hello, World!", items: Array.from({ length: 20 }, (_, i) => ({ id: i, name: `item-${i}`, tags: ["a", "b", "c"] })) };
function fib(n: number): number {
  return n < 2 ? n : fib(n - 1) + fib(n - 2);
}

@Controller()
class BenchController {
  @Get("plaintext") plaintext() { return "Hello, World!"; }
  @Get("json") json() { return payload; }
  @Get("cpu") cpu() { return String(fib(27)); }
  @Get("health") health() { return "ok"; }
  @Get("whoami") whoami() { return who; }
  @Get("crash") crash() { setTimeout(() => process.exit(1), 10); return "bye"; }
}

@Module({ controllers: [BenchController] })
class AppModule {}

// A bootstrap() function rather than top-level await: PM2's Bun container
// require()s the entry, which rejects modules with top-level await.
async function bootstrap() {
  const app = await NestFactory.create(AppModule, { logger: ["error", "warn"] });
  app.enableShutdownHooks();
  await app.listen(Number(process.env.PORT ?? 3000), "127.0.0.1");
  console.log(`nest bench listening on :${process.env.PORT} (${who})`);
}
bootstrap();
