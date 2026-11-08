import { spawn } from "node:child_process";

// Filled from the running worker, including on remote and container targets.
const worker = __MJ_WORKER__;
const socket = __MJ_SOCKET__;

function drain() {
  return new Promise((resolve, reject) => {
    const child = spawn(worker, [
      "worker", "mailbox-hook", "--socket", socket, "--event", "PostToolUse",
    ], { stdio: ["pipe", "pipe", "pipe"] });
    const output = [];
    child.stdout.on("data", (chunk) => output.push(chunk));
    child.stderr.on("data", (chunk) => process.stderr.write(chunk));
    child.on("error", reject);
    child.stdin.on("error", reject);
    child.on("close", (code) => {
      if (code !== 0) {
        reject(new Error(`Mjolnir mailbox hook exited with status ${code}`));
        return;
      }
      try {
        resolve(JSON.parse(Buffer.concat(output).toString("utf8"))
          .hookSpecificOutput?.additionalContext);
      } catch (error) {
        reject(error);
      }
    });
    child.stdin.end("{}");
  });
}

export default {
  id: "mjolnir.mailbox",
  async setup({ session, tool }) {
    const registration = await tool.hook("execute.after", async (event) => {
      // Native OpenCode children share this plugin process, but not the
      // lead's mailbox. A child must never consume its parent's messages.
      if ((await session.get({ sessionID: event.sessionID })).parentID) return;
      const text = await drain();
      if (!text) return;
      if (event.status === "completed") {
        const content = event.result.content;
        event.result = {
          ...event.result,
          content: [
            ...(typeof content === "string" ? [{ type: "text", text: content }] : content ?? []),
            { type: "text", text },
          ],
        };
      } else {
        event.error.message += `\n\n${text}`;
      }
    });
    return () => registration.dispose();
  },
};
