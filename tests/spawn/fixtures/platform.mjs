// Platform-specific commands used by spawn tests and fixtures.
// Each returns [command, args] ready for Aike.spawn/spawnSync.
export function sleepCmd(seconds) {
  return process.platform === "win32"
    ? ["powershell", ["-NoProfile", "-Command", `Start-Sleep -Seconds ${seconds}`]]
    : ["sleep", [`${seconds}`]];
}

export function echoCmd(text) {
  return process.platform === "win32"
    ? ["cmd", ["/c", `echo ${text}`]]
    : ["sh", ["-c", `echo ${text}`]];
}