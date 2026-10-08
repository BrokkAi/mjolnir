export const effortQuestions = {
  effort: {
    type: "choice",
    instructions: "Which reasoning-effort level does this assignment need? Assess the assignment's difficulty and scope: how broadly the code must be read, how many independent decisions it requires, how much verification is needed, and how much risk there is of subtle bugs. Judge only the described assignment, not who performs it or which model is named.",
    criteria: {
      medium: {
        what: "A narrow task with little code to read, few independent decisions, routine verification, and low risk of subtle bugs.",
      },
      high: {
        what: "A moderately scoped task requiring a meaningful amount of code reading, several related decisions, deliberate verification, or care around some subtle failure modes.",
      },
      xhigh: {
        what: "A broad or demanding task involving substantial code reading, many interdependent decisions, thorough verification, or significant risk of subtle bugs.",
      },
      max: {
        what: "An exceptionally broad, ambiguous, or delicate task requiring extensive code reading, many independent high-consequence decisions, rigorous verification, and careful control of serious subtle-bug risks.",
      },
    },
  },
} as const;

export interface EffortRequest {
  task_name: string;
  instructions: string;
  model: string;
  instructions_truncated: boolean;
}

const encoder = new TextEncoder();
const choices = ["medium", "high", "xhigh", "max"] as const;

function object(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function nonemptyText(value: unknown, maxBytes?: number): value is string {
  return typeof value === "string" && value.trim().length > 0
    && (maxBytes === undefined || encoder.encode(value).byteLength <= maxBytes);
}

export function effortRequest(value: unknown): value is EffortRequest {
  return object(value) && Object.keys(value).length === 4
    && ["task_name", "instructions", "model", "instructions_truncated"].every(key => key in value)
    && nonemptyText(value.task_name, 256)
    && nonemptyText(value.instructions)
    && nonemptyText(value.model, 256)
    && typeof value.instructions_truncated === "boolean";
}

function probability(value: unknown): value is number {
  return typeof value === "number" && Number.isFinite(value) && value >= 0 && value <= 1;
}

export function effortAnswers(value: unknown): Record<string, unknown> | undefined {
  if (!object(value) || !object(value.answers) || Object.keys(value.answers).length !== 1) return;
  const answer = value.answers.effort;
  if (!object(answer) || answer.type !== "choice" || !choices.includes(answer.choice as typeof choices[number])
    || !probability(answer.confidence) || !object(answer.probabilities)) return;
  const scores = answer.probabilities;
  if (Object.keys(scores).length !== choices.length || !choices.every(choice => probability(scores[choice]))) return;
  const values = Object.values(scores) as number[];
  const winner = scores[answer.choice as typeof choices[number]] as number;
  if (Math.abs(values.reduce((total, score) => total + score, 0) - 1) > 0.005 * choices.length + 1e-12
    || values.some(score => score > winner + 1e-12)) return;

  return {
    effort: {
      choice: answer.choice,
      confidence: answer.confidence,
      probabilities: scores,
    },
  };
}
