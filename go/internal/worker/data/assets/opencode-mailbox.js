import { createHash } from "node:crypto";
import { spawn } from "node:child_process";

// Filled from the running worker, including on remote and container targets.
const worker = __MJ_WORKER__;
const socket = __MJ_SOCKET__;

function request(method, params) {
  return new Promise((resolve, reject) => {
    // The shared Rust transport owns IPC framing, deadlines and long socket paths.
    const child = spawn(worker, ["worker", "mailbox-request", "--socket", socket],
      { stdio: ["pipe", "pipe", "pipe"] });
    const output = [];
    child.stdout.on("data", chunk => output.push(chunk));
    child.stderr.on("data", chunk => process.stderr.write(chunk));
    child.on("error", reject);
    child.stdin.on("error", reject);
    child.on("close", code => {
      if (code !== 0) {
        reject(new Error(`Mjolnir mailbox request exited with status ${code}`));
        return;
      }
      try {
        resolve(JSON.parse(Buffer.concat(output).toString("utf8")));
      } catch (error) {
        reject(error);
      }
    });
    child.stdin.end(JSON.stringify({ method, params }));
  });
}

export default {
  id: "mjolnir.mailbox",
  async setup({ session, tool }) {
    const registration = await tool.hook("execute.after", async (event) => {
      // Native children share this plugin process, but not the lead's mailbox.
      if ((await session.get({ sessionID: event.sessionID })).parentID) return;
      const response = await request("drain_mailbox", { hook_event: "PostToolBatch" });
      if (response.type !== "mailbox_drained") throw new Error("Invalid Mjolnir mailbox lease");
      const lease = response.data;
      if (!lease.lease_id) return;
      for (const context of lease.contexts) {
        // Each producer event retains its ID even if an unacknowledged lease
        // expires and is combined with newer events. OpenCode deduplicates
        // this ID in its durable inbox and in already-promoted messages.
        const id = "msg_" + createHash("sha256")
          .update(JSON.stringify([event.sessionID, context.key])).digest("hex");
        await session.synthetic({
          sessionID: event.sessionID, id, text: context.text,
          delivery: "steer", resume: false,
        });
      }
      // OpenCode now owns durable copies. A crash before this point leaves
      // the lease unacknowledged and safely replayable with the same IDs.
      const ack = await request("ack_mailbox", { lease_id: lease.lease_id });
      if (ack.type !== "mailbox_acknowledged" || !ack.data.acknowledged)
        throw new Error("Mjolnir mailbox lease was not acknowledged");
    });
    return () => registration.dispose();
  },
};
