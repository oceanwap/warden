process.on("SIGTERM", () => { console.log("worker got SIGTERM"); });
postMessage("ready");
setInterval(() => {}, 1000);
