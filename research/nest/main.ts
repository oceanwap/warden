import "reflect-metadata";
import { Controller, Get, Injectable, Module } from "@nestjs/common";
import { NestFactory } from "@nestjs/core";
import { threadId } from "node:worker_threads";
@Injectable() class AppService { who() { return `${process.pid}/t${threadId}`; } }
@Controller() class AppController {
  constructor(private readonly svc: AppService) {}
  @Get() root() { return this.svc.who(); }
  @Get("health") health() { return { ok: true }; }
}
@Module({ controllers: [AppController], providers: [AppService] }) class AppModule {}
const app = await NestFactory.create(AppModule, { logger: ["error", "warn"] });
app.enableShutdownHooks();
await app.listen(Number(process.env.PORT ?? 3400));
console.log(`nest listening pid=${process.pid} t=${threadId}`);
