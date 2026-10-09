// Imported before `@daytona/sdk`, so it runs first. The adapter's settings are the Daytona
// client's only source: the SDK otherwise reads DAYTONA_* from the process environment and
// from .env files in the folder it is loaded in, so neither reaches it. Standard output
// carries the protocol alone: anything the SDK prints goes to the log.

for (const name of Object.keys(process.env)) {
  if (name.startsWith("DAYTONA_")) delete process.env[name];
}
process.chdir("/");
console.log = console.info = console.debug = console.error;
